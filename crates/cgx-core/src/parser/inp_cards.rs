//! Keyword-card scanner for CalculiX `.inp` decks.
//!
//! The scanner walks a root deck (following `*INCLUDE` in place) and records one
//! [`InpCard`] per keyword card, without loading data lines into memory. Data lines
//! can be fetched lazily with [`read_card_data`] using the recorded byte offset.

use crate::parser::inp::InpInclude;
use crate::{FrdError, Result};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// One keyword card (`*KEYWORD, PARAM=VALUE, ...`) and where its data lines live.
#[derive(Debug, Clone, PartialEq)]
pub struct InpCard {
    /// Upper-case keyword without the leading `*`, inner whitespace collapsed.
    pub keyword: String,
    /// Parameters in source order: keys upper-cased, values verbatim (trimmed).
    pub params: Vec<(String, Option<String>)>,
    pub file: PathBuf,
    /// 1-based line of the first header line.
    pub line: usize,
    /// 0 = model level, n = inside the n-th `*STEP`.
    pub step: u32,
    /// Number of data lines (comment and blank lines are not counted).
    pub data_lines: usize,
    /// Byte offset in `file` of the first data line (or of the line following the
    /// header when the card has no data lines).
    pub data_offset: u64,
}

/// Result of scanning a deck and all its includes.
#[derive(Debug, Clone)]
pub struct InpScan {
    pub cards: Vec<InpCard>,
    pub includes: Vec<InpInclude>,
    pub total_lines: usize,
}

/// Safety limits for [`scan_cards`].
#[derive(Debug, Clone, PartialEq)]
pub struct ScanLimits {
    pub max_files: usize,
    pub max_depth: usize,
    pub max_total_bytes: u64,
}

impl Default for ScanLimits {
    fn default() -> Self {
        ScanLimits {
            max_files: 1000,
            max_depth: 16,
            max_total_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

/// Per-step summary produced by [`InpScan::steps`].
#[derive(Debug, Clone, PartialEq)]
pub struct InpStepSummary {
    pub number: u32,
    pub file: PathBuf,
    pub line: usize,
    /// Parameters of the `*STEP` card, re-rendered as `KEY=VALUE, KEY, ...`.
    pub params: String,
    pub procedure: Option<String>,
    /// Keyword -> count for every card inside the step (including `STEP`/`END STEP`).
    pub cards: BTreeMap<String, usize>,
}

/// A card (other than `*INCLUDE`) that references an external file via `INPUT=` or `FILE=`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalRef {
    pub card: String,
    pub param: String,
    pub target: String,
    pub resolved_path: PathBuf,
    pub file: PathBuf,
    pub line: usize,
    pub exists: bool,
}

const PROCEDURES: &[&str] = &[
    "STATIC",
    "FREQUENCY",
    "BUCKLE",
    "DYNAMIC",
    "MODAL DYNAMIC",
    "STEADY STATE DYNAMICS",
    "COMPLEX FREQUENCY",
    "HEAT TRANSFER",
    "COUPLED TEMPERATURE-DISPLACEMENT",
    "UNCOUPLED TEMPERATURE-DISPLACEMENT",
    "VISCO",
    "GREEN",
    "SENSITIVITY",
    "ELECTROMAGNETICS",
    "SUBSTRUCTURE GENERATE",
    "CFD",
];

/// Normalize a keyword: strip an optional `*`, collapse whitespace, upper-case.
fn normalize_keyword(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix('*').unwrap_or(s);
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase()
}

fn is_comment(line: &str) -> bool {
    line.starts_with("**")
}

fn is_header(line: &str) -> bool {
    line.starts_with('*') && !is_comment(line)
}

fn strip_quotes(s: &str) -> &str {
    s.trim_matches(|c| c == '"' || c == '\'').trim()
}

/// Parse the (already joined) header text into keyword and parameters.
fn parse_header(text: &str) -> (String, Vec<(String, Option<String>)>) {
    let body = text.trim_start().strip_prefix('*').unwrap_or(text);
    let mut parts = body.split(',');
    let keyword = normalize_keyword(parts.next().unwrap_or(""));
    let mut params = Vec::new();
    for part in parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('=') {
            Some((k, v)) => params.push((k.trim().to_uppercase(), Some(v.trim().to_string()))),
            None => params.push((part.to_uppercase(), None)),
        }
    }
    (keyword, params)
}

fn param_value<'a>(card: &'a InpCard, key: &str) -> Option<&'a str> {
    card.params
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.as_deref())
}

/// Read one line (without `\n` / `\r\n`). Returns `(start_offset, text)` or `None` at EOF.
fn next_line<R: BufRead>(
    reader: &mut R,
    pos: &mut u64,
    buf: &mut Vec<u8>,
) -> Result<Option<(u64, String)>> {
    buf.clear();
    let n = reader.read_until(b'\n', buf)?;
    if n == 0 {
        return Ok(None);
    }
    let start = *pos;
    *pos += n as u64;
    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
        buf.pop();
    }
    Ok(Some((start, String::from_utf8_lossy(buf).into_owned())))
}

