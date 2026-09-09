//! Parser, consolidator, and inspection engine for CalculiX / Abaqus `.inp` and `.nam` decks.

use crate::model::{Set, SetType};
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

/// Recursively inline all `*INCLUDE` statements into a single consolidated `.inp` string.
pub fn consolidate_inp<P: AsRef<Path>>(root_path: P) -> Result<(String, InpConsolidateStats)> {
    let root_path = root_path.as_ref().to_path_buf();
    if !root_path.exists() {
        return Err(FrdError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Root .inp file not found: {}", root_path.display()),
        )));
    }

    let mut output = String::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    let mut stats = InpConsolidateStats {
        root_file: root_path.clone(),
        ..Default::default()
    };

    consolidate_recursive(&root_path, &root_path, &mut output, &mut visited, &mut stats, 0)?;

    stats.total_lines_written = output.lines().count();
    Ok((output, stats))
}

fn consolidate_recursive(
    current_file: &Path,
    root_file: &Path,
    output: &mut String,
    visited: &mut HashSet<PathBuf>,
    stats: &mut InpConsolidateStats,
    depth: usize,
) -> Result<()> {
    if depth > 10 {
        return Err(FrdError::InvalidFormat(format!(
            "Exceeded maximum *INCLUDE recursion depth (10) at {}",
            current_file.display()
        )));
    }

    let canonical = fs::canonicalize(current_file).unwrap_or_else(|_| current_file.to_path_buf());
    if visited.contains(&canonical) {
        return Err(FrdError::InvalidFormat(format!(
            "Circular *INCLUDE detected: {}",
            current_file.display()
        )));
    }
    visited.insert(canonical.clone());

    let content = fs::read_to_string(current_file).map_err(|e| {
        FrdError::Io(std::io::Error::new(
            e.kind(),
            format!("Failed to read {}: {}", current_file.display(), e),
        ))
    })?;

    let parent_dir = current_file.parent().unwrap_or_else(|| Path::new("."));

    for line in content.lines() {
        if let Some(target_rel) = parse_include_target(line) {
            let target_path = parent_dir.join(&target_rel);
            if !target_path.exists() {
                return Err(FrdError::NotFound(format!(
                    "Included file not found: '{}' (referenced from {})",
                    target_rel,
                    current_file.display()
                )));
            }

            let display_name = target_path
                .strip_prefix(root_file.parent().unwrap_or_else(|| Path::new(".")))
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

            consolidate_recursive(&target_path, root_file, output, visited, stats, depth + 1)?;

            output.push_str(&format!(
                "** ====================================================================\n\
                 ** END INCLUDE: {}\n\
                 ** ====================================================================\n",
                display_name
            ));
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }

    Ok(())
}

/// Inspect an `.inp` file: discover all included files, check missing dependencies,
/// and summarize sets, boundary conditions, and steps.
pub fn inspect_inp<P: AsRef<Path>>(root_path: P) -> Result<InpSummary> {
    let root_path = root_path.as_ref().to_path_buf();
    let mut summary = InpSummary {
        root_file: root_path.clone(),
        ..Default::default()
    };

    let mut visited: HashSet<PathBuf> = HashSet::new();
    inspect_recursive(&root_path, &mut summary, &mut visited, 0)?;

    summary.total_files_count = visited.len();
    Ok(summary)
}

fn inspect_recursive(
    current_file: &Path,
    summary: &mut InpSummary,
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
                inspect_recursive(&target_path, summary, visited, depth + 1)?;
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
        let entry = sets.entry(name).or_insert_with(|| Set {
            name: set.name.clone(),
            set_type: set.set_type.clone(),
            node_ids: Vec::new(),
            element_ids: Vec::new(),
        });
        entry.node_ids.extend(set.node_ids);
        entry.element_ids.extend(set.element_ids);
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
                sets.extend(inp_sets);
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
                        let entry = sets.entry(name).or_insert_with(|| Set {
                            name: s.name.clone(),
                            set_type: s.set_type.clone(),
                            node_ids: Vec::new(),
                            element_ids: Vec::new(),
                        });
                        entry.node_ids.extend(s.node_ids);
                        entry.element_ids.extend(s.element_ids);
                    }
                }
            }
        }
    }

    sets
}
