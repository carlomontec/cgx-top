//! Parser, consolidator, and inspection engine for CalculiX / Abaqus `.inp` and `.nam` decks.

use crate::parser::inp_cards::ScanLimits;
use crate::model::{merge_set_into, Set, SetType};
use crate::{FrdError, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Record of an included file within an `.inp` deck
#[derive(Debug, Clone)]
pub struct InpInclude {
    pub raw_target: String,
    pub resolved_path: PathBuf,
    pub exists: bool,
    pub file_size_bytes: u64,
    pub line_number: usize,
    pub included_from: PathBuf,
}

/// Structural summary of an `.inp` deck and its dependencies
#[derive(Debug, Clone, Default)]
pub struct InpSummary {
    pub root_file: PathBuf,
    pub includes: Vec<InpInclude>,
    pub node_sets: HashMap<String, usize>,
    pub element_sets: HashMap<String, usize>,
    pub materials: Vec<String>,
    pub step_count: usize,
    pub boundary_count: usize,
    pub has_nodes: bool,
    pub has_elements: bool,
    pub total_lines: usize,
    pub total_files_count: usize,
}

/// Statistics for a consolidation operation
#[derive(Debug, Clone, Default)]
pub struct InpConsolidateStats {
    pub root_file: PathBuf,
    pub output_file: Option<PathBuf>,
    pub total_files_inlined: usize,
    pub total_lines_written: usize,
    pub inlined_files: Vec<PathBuf>,
}

/// Extract the file target from an `*INCLUDE` line, e.g.:
/// `*INCLUDE, INPUT=NFIX.nam` or `*include, input="all.msh"`
pub fn parse_include_target(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with('*') || trimmed.starts_with("**") {
        return None;
    }

    let upper = trimmed.to_uppercase();
    if !upper.starts_with("*INCLUDE") {
        return None;
    }

    // Split on commas: *INCLUDE, INPUT=foo
    for part in trimmed.split(',') {
        let part_trimmed = part.trim();
        let part_upper = part_trimmed.to_uppercase();
        if part_upper.starts_with("INPUT=") {
            let val = part_trimmed[6..].trim();
            let clean = val.trim_matches(|c| c == '"' || c == '\'').trim();
            if !clean.is_empty() {
                return Some(clean.to_string());
            }
        }
    }

    None
}

/// Resource accounting shared by the recursive walkers, enforcing [`ScanLimits`].
struct Budget {
    limits: ScanLimits,
    files_opened: usize,
    total_bytes: u64,
}

impl Budget {
    fn new() -> Self {
        Budget {
            limits: ScanLimits::default(),
            files_opened: 0,
            total_bytes: 0,
        }
    }

    /// Check depth and file-count limits, then account for one more opened file.
    fn enter(&mut self, path: &Path, depth: usize) -> Result<()> {
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
        let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if self.total_bytes.saturating_add(size) > self.limits.max_total_bytes {
            return Err(FrdError::InvalidFormat(format!(
                "Exceeded maximum total bytes ({}) at {}",
                self.limits.max_total_bytes,
                path.display()
            )));
        }
        self.files_opened += 1;
        self.total_bytes += size;
        Ok(())
    }
}

fn circular_error(path: &Path) -> FrdError {
    FrdError::InvalidFormat(format!("Circular *INCLUDE detected: {}", path.display()))
}

fn read_file(path: &Path) -> Result<String> {
    fs::read_to_string(path).map_err(|e| {
        FrdError::Io(std::io::Error::new(
            e.kind(),
            format!("Failed to read {}: {}", path.display(), e),
        ))
    })
}

fn is_comment_or_blank(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with("**")
}

/// Collect the header starting at `lines[start]` (a `*` card), joining continuation lines
/// while the text ends with a comma. Returns the joined text and the index after the header.
fn collect_header(lines: &[&str], start: usize) -> (String, usize) {
    let mut text = lines[start].trim().to_string();
    let mut next = start + 1;
    while text.ends_with(',') && next < lines.len() {
        let cont = lines[next].trim();
        next += 1;
        if is_comment_or_blank(cont) {
            continue;
        }
        text.push(' ');
        text.push_str(cont);
    }
    (text, next)
}

/// Value of the `INPUT=<file>` parameter of a non-INCLUDE card header, if any, together
/// with the header re-rendered without that parameter.
fn split_input_param(header: &str) -> Option<(String, String)> {
    let upper = header.to_uppercase();
    if upper.starts_with("*INCLUDE") {
        return None;
    }
    let mut target = None;
    let mut kept: Vec<&str> = Vec::new();
    for part in header.split(',') {
        let p = part.trim();
        if p.to_uppercase().starts_with("INPUT=") {
            let val = p[6..].trim().trim_matches(|c| c == '"' || c == '\'').trim();
            if !val.is_empty() && target.is_none() {
                target = Some(val.to_string());
                continue;
            }
        }
        if !p.is_empty() {
            kept.push(p);
        }
    }
    target.map(|t| (kept.join(", "), t))
}

/// Recursively inline all `*INCLUDE` statements into a single consolidated `.inp` string.
///
/// Cards other than `*INCLUDE` carrying `INPUT=<file>` (e.g. `*NODE, INPUT=nodes.inc`) are
/// inlined too: the header is written without the `INPUT` parameter, followed by the
/// file's lines. A file included more than once (a diamond) is inlined each time; only a
/// real cycle is an error. Limits come from [`ScanLimits::default`].
pub fn consolidate_inp<P: AsRef<Path>>(root_path: P) -> Result<(String, InpConsolidateStats)> {
    let root_path = root_path.as_ref().to_path_buf();
    if !root_path.exists() {
        return Err(FrdError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Root .inp file not found: {}", root_path.display()),
        )));
    }

    let mut output = String::new();
    let mut stack: Vec<PathBuf> = Vec::new();
    let mut budget = Budget::new();
    let mut stats = InpConsolidateStats {
        root_file: root_path.clone(),
        ..Default::default()
    };

    consolidate_recursive(
        &root_path,
        &root_path,
        &mut output,
        &mut stack,
        &mut budget,
        &mut stats,
        0,
    )?;

    stats.total_lines_written = output.lines().count();
    Ok((output, stats))
}

