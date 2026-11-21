//! Located, reviewable text edits of CalculiX `.inp` decks.
//!
//! Cards are addressed through [`scan_cards`]; every address is resolved against the
//! ORIGINAL deck before any edit is applied. Edits are line based: everything that is not
//! edited is copied byte for byte, line endings (`\n` or `\r\n`) are kept per file, and the
//! result is always written to NEW files (see [`plan_edit`] and [`EditPlan::write`]).

use crate::parser::inp_cards::{scan_cards, InpCard, InpScan, ScanLimits};
use crate::FrdError;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// How an operation finds its card.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CardAddress {
    /// Keyword, case-insensitive, leading `*` optional.
    pub keyword: String,
    /// `Some(0)` = model level, `Some(n)` = inside step n, `None` = anywhere.
    pub step: Option<u32>,
    /// 0-based index among the matches; required when several cards match.
    pub index: Option<usize>,
    /// Only cards having this parameter with this value (case-insensitive).
    pub param: Option<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    /// Set a parameter value; `None` removes it, `Some("")` makes it a bare flag.
    SetParam { card: CardAddress, param: String, value: Option<String> },
    /// Replace all data lines of a card.
    ReplaceData { card: CardAddress, lines: Vec<String> },
    /// Insert text (starting with a `*` line) after a card and its data lines.
    InsertAfter { card: CardAddress, text: String },
    /// Delete a card, its header continuation lines and its data lines.
    DeleteCard { card: CardAddress },
    /// Append text containing `*STEP` ... `*END STEP` after the last `*END STEP` of the root deck.
    AppendStep { text: String },
}

/// A card as shown in candidate lists.
#[derive(Debug, Clone, PartialEq)]
pub struct CardRef {
    pub file: PathBuf,
    pub line: usize,
    /// First header line, trimmed.
    pub header: String,
}

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// No card matches; `valid` lists the most frequent keywords (or the candidates).
    #[error("{message}")]
    UnknownCard { message: String, valid: Vec<String> },
    #[error("{message}")]
    AmbiguousCard { message: String, candidates: Vec<CardRef> },
    #[error("{message}")]
    Invalid { message: String, valid: Vec<String> },
    #[error("{0}")]
    Conflict(String),
    #[error("{} already exists", .0.display())]
    OutputExists(PathBuf),
    #[error("deck cannot be scanned: {0}")]
    Deck(FrdError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

type R<T> = std::result::Result<T, EditError>;

fn invalid<T>(message: impl Into<String>) -> R<T> {
    Err(EditError::Invalid { message: message.into(), valid: Vec::new() })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileRole {
    Root,
    Include,
}

/// Where an operation landed (original coordinates).
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedOp {
    /// 0-based index in the request.
    pub op: usize,
    pub kind: &'static str,
    pub file: PathBuf,
    /// 1-based line of the card header (or insertion point for `append_step`).
    pub line: usize,
    pub header: String,
}

/// Replace original lines `start..end` (0-based, end exclusive) with `repl`.
#[derive(Debug, Clone)]
struct LineEdit {
    start: usize,
    end: usize,
    repl: Vec<Vec<u8>>,
    ord: usize,
}

/// One file of the result: the original lines, the edits and the new location.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub role: FileRole,
    /// The original file (never written).
    pub original: PathBuf,
    /// The file that will be created.
    pub path: PathBuf,
    lines: Vec<Vec<u8>>,
    eol: Vec<u8>,
    edits: Vec<LineEdit>,
    link_edits: Vec<LineEdit>,
    new_bytes: Vec<u8>,
}

impl PlannedFile {
    pub fn original_bytes(&self) -> Vec<u8> {
        self.lines.concat()
    }

    pub fn new_bytes(&self) -> &[u8] {
        &self.new_bytes
    }

    /// Unified diff of this file against its original (`context` lines around each change).
    pub fn unified_diff(&self, old_name: &str, new_name: &str, context: usize) -> String {
        let mut edits: Vec<&LineEdit> = self.edits.iter().chain(&self.link_edits).collect();
        edits.sort_by_key(|e| (e.start, e.end, e.ord));
        // Trim identical leading and trailing lines of each edit.
        struct Change<'a> {
            start: usize,
            old_len: usize,
            new: &'a [Vec<u8>],
        }
        let mut changes: Vec<Change> = Vec::new();
        for e in edits {
            let old = &self.lines[e.start..e.end];
            let mut a = 0;
            while a < old.len() && a < e.repl.len() && old[a] == e.repl[a] {
                a += 1;
            }
            let mut b = 0;
            while b < old.len() - a && b < e.repl.len() - a && old[old.len() - 1 - b] == e.repl[e.repl.len() - 1 - b] {
                b += 1;
            }
            let (old_len, new) = (old.len() - a - b, &e.repl[a..e.repl.len() - b]);
            if old_len == 0 && new.is_empty() {
                continue;
            }
            changes.push(Change { start: e.start + a, old_len, new });
        }
        if changes.is_empty() {
            return String::new();
        }
        let n = self.lines.len();
        let show = |prefix: char, line: &[u8], out: &mut String| {
            out.push(prefix);
            out.push_str(&String::from_utf8_lossy(content(line)));
            out.push('\n');
            if !line.ends_with(b"\n") {
                out.push_str("\\ No newline at end of file\n");
            }
        };
        let mut out = format!("--- {old_name}\n+++ {new_name}\n");
        let mut delta: isize = 0;
        let mut i = 0;
        while i < changes.len() {
            // Group changes whose gap is at most 2 * context.
            let mut j = i + 1;
            while j < changes.len() && changes[j].start - (changes[j - 1].start + changes[j - 1].old_len) <= 2 * context {
                j += 1;
            }
            let group = &changes[i..j];
            let h_start = group[0].start.saturating_sub(context);
            let last = &group[group.len() - 1];
            let h_end = (last.start + last.old_len + context).min(n);
            let mut body = String::new();
            let mut pos = h_start;
            let mut new_count = 0usize;
            let mut group_delta: isize = 0;
            for c in group {
                for l in &self.lines[pos..c.start] {
                    show(' ', l, &mut body);
                    new_count += 1;
                }
                for l in &self.lines[c.start..c.start + c.old_len] {
                    show('-', l, &mut body);
                }
                for l in c.new {
                    show('+', l, &mut body);
                    new_count += 1;
                }
                group_delta += c.new.len() as isize - c.old_len as isize;
                pos = c.start + c.old_len;
            }
            for l in &self.lines[pos..h_end] {
                show(' ', l, &mut body);
                new_count += 1;
            }
            let old_count = h_end - h_start;
            let old_s = if old_count == 0 { h_start } else { h_start + 1 };
            let new_start = h_start as isize + delta;
            let new_s = if new_count == 0 { new_start } else { new_start + 1 };
            out.push_str(&format!("@@ -{old_s},{old_count} +{new_s},{new_count} @@\n"));
            out.push_str(&body);
            delta += group_delta;
            i = j;
        }
        out
    }
}