struct Scanner<'a> {
    limits: &'a ScanLimits,
    cards: Vec<InpCard>,
    includes: Vec<InpInclude>,
    total_lines: usize,
    files_opened: usize,
    total_bytes: u64,
    cur_step: u32,
    step_counter: u32,
    stack: Vec<PathBuf>,
}

impl<'a> Scanner<'a> {
    fn scan_file(&mut self, path: &Path, depth: usize) -> Result<()> {
        let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if self.stack.contains(&canonical) {
            return Err(FrdError::InvalidFormat(format!(
                "Circular *INCLUDE detected: {}",
                path.display()
            )));
        }
        if depth > self.limits.max_depth {
            return Err(FrdError::InvalidFormat(format!(
                "Exceeded maximum *INCLUDE depth ({}) at {}",
                self.limits.max_depth,
                path.display()
            )));
        }
        if self.files_opened + 1 > self.limits.max_files {
            return Err(FrdError::InvalidFormat(format!(
                "Exceeded maximum number of files ({}) at {}",
                self.limits.max_files,
                path.display()
            )));
        }
        let size = fs::metadata(path)?.len();
        if self.total_bytes.saturating_add(size) > self.limits.max_total_bytes {
            return Err(FrdError::InvalidFormat(format!(
                "Exceeded maximum total bytes ({}) at {}",
                self.limits.max_total_bytes,
                path.display()
            )));
        }
        let file = File::open(path)?;
        self.files_opened += 1;
        self.total_bytes += size;
        self.stack.push(canonical);

        let result = self.scan_reader(BufReader::new(file), path, depth);
        self.stack.pop();
        result
    }

    fn scan_reader<R: BufRead>(&mut self, mut reader: R, path: &Path, depth: usize) -> Result<()> {
        let mut pos: u64 = 0;
        let mut buf: Vec<u8> = Vec::new();
        let mut line_no: usize = 0;
        // Index (into self.cards) of the card owning the data lines being read in this file.
        let mut current: Option<usize> = None;

        while let Some((start, line)) = next_line(&mut reader, &mut pos, &mut buf)? {
            line_no += 1;
            self.total_lines += 1;
            let trimmed = line.trim();
            if trimmed.is_empty() || is_comment(trimmed) {
                continue;
            }
            if !is_header(trimmed) {
                // Data line for the current card of this file.
                if let Some(idx) = current {
                    let card = &mut self.cards[idx];
                    if card.data_lines == 0 {
                        card.data_offset = start;
                    }
                    card.data_lines += 1;
                }
                continue;
            }

            // Header, possibly continued while it ends with a comma.
            let header_line = line_no;
            let mut text = trimmed.to_string();
            while text.ends_with(',') {
                match next_line(&mut reader, &mut pos, &mut buf)? {
                    Some((_, cont)) => {
                        line_no += 1;
                        self.total_lines += 1;
                        let ct = cont.trim();
                        if ct.is_empty() || is_comment(ct) {
                            continue;
                        }
                        text.push(' ');
                        text.push_str(ct);
                    }
                    None => break,
                }
            }

            let (keyword, params) = parse_header(&text);
            let mut card = InpCard {
                keyword,
                params,
                file: path.to_path_buf(),
                line: header_line,
                step: self.cur_step,
                data_lines: 0,
                data_offset: pos,
            };
            match card.keyword.as_str() {
                "STEP" => {
                    self.step_counter += 1;
                    self.cur_step = self.step_counter;
                    card.step = self.cur_step;
                }
                "END STEP" => {
                    card.step = self.cur_step;
                    self.cur_step = 0;
                }
                _ => {}
            }
            let include_target = if card.keyword == "INCLUDE" {
                param_value(&card, "INPUT").map(|v| strip_quotes(v).to_string())
            } else {
                None
            };
            self.cards.push(card);
            current = Some(self.cards.len() - 1);

            if let Some(target) = include_target.filter(|t| !t.is_empty()) {
                let parent = path.parent().unwrap_or_else(|| Path::new("."));
                let resolved = parent.join(&target);
                let exists = resolved.is_file();
                let size = if exists {
                    fs::metadata(&resolved).map(|m| m.len()).unwrap_or(0)
                } else {
                    0
                };
                self.includes.push(InpInclude {
                    raw_target: target,
                    resolved_path: resolved.clone(),
                    exists,
                    file_size_bytes: size,
                    line_number: header_line,
                    included_from: path.to_path_buf(),
                });
                if exists {
                    self.scan_file(&resolved, depth + 1)?;
                }
            }
        }
        Ok(())
    }
}

/// Scan `root` and every file it includes, returning all keyword cards in encounter order.
///
/// Include paths are resolved relative to the including file's directory. A missing
/// include is recorded with `exists = false` and scanning continues. Include cycles and
/// exceeded [`ScanLimits`] yield `FrdError::InvalidFormat`.
pub fn scan_cards(root: &Path, limits: &ScanLimits) -> Result<InpScan> {
    let mut scanner = Scanner {
        limits,
        cards: Vec::new(),
        includes: Vec::new(),
        total_lines: 0,
        files_opened: 0,
        total_bytes: 0,
        cur_step: 0,
        step_counter: 0,
        stack: Vec::new(),
    };
    scanner.scan_file(root, 0)?;
    Ok(InpScan {
        cards: scanner.cards,
        includes: scanner.includes,
        total_lines: scanner.total_lines,
    })
}