fn consolidate_recursive(
    current_file: &Path,
    root_file: &Path,
    output: &mut String,
    stack: &mut Vec<PathBuf>,
    budget: &mut Budget,
    stats: &mut InpConsolidateStats,
    depth: usize,
) -> Result<()> {
    let canonical = fs::canonicalize(current_file).unwrap_or_else(|_| current_file.to_path_buf());
    if stack.contains(&canonical) {
        return Err(circular_error(current_file));
    }
    budget.enter(current_file, depth)?;
    stack.push(canonical);
    let result = consolidate_body(current_file, root_file, output, stack, budget, stats, depth);
    stack.pop();
    result
}

fn consolidate_body(
    current_file: &Path,
    root_file: &Path,
    output: &mut String,
    stack: &mut Vec<PathBuf>,
    budget: &mut Budget,
    stats: &mut InpConsolidateStats,
    depth: usize,
) -> Result<()> {
    let content = read_file(current_file)?;
    let lines: Vec<&str> = content.lines().collect();
    let parent_dir = current_file.parent().unwrap_or_else(|| Path::new("."));
    let root_dir = root_file.parent().unwrap_or_else(|| Path::new("."));

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if !trimmed.starts_with('*') || trimmed.starts_with("**") {
            output.push_str(line);
            output.push('\n');
            i += 1;
            continue;
        }

        let (header, next) = collect_header(&lines, i);
        let is_include = header.to_uppercase().starts_with("*INCLUDE");
        let include_target = if is_include {
            parse_include_target(&header)
        } else {
            None
        };

        if let Some(target_rel) = include_target {
            let target_path = parent_dir.join(&target_rel);
            if !target_path.exists() {
                return Err(FrdError::NotFound(format!(
                    "Included file not found: '{}' (referenced from {})",
                    target_rel,
                    current_file.display()
                )));
            }
            let display_name = target_path
                .strip_prefix(root_dir)
                .unwrap_or(&target_path)
                .display()
                .to_string();

            output.push_str(&format!(
                "** ====================================================================\n\
                 ** BEGIN INCLUDE: {}\n\
                 ** ====================================================================\n",
                display_name
            ));
            stats.total_files_inlined += 1;
            stats.inlined_files.push(target_path.clone());
            consolidate_recursive(&target_path, root_file, output, stack, budget, stats, depth + 1)?;
            output.push_str(&format!(
                "** ====================================================================\n\
                 ** END INCLUDE: {}\n\
                 ** ====================================================================\n",
                display_name
            ));
        } else if let Some((bare_header, target_rel)) = split_input_param(&header) {
            let target_path = parent_dir.join(&target_rel);
            if !target_path.is_file() {
                return Err(FrdError::NotFound(format!(
                    "Input file not found: '{}' (referenced from {})",
                    target_rel,
                    current_file.display()
                )));
            }
            budget.enter(&target_path, depth + 1)?;
            let data = read_file(&target_path)?;
            stats.total_files_inlined += 1;
            stats.inlined_files.push(target_path.clone());

            output.push_str(&bare_header);
            output.push('\n');
            for data_line in data.lines() {
                output.push_str(data_line);
                output.push('\n');
            }
        } else {
            for original in &lines[i..next] {
                output.push_str(original);
                output.push('\n');
            }
        }
        i = next;
    }

    Ok(())
}