/// An include line of a new file that points to another new file.
#[derive(Debug, Clone)]
struct Link {
    parent: usize,
    child: usize,
    card: InpCard,
}

/// The planned result of an edit request; nothing is on disk until [`EditPlan::write`].
#[derive(Debug)]
pub struct EditPlan {
    /// Root first, then edited includes and their ancestors.
    pub files: Vec<PlannedFile>,
    pub applied: Vec<AppliedOp>,
    /// Scan of the original deck.
    pub before: InpScan,
    links: Vec<Link>,
}

// ---------- line helpers ----------

fn split_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            out.push(bytes[start..=i].to_vec());
            start = i + 1;
        }
    }
    if start < bytes.len() {
        out.push(bytes[start..].to_vec());
    }
    out
}

fn content(line: &[u8]) -> &[u8] {
    let mut e = line.len();
    if e > 0 && line[e - 1] == b'\n' {
        e -= 1;
        if e > 0 && line[e - 1] == b'\r' {
            e -= 1;
        }
    }
    &line[..e]
}

fn terminator(line: &[u8]) -> &[u8] {
    &line[content(line).len()..]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Skip,
    Header,
    Data,
}

fn kind(line: &[u8]) -> Kind {
    let s = String::from_utf8_lossy(content(line));
    let t = s.trim();
    if t.is_empty() || t.starts_with("**") {
        Kind::Skip
    } else if t.starts_with('*') {
        Kind::Header
    } else {
        Kind::Data
    }
}

fn detect_eol(bytes: &[u8]) -> Vec<u8> {
    match bytes.iter().position(|b| *b == b'\n') {
        Some(i) if i > 0 && bytes[i - 1] == b'\r' => b"\r\n".to_vec(),
        _ => b"\n".to_vec(),
    }
}