/// Read up to `max_lines` data lines of `card` (comments and blank lines skipped, line
/// endings stripped). Stops at the next keyword card.
pub fn read_card_data(card: &InpCard, max_lines: usize) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if max_lines == 0 || card.data_lines == 0 {
        return Ok(out);
    }
    let mut file = File::open(&card.file)?;
    file.seek(SeekFrom::Start(card.data_offset))?;
    let mut reader = BufReader::new(file);
    let mut pos = card.data_offset;
    let mut buf = Vec::new();
    let limit = max_lines.min(card.data_lines);
    while out.len() < limit {
        let Some((_, line)) = next_line(&mut reader, &mut pos, &mut buf)? else {
            break;
        };
        let t = line.trim();
        if t.is_empty() || is_comment(t) {
            continue;
        }
        if is_header(t) {
            break;
        }
        out.push(line);
    }
    Ok(out)
}

impl InpScan {
    /// Keyword -> count for model-level cards (`step == 0`).
    pub fn census(&self) -> BTreeMap<String, usize> {
        let mut map = BTreeMap::new();
        for c in self.cards.iter().filter(|c| c.step == 0) {
            *map.entry(c.keyword.clone()).or_insert(0) += 1;
        }
        map
    }

    /// One summary per `*STEP` card, in order.
    pub fn steps(&self) -> Vec<InpStepSummary> {
        let mut out = Vec::new();
        for card in self.cards.iter().filter(|c| c.keyword == "STEP") {
            let params = card
                .params
                .iter()
                .map(|(k, v)| match v {
                    Some(v) => format!("{}={}", k, v),
                    None => k.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let mut cards = BTreeMap::new();
            let mut procedure = None;
            for c in self.cards.iter().filter(|c| c.step == card.step) {
                *cards.entry(c.keyword.clone()).or_insert(0) += 1;
                if procedure.is_none() && PROCEDURES.contains(&c.keyword.as_str()) {
                    procedure = Some(c.keyword.clone());
                }
            }
            out.push(InpStepSummary {
                number: card.step,
                file: card.file.clone(),
                line: card.line,
                params,
                procedure,
                cards,
            });
        }
        out
    }

    /// Cards other than `*INCLUDE` with an `INPUT` or `FILE` parameter.
    pub fn external_refs(&self) -> Vec<ExternalRef> {
        let mut out = Vec::new();
        for card in self.cards.iter().filter(|c| c.keyword != "INCLUDE") {
            for key in ["INPUT", "FILE"] {
                if let Some(value) = param_value(card, key) {
                    let target = strip_quotes(value).to_string();
                    let parent = card.file.parent().unwrap_or_else(|| Path::new("."));
                    let resolved = parent.join(&target);
                    out.push(ExternalRef {
                        card: card.keyword.clone(),
                        param: key.to_string(),
                        target,
                        exists: resolved.exists(),
                        resolved_path: resolved,
                        file: card.file.clone(),
                        line: card.line,
                    });
                }
            }
        }
        out
    }

    /// Applicable structural flags, in a fixed order.
    pub fn flags(&self) -> Vec<&'static str> {
        let any = |kw: &str| self.cards.iter().any(|c| c.keyword == kw);
        // A STEP parameter counts unless explicitly set to NO.
        let step_param = |p: &str| {
            self.cards.iter().any(|c| {
                c.keyword == "STEP"
                    && c.params.iter().any(|(k, v)| {
                        k == p && !v.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("NO"))
                    })
            })
        };
        let mut f = Vec::new();
        if any("CYCLIC SYMMETRY MODEL") {
            f.push("CYCLIC_SYMMETRY");
        }
        if any("CONTACT PAIR") {
            f.push("CONTACT");
        }
        if step_param("NLGEOM") {
            f.push("NLGEOM");
        }
        if step_param("PERTURBATION") {
            f.push("PERTURBATION");
        }
        if self.includes.iter().any(|i| !i.exists) {
            f.push("MISSING_INCLUDES");
        }
        if !any("STEP") {
            f.push("NO_STEPS");
        }
        if !any("NODE") || !any("ELEMENT") {
            f.push("NO_MESH");
        }
        f
    }

    /// Find cards by keyword (case-insensitive, leading `*` optional, whitespace collapsed).
    /// `step`: `Some(0)` = model level, `Some(n)` = inside step n, `None` = anywhere.
    pub fn find(&self, keyword: &str, step: Option<u32>) -> Vec<&InpCard> {
        let kw = normalize_keyword(keyword);
        self.cards
            .iter()
            .filter(|c| c.keyword == kw && step.map_or(true, |s| c.step == s))
            .collect()
    }
}