/// Inspect an `.inp` file: discover all included files, check missing dependencies,
/// and summarize sets, boundary conditions, and steps.
///
/// Limits come from [`ScanLimits::default`]; an include cycle or an exceeded limit is an
/// error. A file included more than once is walked each time (as consolidation would
/// inline it) but counted once in `total_files_count`.
pub fn inspect_inp<P: AsRef<Path>>(root_path: P) -> Result<InpSummary> {
    let root_path = root_path.as_ref().to_path_buf();
    let mut summary = InpSummary {
        root_file: root_path.clone(),
        ..Default::default()
    };

    let mut stack: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut budget = Budget::new();
    inspect_recursive(&root_path, &mut summary, &mut stack, &mut seen, &mut budget, 0)?;

    summary.total_files_count = seen.len();
    Ok(summary)
}

fn inspect_recursive(
    current_file: &Path,
    summary: &mut InpSummary,
    stack: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    budget: &mut Budget,
    depth: usize,
) -> Result<()> {
    let canonical = fs::canonicalize(current_file).unwrap_or_else(|_| current_file.to_path_buf());
    if stack.contains(&canonical) {
        return Err(circular_error(current_file));
    }
    budget.enter(current_file, depth)?;
    seen.insert(canonical.clone());
    stack.push(canonical);
    let result = inspect_body(current_file, summary, stack, seen, budget, depth);
    stack.pop();
    result
}