fn text_lines(text: &str, eol: &[u8]) -> Vec<Vec<u8>> {
    text.lines()
        .map(|l| {
            let mut v = l.as_bytes().to_vec();
            v.extend_from_slice(eol);
            v
        })
        .collect()
}

fn keyword_of(line: &str) -> Option<String> {
    let t = line.trim();
    if !t.starts_with('*') || t.starts_with("**") {
        return None;
    }
    let kw = t[1..].split(',').next().unwrap_or("");
    Some(kw.split_whitespace().collect::<Vec<_>>().join(" ").to_uppercase())
}

fn strip_quotes(s: &str) -> &str {
    s.trim_matches(|c| c == '"' || c == '\'').trim()
}

/// Line ranges (0-based, end exclusive) of a card.
struct Extent {
    h_start: usize,
    h_end: usize,
    d_start: usize,
    d_end: usize,
}

fn extent(lines: &[Vec<u8>], header_line: usize) -> R<Extent> {
    let h_start = header_line.wrapping_sub(1);
    if h_start >= lines.len() || kind(&lines[h_start]) != Kind::Header {
        return invalid(format!("line {header_line} is no longer a keyword line: the deck changed since it was scanned"));
    }
    let mut text = String::from_utf8_lossy(content(&lines[h_start])).trim().to_string();
    let mut i = h_start;
    // Same rule as the scanner: a header ending with a comma continues on the next
    // non-blank, non-comment line.
    while text.ends_with(',') {
        let j = i + 1;
        if j >= lines.len() {
            break;
        }
        i = j;
        if kind(&lines[j]) != Kind::Skip {
            text.push(' ');
            text.push_str(String::from_utf8_lossy(content(&lines[j])).trim());
        }
    }
    let h_end = i + 1;
    let (mut first, mut last) = (None, None);
    for (k, l) in lines.iter().enumerate().skip(h_end) {
        match kind(l) {
            Kind::Skip => {}
            Kind::Header => break,
            Kind::Data => {
                first.get_or_insert(k);
                last = Some(k);
            }
        }
    }
    let (d_start, d_end) = match (first, last) {
        (Some(f), Some(l)) => (f, l + 1),
        _ => (h_end, h_end),
    };
    Ok(Extent { h_start, h_end, d_start, d_end })
}

// ---------- header editing ----------

/// Byte spans of the comma separated segments of `s`.
fn segments(s: &str) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        if ch == ',' {
            v.push((start, i));
            start = i + 1;
        }
    }
    v.push((start, s.len()));
    v
}

fn utf8_line(line: &[u8]) -> R<String> {
    String::from_utf8(content(line).to_vec()).map_err(|_| EditError::Invalid {
        message: "a header line to edit is not valid UTF-8".into(),
        valid: Vec::new(),
    })
}

/// Set (`Some`), flag (`Some("")`), add or remove (`None`) one parameter of a card header,
/// leaving all other formatting untouched.
fn edit_header(lines: &[Vec<u8>], ext: &Extent, name: &str, value: Option<&str>, ord: usize) -> R<Vec<LineEdit>> {
    struct L {
        idx: usize,
        s: String,
        term: Vec<u8>,
    }
    let name_u = name.trim().to_uppercase();
    let mut ls: Vec<L> = Vec::new();
    for (i, line) in lines.iter().enumerate().take(ext.h_end).skip(ext.h_start) {
        if kind(line) == Kind::Skip {
            continue;
        }
        ls.push(L { idx: i, s: utf8_line(line)?, term: terminator(line).to_vec() });
    }
    let mut found = None;
    'outer: for (li, l) in ls.iter().enumerate() {
        for (si, (a, b)) in segments(&l.s).into_iter().enumerate() {
            if li == 0 && si == 0 {
                continue; // the keyword itself
            }
            let t = l.s[a..b].trim();
            if t.is_empty() {
                continue;
            }
            if t.split('=').next().unwrap_or("").trim().to_uppercase() == name_u {
                found = Some((li, si, a, b));
                break 'outer;
            }
        }
    }
    let mut new: Vec<Option<String>> = ls.iter().map(|l| Some(l.s.clone())).collect();
    match (found, value) {
        (Some((li, _, a, b)), Some(v)) => {
            let seg = &ls[li].s[a..b];
            let lead = &seg[..seg.len() - seg.trim_start().len()];
            let trail = &seg[seg.trim_end().len()..];
            let key_raw = seg.trim().split('=').next().unwrap_or("").trim();
            let eq = if v.is_empty() { String::new() } else { format!("={v}") };
            new[li] = Some(format!("{}{lead}{key_raw}{eq}{trail}{}", &ls[li].s[..a], &ls[li].s[b..]));
        }
        (Some((li, si, a, b)), None) => {
            let s = &ls[li].s;
            let segs = segments(s);
            if si > 0 {
                // Remove the preceding comma and the segment.
                new[li] = Some(format!("{}{}", &s[..a - 1], &s[b..]));
            } else {
                // First segment of a continuation line: remove it and the comma after it.
                let mut end = b;
                if segs.len() > 1 {
                    end = b + 1;
                }
                let rest = s[end..].trim_start();
                let head = &s[..a];
                let joined = format!("{head}{rest}");
                new[li] = if joined.trim().is_empty() { None } else { Some(joined) };
            }
            // A header must not end with a comma: it would swallow the next line.
            if let Some(last) = new.iter_mut().rev().find(|x| x.is_some()) {
                let s = last.as_mut().unwrap();
                if let Some(t) = s.trim_end().strip_suffix(',') {
                    *s = t.trim_end().to_string();
                }
            }
        }
        (None, Some(v)) => {
            let li = ls.len() - 1;
            let t = ls[li].s.trim_end();
            let sep = if t.ends_with(',') { " " } else { ", " };
            let eq = if v.is_empty() { String::new() } else { format!("={v}") };
            new[li] = Some(format!("{t}{sep}{}{eq}", name.trim()));
        }
        (None, None) => {
            let present: Vec<String> = ls
                .iter()
                .enumerate()
                .flat_map(|(li, l)| {
                    segments(&l.s)
                        .into_iter()
                        .enumerate()
                        .filter(move |(si, _)| !(li == 0 && *si == 0))
                        .map(|(_, (a, b))| l.s[a..b].trim().split('=').next().unwrap_or("").trim().to_uppercase())
                        .filter(|k| !k.is_empty())
                        .collect::<Vec<_>>()
                })
                .collect();
            return Err(EditError::Invalid { message: format!("the card has no parameter {name_u} to remove"), valid: present });
        }
    }
    let mut out = Vec::new();
    for (l, n) in ls.iter().zip(new) {
        match n {
            Some(s) if s == l.s => {}
            Some(s) => {
                let mut b = s.into_bytes();
                b.extend_from_slice(&l.term);
                out.push(LineEdit { start: l.idx, end: l.idx + 1, repl: vec![b], ord });
            }
            None => out.push(LineEdit { start: l.idx, end: l.idx + 1, repl: Vec::new(), ord }),
        }
    }
    Ok(out)
}

fn validate_param(param: &str, value: Option<&str>) -> R<()> {
    let p = param.trim();
    if p.is_empty() || p.contains([',', '=', '\n', '\r', '*']) {
        return invalid(format!("invalid parameter name {param:?}"));
    }
    if let Some(v) = value {
        if v.contains([',', '\n', '\r']) {
            return invalid(format!("invalid parameter value {v:?}: commas and line breaks are not allowed"));
        }
    }
    Ok(())
}

// ---------- applying edits ----------

fn apply(lines: &[Vec<u8>], eol: &[u8], edits: &[&LineEdit]) -> R<Vec<u8>> {
    let mut sorted: Vec<&LineEdit> = edits.to_vec();
    sorted.sort_by_key(|e| (e.start, e.end, e.ord));
    let mut out: Vec<u8> = Vec::with_capacity(lines.iter().map(|l| l.len()).sum());
    let push = |out: &mut Vec<u8>, line: &[u8]| {
        if !out.is_empty() && !out.ends_with(b"\n") {
            out.extend_from_slice(eol); // the previous line was the unterminated last line
        }
        out.extend_from_slice(line);
    };
    let mut cursor = 0;
    for e in sorted {
        if e.start < cursor {
            return invalid("two edits overlap");
        }
        for l in &lines[cursor..e.start] {
            push(&mut out, l);
        }
        let keep_open = e.end == lines.len() && e.start < e.end && !lines[lines.len() - 1].ends_with(b"\n");
        for (k, l) in e.repl.iter().enumerate() {
            if keep_open && k + 1 == e.repl.len() {
                push(&mut out, content(l));
            } else {
                push(&mut out, l);
            }
        }
        cursor = e.end;
    }
    for l in &lines[cursor..] {
        push(&mut out, l);
    }
    Ok(out)
}