fn inspect_body(
    current_file: &Path,
    summary: &mut InpSummary,
    stack: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    budget: &mut Budget,
    depth: usize,
) -> Result<()> {
    let content = match fs::read_to_string(current_file) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    summary.total_lines += content.lines().count();
    let parent_dir = current_file.parent().unwrap_or_else(|| Path::new("."));

    for (line_idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some(target_rel) = parse_include_target(trimmed) {
            let target_path = parent_dir.join(&target_rel);
            let exists = target_path.exists();
            let size = if exists {
                fs::metadata(&target_path).map(|m| m.len()).unwrap_or(0)
            } else {
                0
            };

            summary.includes.push(InpInclude {
                raw_target: target_rel.clone(),
                resolved_path: target_path.clone(),
                exists,
                file_size_bytes: size,
                line_number: line_idx + 1,
                included_from: current_file.to_path_buf(),
            });

            if exists {
                inspect_recursive(&target_path, summary, stack, seen, budget, depth + 1)?;
            }
        } else if trimmed.starts_with('*') && !trimmed.starts_with("**") {
            let upper = trimmed.to_uppercase();
            if upper.starts_with("*STEP") {
                summary.step_count += 1;
            } else if upper.starts_with("*BOUNDARY") {
                summary.boundary_count += 1;
            } else if upper.starts_with("*MATERIAL") {
                if let Some(name) = extract_param_value(trimmed, "NAME") {
                    if !summary.materials.contains(&name) {
                        summary.materials.push(name);
                    }
                }
            } else if upper.starts_with("*NODE") && !upper.starts_with("*NODE FILE") && !upper.starts_with("*NODE PRINT") {
                summary.has_nodes = true;
            } else if upper.starts_with("*ELEMENT") && !upper.starts_with("*ELEMENT OUTPUT") {
                summary.has_elements = true;
            }
        }
    }

    // Also extract sets from this file
    let sets = parse_sets_from_str(&content);
    for (name, set) in sets {
        match set.set_type {
            SetType::Node => {
                summary.node_sets.insert(name, set.node_ids.len());
            }
            SetType::Element => {
                summary.element_sets.insert(name, set.element_ids.len());
            }
            SetType::Mixed => {
                summary.node_sets.insert(name.clone(), set.node_ids.len());
                summary.element_sets.insert(name, set.element_ids.len());
            }
        }
    }

    Ok(())
}

/// Extract parameter value from a keyword line, e.g. `*NSET, NSET=FOO, GENERATE` -> "FOO"
fn extract_param_value(line: &str, param_name: &str) -> Option<String> {
    let param_upper = format!("{}=", param_name.to_uppercase());
    for part in line.split(',') {
        let trimmed = part.trim();
        let upper = trimmed.to_uppercase();
        if upper.starts_with(&param_upper) {
            let val = trimmed[param_upper.len()..].trim();
            return Some(val.trim_matches(|c| c == '"' || c == '\'').to_string());
        }
    }
    None
}

/// Check if keyword line has a specific flag, e.g. `GENERATE` in `*NSET, NSET=FOO, GENERATE`
fn has_keyword_flag(line: &str, flag_name: &str) -> bool {
    let flag_upper = flag_name.to_uppercase();
    line.split(',').any(|part| part.trim().to_uppercase() == flag_upper)
}

/// Parse all `*NSET` and `*ELSET` definitions from a string buffer
pub fn parse_sets_from_str(content: &str) -> HashMap<String, Set> {
    let mut sets: HashMap<String, Set> = HashMap::new();

    let mut current_set_name: Option<String> = None;
    let mut current_set_type: Option<SetType> = None;
    let mut current_is_generate = false;
    let mut current_ids: Vec<u32> = Vec::new();

    let flush_set = |name: &Option<String>,
                     stype: &Option<SetType>,
                     ids: &mut Vec<u32>,
                     sets: &mut HashMap<String, Set>| {
        if let (Some(n), Some(t)) = (name, stype) {
            let entry = sets.entry(n.clone()).or_insert_with(|| Set {
                name: n.clone(),
                set_type: t.clone(),
                node_ids: Vec::new(),
                element_ids: Vec::new(),
            });

            match t {
                SetType::Node => entry.node_ids.extend(ids.drain(..)),
                SetType::Element => entry.element_ids.extend(ids.drain(..)),
                SetType::Mixed => entry.node_ids.extend(ids.drain(..)),
            }
        }
        ids.clear();
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("**") {
            continue;
        }

        if trimmed.starts_with('*') {
            // Flush any ongoing set
            flush_set(
                &current_set_name,
                &current_set_type,
                &mut current_ids,
                &mut sets,
            );
            current_set_name = None;
            current_set_type = None;
            current_is_generate = false;

            let upper = trimmed.to_uppercase();
            if upper.starts_with("*NSET") {
                if let Some(name) = extract_param_value(trimmed, "NSET") {
                    current_set_name = Some(name);
                    current_set_type = Some(SetType::Node);
                    current_is_generate = has_keyword_flag(trimmed, "GENERATE");
                }
            } else if upper.starts_with("*ELSET") {
                if let Some(name) = extract_param_value(trimmed, "ELSET") {
                    current_set_name = Some(name);
                    current_set_type = Some(SetType::Element);
                    current_is_generate = has_keyword_flag(trimmed, "GENERATE");
                }
            }
        } else if current_set_name.is_some() {
            // Data line for the active set
            if current_is_generate {
                // Format: start, end [, step]
                let parts: Vec<&str> = trimmed.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
                if parts.len() >= 2 {
                    if let (Ok(start), Ok(end)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
                        let step = if parts.len() >= 3 {
                            parts[2].parse::<u32>().unwrap_or(1).max(1)
                        } else {
                            1
                        };
                        let mut curr = start;
                        while curr <= end {
                            current_ids.push(curr);
                            curr += step;
                        }
                    }
                }
            } else {
                // Comma-separated list of IDs across lines
                for part in trimmed.split(',') {
                    let id_str = part.trim();
                    if let Ok(id) = id_str.parse::<u32>() {
                        current_ids.push(id);
                    }
                }
            }
        }
    }

    flush_set(
        &current_set_name,
        &current_set_type,
        &mut current_ids,
        &mut sets,
    );

    for set in sets.values_mut() {
        set.dedup_ids();
    }
    sets
}