// ---------- planning ----------

struct Planner {
    root: PathBuf,
    files: Vec<PlannedFile>,
    index: HashMap<PathBuf, usize>,
    ord: usize,
}

impl Planner {
    fn idx(&mut self, path: &Path) -> R<usize> {
        if let Some(i) = self.index.get(path) {
            return Ok(*i);
        }
        let bytes = std::fs::read(path)?;
        let role = if path == self.root { FileRole::Root } else { FileRole::Include };
        self.files.push(PlannedFile {
            role,
            original: path.to_path_buf(),
            path: PathBuf::new(),
            eol: detect_eol(&bytes),
            lines: split_lines(&bytes),
            edits: Vec::new(),
            link_edits: Vec::new(),
            new_bytes: Vec::new(),
        });
        self.index.insert(path.to_path_buf(), self.files.len() - 1);
        Ok(self.files.len() - 1)
    }

    fn next_ord(&mut self) -> usize {
        self.ord += 1;
        self.ord
    }
}

fn header_of(card: &InpCard) -> String {
    let Ok(f) = std::fs::File::open(&card.file) else { return String::new() };
    let mut r = BufReader::new(f);
    let mut buf = Vec::new();
    for _ in 0..card.line {
        buf.clear();
        if r.read_until(b'\n', &mut buf).unwrap_or(0) == 0 {
            return String::new();
        }
    }
    String::from_utf8_lossy(content(&buf)).trim().to_string()
}

fn card_ref(card: &InpCard) -> CardRef {
    CardRef { file: card.file.clone(), line: card.line, header: header_of(card) }
}

fn frequent_keywords(scan: &InpScan) -> Vec<String> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for c in &scan.cards {
        *counts.entry(c.keyword.as_str()).or_default() += 1;
    }
    let mut v: Vec<(&str, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    v.into_iter().map(|(k, _)| k.to_string()).collect()
}

fn resolve<'a>(scan: &'a InpScan, addr: &CardAddress) -> R<&'a InpCard> {
    if addr.keyword.trim().trim_start_matches('*').trim().is_empty() {
        return invalid("card.keyword is empty");
    }
    let mut matches = scan.find(&addr.keyword, addr.step);
    if let Some((name, value)) = &addr.param {
        let (name, value) = (name.trim().to_uppercase(), strip_quotes(value).to_uppercase());
        matches.retain(|c| {
            c.params
                .iter()
                .any(|(k, v)| *k == name && strip_quotes(v.as_deref().unwrap_or("")).to_uppercase() == value)
        });
    }
    let describe = || {
        let mut s = format!("*{}", addr.keyword.trim().trim_start_matches('*').trim().to_uppercase());
        if let Some(st) = addr.step {
            s += &format!(" in {}", if st == 0 { "the model level".to_string() } else { format!("step {st}") });
        }
        if let Some((n, v)) = &addr.param {
            s += &format!(" with {}={}", n.trim().to_uppercase(), v);
        }
        s
    };
    if matches.is_empty() {
        return Err(EditError::UnknownCard {
            message: format!("no card matches {}", describe()),
            valid: frequent_keywords(scan),
        });
    }
    match addr.index {
        Some(i) => matches.get(i).copied().ok_or_else(|| EditError::UnknownCard {
            message: format!("index {i} is out of range: {} cards match {}", matches.len(), describe()),
            valid: matches.iter().take(200).map(|c| fmt_ref(&card_ref(c))).collect(),
        }),
        None if matches.len() > 1 => Err(EditError::AmbiguousCard {
            message: format!("{} cards match {}; pass index (0-based) or narrow with step/param", matches.len(), describe()),
            candidates: matches.iter().take(200).map(|c| card_ref(c)).collect(),
        }),
        None => Ok(matches[0]),
    }
}

fn fmt_ref(r: &CardRef) -> String {
    format!("{}:{}  {}", r.file.display(), r.line, r.header)
}

fn canonical_target(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let name = p.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    let parent = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf()).join(name)
}

fn edit_name(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("include");
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}_edit.{ext}"),
        None => format!("{stem}_edit"),
    }
}

/// `*INCLUDE` header edit that points `INPUT=` to `new_value` (quotes of the old value kept).
fn include_edit(lines: &[Vec<u8>], card: &InpCard, new_value: &str, ord: usize) -> R<Vec<LineEdit>> {
    let ext = extent(lines, card.line)?;
    let raw = card
        .params
        .iter()
        .find(|(k, _)| k == "INPUT")
        .and_then(|(_, v)| v.clone())
        .unwrap_or_default();
    let value = match raw.chars().next() {
        Some(q) if q == '"' || q == '\'' => format!("{q}{new_value}{q}"),
        _ => new_value.to_string(),
    };
    edit_header(lines, &ext, "INPUT", Some(&value), ord)
}

fn include_value(card: &InpCard, new_file: &Path) -> String {
    let raw = card.params.iter().find(|(k, _)| k == "INPUT").and_then(|(_, v)| v.as_deref()).unwrap_or("");
    let unq = strip_quotes(raw);
    let dir = unq.rfind(['/', '\\']).map(|i| &unq[..=i]).unwrap_or("");
    format!("{dir}{}", new_file.file_name().and_then(|n| n.to_str()).unwrap_or(""))
}