/// Recursively parse all sets from an `.inp` file and all its includes.
pub fn parse_sets_from_inp<P: AsRef<Path>>(root_path: P) -> Result<HashMap<String, Set>> {
    let root_path = root_path.as_ref().to_path_buf();
    let mut sets: HashMap<String, Set> = HashMap::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();

    parse_sets_recursive(&root_path, &mut sets, &mut visited, 0)?;
    Ok(sets)
}

fn parse_sets_recursive(
    current_file: &Path,
    sets: &mut HashMap<String, Set>,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) -> Result<()> {
    if depth > 10 {
        return Ok(());
    }

    let canonical = fs::canonicalize(current_file).unwrap_or_else(|_| current_file.to_path_buf());
    if visited.contains(&canonical) {
        return Ok(());
    }
    visited.insert(canonical);

    let content = match fs::read_to_string(current_file) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    let local_sets = parse_sets_from_str(&content);
    for (name, set) in local_sets {
        merge_set_into(sets, name, set);
    }

    let parent_dir = current_file.parent().unwrap_or_else(|| Path::new("."));
    for line in content.lines() {
        if let Some(target_rel) = parse_include_target(line) {
            let target_path = parent_dir.join(&target_rel);
            if target_path.exists() {
                parse_sets_recursive(&target_path, sets, visited, depth + 1)?;
            }
        }
    }

    Ok(())
}

/// Auto-discover and load companion sets for a given `.frd` file.
/// Looks for `<stem>.inp` and any `*.nam` files in the same directory.
pub fn load_companion_sets<P: AsRef<Path>>(frd_path: P) -> HashMap<String, Set> {
    let frd_path = frd_path.as_ref();
    let mut sets = HashMap::new();

    let parent_dir = match frd_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };

    // 1. Check for <stem>.inp
    if let Some(stem) = frd_path.file_stem() {
        let companion_inp = parent_dir.join(format!("{}.inp", stem.to_string_lossy()));
        if companion_inp.exists() {
            if let Ok(inp_sets) = parse_sets_from_inp(&companion_inp) {
                for (name, set) in inp_sets {
                    merge_set_into(&mut sets, name, set);
                }
            }
        }
    }

    // 2. Scan directory for *.nam files
    if let Ok(entries) = fs::read_dir(parent_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("nam") {
                if let Ok(nam_sets) = parse_sets_from_inp(&path) {
                    for (name, s) in nam_sets {
                        merge_set_into(&mut sets, name, s);
                    }
                }
            }
        }
    }

    sets
}