/// Resolve every operation against the original deck and build the edited files in memory.
///
/// `output` is where the edited root deck will be written. Edited include files are written
/// next to their originals as `<stem>_edit.<ext>`, and the `*INCLUDE` lines on the path from
/// the root are retargeted. Files of the original deck are never written.
pub fn plan_edit(root: &Path, ops: &[EditOp], output: &Path) -> R<EditPlan> {
    if ops.is_empty() {
        return invalid("ops is empty");
    }
    let before = scan_cards(root, &ScanLimits::default()).map_err(EditError::Deck)?;
    let mut pl = Planner { root: root.to_path_buf(), files: Vec::new(), index: HashMap::new(), ord: 0 };
    pl.idx(root)?;
    let mut applied = Vec::new();
    // (file, header line) -> is an append_step anchor
    let mut touched: HashMap<(PathBuf, usize), (usize, bool)> = HashMap::new();
    let mut touch = |card: &InpCard, op: usize, append: bool| -> R<()> {
        let key = (card.file.clone(), card.line);
        if let Some((other, other_append)) = touched.get(&key) {
            if !(append && *other_append) {
                return Err(EditError::Conflict(format!(
                    "ops {other} and {op} both touch the card at {}:{}; merge them into one op or split the request",
                    card.file.display(),
                    card.line
                )));
            }
        }
        touched.insert(key, (op, append));
        Ok(())
    };

    for (n, op) in ops.iter().enumerate() {
        let ord = pl.next_ord();
        match op {
            EditOp::SetParam { card: addr, param, value } => {
                validate_param(param, value.as_deref())?;
                let card = resolve(&before, addr)?;
                touch(card, n, false)?;
                let fi = pl.idx(&card.file)?;
                let ext = extent(&pl.files[fi].lines, card.line)?;
                let edits = edit_header(&pl.files[fi].lines, &ext, param, value.as_deref(), ord)?;
                pl.files[fi].edits.extend(edits);
                applied.push(applied_op(n, "set_param", card));
            }
            EditOp::ReplaceData { card: addr, lines } => {
                let card = resolve(&before, addr)?;
                touch(card, n, false)?;
                let fi = pl.idx(&card.file)?;
                let ext = extent(&pl.files[fi].lines, card.line)?;
                let mut repl = Vec::new();
                for l in lines {
                    for part in l.lines() {
                        let t = part.trim();
                        if t.starts_with('*') && !t.starts_with("**") {
                            return invalid(format!("data line {part:?} starts with '*' and would be read as a keyword card"));
                        }
                    }
                    repl.extend(text_lines(l, &pl.files[fi].eol));
                    if l.is_empty() {
                        repl.push(pl.files[fi].eol.clone());
                    }
                }
                pl.files[fi].edits.push(LineEdit { start: ext.d_start, end: ext.d_end, repl, ord });
                applied.push(applied_op(n, "replace_data", card));
            }
            EditOp::InsertAfter { card: addr, text } => {
                let first = text.lines().find(|l| !l.trim().is_empty());
                if !first.is_some_and(|l| l.trim().starts_with('*')) {
                    return invalid("insert_after text must start with a '*' keyword line or a '**' comment");
                }
                let card = resolve(&before, addr)?;
                touch(card, n, false)?;
                let fi = pl.idx(&card.file)?;
                let ext = extent(&pl.files[fi].lines, card.line)?;
                let repl = text_lines(text, &pl.files[fi].eol);
                pl.files[fi].edits.push(LineEdit { start: ext.d_end, end: ext.d_end, repl, ord });
                applied.push(applied_op(n, "insert_after", card));
            }
            EditOp::DeleteCard { card: addr } => {
                let card = resolve(&before, addr)?;
                touch(card, n, false)?;
                let fi = pl.idx(&card.file)?;
                let ext = extent(&pl.files[fi].lines, card.line)?;
                pl.files[fi].edits.push(LineEdit { start: ext.h_start, end: ext.d_end, repl: Vec::new(), ord });
                applied.push(applied_op(n, "delete_card", card));
            }
            EditOp::AppendStep { text } => {
                let kws: Vec<String> = text.lines().filter_map(keyword_of).collect();
                if !kws.iter().any(|k| k == "STEP") || !kws.iter().any(|k| k == "END STEP") {
                    return invalid("append_step text must contain *STEP and *END STEP");
                }
                let anchor = before.cards.iter().filter(|c| c.keyword == "END STEP" && c.file == root).max_by_key(|c| c.line);
                let fi = pl.idx(root)?;
                let (at, line, header) = match anchor {
                    Some(c) => {
                        touch(c, n, true)?;
                        (extent(&pl.files[fi].lines, c.line)?.d_end, c.line, header_of(c))
                    }
                    None if before.cards.iter().any(|c| c.keyword == "STEP") => {
                        return invalid("the last *END STEP is not in the root deck; append_step only appends to the root file");
                    }
                    None => (pl.files[fi].lines.len(), pl.files[fi].lines.len(), String::new()),
                };
                let repl = text_lines(text, &pl.files[fi].eol);
                pl.files[fi].edits.push(LineEdit { start: at, end: at, repl, ord });
                applied.push(AppliedOp { op: n, kind: "append_step", file: root.to_path_buf(), line, header });
            }
        }
    }

    // Include chain: every edited include (and its ancestors) becomes a new file.
    let mut links: Vec<Link> = Vec::new();
    let mut queue: Vec<usize> = (1..pl.files.len()).collect();
    let mut linked: HashSet<usize> = HashSet::new();
    while let Some(i) = queue.pop() {
        if pl.files[i].original == root || !linked.insert(i) {
            continue;
        }
        let f = pl.files[i].original.clone();
        let sites: Vec<_> = before.includes.iter().filter(|inc| inc.resolved_path == f).collect();
        if sites.len() != 1 {
            return invalid(format!(
                "{} is included from {} places; edits inside it are ambiguous (use inp_consolidate first)",
                f.display(),
                sites.len()
            ));
        }
        let site = sites[0];
        let card = before
            .cards
            .iter()
            .find(|c| c.keyword == "INCLUDE" && c.file == site.included_from && c.line == site.line_number)
            .ok_or_else(|| EditError::Invalid { message: "include card not found".into(), valid: Vec::new() })?
            .clone();
        if let Some((op, _)) = touched.get(&(card.file.clone(), card.line)) {
            return Err(EditError::Conflict(format!(
                "op {op} touches the *INCLUDE card at {}:{} that leads to an edited file",
                card.file.display(),
                card.line
            )));
        }
        let parent = pl.idx(&site.included_from)?;
        queue.push(parent);
        links.push(Link { parent, child: i, card });
    }

    // New locations and safety checks.
    let mut tree: HashSet<PathBuf> = HashSet::new();
    tree.insert(canonical_target(root));
    for c in &before.cards {
        tree.insert(canonical_target(&c.file));
    }
    for inc in &before.includes {
        tree.insert(canonical_target(&inc.resolved_path));
    }
    let uses_relative = !before.includes.is_empty() || !before.external_refs().is_empty();
    if uses_relative {
        let same_dir = |a: &Path, b: &Path| canonical_target(&a.join("x")).parent() == canonical_target(&b.join("x")).parent();
        let od = output.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let rd = root.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        if !same_dir(od, rd) {
            return invalid(format!(
                "output_path must be in the directory of the root deck ({}): the deck uses relative file references",
                rd.display()
            ));
        }
    }
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for f in pl.files.iter_mut() {
        f.path = if f.role == FileRole::Root { output.to_path_buf() } else { f.original.with_file_name(edit_name(&f.original)) };
        let c = canonical_target(&f.path);
        if tree.contains(&c) {
            return invalid(format!("{} is a file of the original deck; originals are never overwritten", f.path.display()));
        }
        if !seen.insert(c) {
            return invalid(format!("two edited files would be written to {}", f.path.display()));
        }
    }
    for l in &links {
        let new_value = include_value(&l.card, &pl.files[l.child].path);
        let ord = pl.next_ord();
        let edits = include_edit(&pl.files[l.parent].lines, &l.card, &new_value, ord)?;
        pl.files[l.parent].link_edits.extend(edits);
    }
    for f in pl.files.iter_mut() {
        let all: Vec<&LineEdit> = f.edits.iter().chain(&f.link_edits).collect();
        f.new_bytes = apply(&f.lines, &f.eol, &all)?;
    }
    Ok(EditPlan { files: pl.files, applied, before, links })
}

fn applied_op(op: usize, kind: &'static str, card: &InpCard) -> AppliedOp {
    AppliedOp { op, kind, file: card.file.clone(), line: card.line, header: header_of(card) }
}

static SCRATCH: AtomicUsize = AtomicUsize::new(0);

impl EditPlan {
    /// Create the new files. With `overwrite = false` nothing is written when one exists.
    pub fn write(&self, overwrite: bool) -> R<()> {
        if !overwrite {
            if let Some(f) = self.files.iter().find(|f| f.path.exists()) {
                return Err(EditError::OutputExists(f.path.clone()));
            }
        }
        for f in &self.files {
            std::fs::write(&f.path, &f.new_bytes)?;
        }
        Ok(())
    }

    /// Scan the root deck written by [`EditPlan::write`].
    pub fn scan_written(&self) -> R<InpScan> {
        scan_cards(&self.files[0].path, &ScanLimits::default()).map_err(EditError::Deck)
    }

    /// Scan the edited deck without writing it: the new files go to a scratch directory
    /// whose include lines are absolute, and the directory is removed afterwards.
    /// (An `INPUT=` set by the user on an include card stays relative.)
    pub fn scan_preview(&self) -> R<InpScan> {
        let dir = std::env::temp_dir().join(format!("cgx-inp-edit-{}-{}", std::process::id(), SCRATCH.fetch_add(1, Ordering::SeqCst)));
        std::fs::create_dir_all(&dir)?;
        let result = self.preview_in(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        result
    }

    fn preview_in(&self, dir: &Path) -> R<InpScan> {
        let paths: Vec<PathBuf> = (0..self.files.len()).map(|i| dir.join(format!("f{i}_{}", self.files[i].path.file_name().and_then(|n| n.to_str()).unwrap_or("deck")))).collect();
        let abs = |p: &Path| if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir().map(|c| c.join(p)).unwrap_or_else(|_| p.to_path_buf()) };
        for (i, f) in self.files.iter().enumerate() {
            let mut extra: Vec<LineEdit> = Vec::new();
            let mut ord = 1_000_000;
            for l in self.links.iter().filter(|l| l.parent == i) {
                ord += 1;
                extra.extend(include_edit(&f.lines, &l.card, &paths[l.child].to_string_lossy(), ord)?);
            }
            for inc in self.before.includes.iter().filter(|inc| inc.included_from == f.original) {
                let idx = inc.line_number - 1;
                let touched = f.edits.iter().any(|e| e.start <= idx && idx < e.end) || self.links.iter().any(|l| l.parent == i && l.card.line == inc.line_number);
                if touched {
                    continue;
                }
                if let Some(card) = self.before.cards.iter().find(|c| c.keyword == "INCLUDE" && c.file == f.original && c.line == inc.line_number) {
                    ord += 1;
                    extra.extend(include_edit(&f.lines, card, &abs(&inc.resolved_path).to_string_lossy(), ord)?);
                }
            }
            let all: Vec<&LineEdit> = f.edits.iter().chain(&extra).collect();
            std::fs::write(&paths[i], apply(&f.lines, &f.eol, &all)?)?;
        }
        scan_cards(&paths[0], &ScanLimits::default()).map_err(EditError::Deck)
    }
}
