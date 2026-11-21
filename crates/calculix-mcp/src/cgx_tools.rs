//! Geometry and meshing through CalculiX GraphiX: `cgx_run_batch` runs a `.fbd` script with
//! `cgx -bg` in a private run directory; `cgx_help` returns the verified command reference.
//!
//! Before anything runs, the script and every file it `read`s are screened (see [`scan_commands`]):
//! `sys`, external programs and path arguments that leave the run directory are rejected. This is
//! a mitigation, not a sandbox: cgx substitutes `valu` contents into parameters at run time, which
//! a static scan cannot follow.

use crate::error::ToolError;
use crate::jobs::{exit_parts, isolated_command, kill_group, over_quota, terminate_group};
use crate::mesh_tools::BBox;
use crate::paths::Roots;
use cgx_core::read_inp_mesh;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CHEATSHEET: &str = include_str!("../resources/cgx_cheatsheet.md");

const MAX_INLINE_BYTES: usize = 64 * 1024;
/// Largest command or deck file that is read to be screened.
const MAX_SCANNED_BYTES: u64 = 256 * 1024 * 1024;
/// Nesting of `read` (and deck `*INCLUDE`) below the top script.
const MAX_DEPTH: usize = 8;
/// Files screened in one run, counting a file again for every place that reads it.
const MAX_SCANNED_FILES: usize = 1000;
const DEFAULT_TIMEOUT_S: u64 = 300;
const MAX_TIMEOUT_S: u64 = 3600;
const TAIL_BYTES: u64 = 4096;
const MAX_FILES_LISTED: usize = 200;
const MAX_MESH_SUMMARIES: usize = 20;
const MAX_ERROR_LINES: usize = 20;
const INLINE_NAME: &str = "script.fbd";
/// Files of the run directory that belong to the server, not to the script.
const OWN_FILES: [&str; 3] = ["stdout.log", "stderr.log", "tmp"];

/// The configured CGX binary and the limits of a run.
#[derive(Debug, Clone)]
pub struct CgxConfig {
    pub binary: PathBuf,
    pub env_passthrough: Vec<String>,
    /// A run directory larger than this is killed
    pub max_run_bytes: u64,
    /// Time between SIGTERM and SIGKILL when a run is stopped
    pub grace: Duration,
}

// ---------- screening ----------

/// `isspace` in the C locale: what cgx's `sscanf("%s")` splits words on.
fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The lines cgx executes, as (first physical line number, text). Like cgx's `frecord` and
/// `getCommandLine`: a line ends at `\n`, `\r\n` or a lone `\r`, and a line ending in `=` continues
/// on the next one (the `=` is dropped and nothing is inserted).
fn logical_lines(text: &[u8]) -> Vec<(usize, Vec<u8>)> {
    let mut physical: Vec<Vec<u8>> = Vec::new();
    let mut cur = Vec::new();
    let mut i = 0;
    while i < text.len() {
        match text[i] {
            b'\n' => physical.push(std::mem::take(&mut cur)),
            b'\r' => {
                physical.push(std::mem::take(&mut cur));
                if text.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            b => cur.push(b),
        }
        i += 1;
    }
    if !cur.is_empty() {
        physical.push(cur);
    }
    let mut out = Vec::new();
    let mut pending: Option<(usize, Vec<u8>)> = None;
    for (k, mut line) in physical.into_iter().enumerate() {
        let continued = line.last() == Some(&b'=');
        if continued {
            line.pop();
        }
        let entry = pending.get_or_insert_with(|| (k + 1, Vec::new()));
        entry.1.extend_from_slice(&line);
        if !continued {
            out.extend(pending.take());
        }
    }
    out.extend(pending);
    out
}

fn words(line: &[u8]) -> Vec<String> {
    line.split(|&b| is_c_space(b)).filter(|w| !w.is_empty()).map(|w| String::from_utf8_lossy(w).into_owned()).collect()
}

/// Why a parameter could address a file outside the run directory, if it could.
fn path_problem(word: &str) -> Option<&'static str> {
    let b = word.as_bytes();
    if word.contains("..") {
        Some("contains '..'")
    } else if b.len() > 1 && b[0] == b'/' {
        Some("is an absolute path")
    } else if b.first() == Some(&b'~') {
        Some("starts with '~' (home directory)")
    } else if word.starts_with("\\\\") || (b.len() > 1 && b[0].is_ascii_alphabetic() && b[1] == b':') {
        Some("is an absolute Windows path")
    } else {
        None
    }
}

/// Why a parameter could inject a shell command, if it could. Some `send` writers pass set names
/// to `system()` (`cat <set>.equ >> ...`), so names must not carry shell syntax.
fn shell_problem(word: &str) -> Option<&'static str> {
    const OPERATORS: [&str; 7] = ["&", "<", ">", "==", "!=", "<=", ">="];
    if word.contains([';', '|', '`', '$', '(', ')']) {
        Some("contains a shell metacharacter (; | ` $ ( ))")
    } else if word.contains(['&', '<', '>']) && !OPERATORS.contains(&word) {
        Some("contains a shell metacharacter (& < >)")
    } else {
        None
    }
}

fn is_number(word: &str) -> bool {
    word.parse::<f64>().is_ok()
}

/// How cgx's `read` (`pre_read`) treats a file, from its extension and first parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadKind {
    /// Executed line by line, like the script itself
    Commands,
    /// A CalculiX/Abaqus deck: set names and `*INCLUDE` files matter
    Deck,
    /// Read as data (frd, stl, list, stack, ...)
    Data,
}

/// Mirror of the dispatch in cgx's `pre_read` (setFunktions.c). Comparisons there are
/// case-sensitive and look at the first 2-4 characters only.
fn read_kind(file: &str, param1: &str) -> Result<ReadKind, &'static str> {
    let ext = file.rfind('.').map_or(file, |k| &file[k + 1..]);
    let (e, p) = (|s: &str| ext.starts_with(s), |s: &str| param1.starts_with(s));
    Ok(if p("sta") || p("-") || e("xwd") || e("edg") || p("edg") {
        ReadKind::Data
    } else if p("foam") {
        return Err("'read <dir> foam' reads a whole OpenFOAM case directory, which this tool does not copy");
    } else if e("frd") || p("ng") || e("stl") || p("vtk") {
        ReadKind::Data
    } else if e("inp") || p("inp") {
        ReadKind::Deck
    } else if param1.is_empty() || p("rep") || p("add") {
        ReadKind::Commands
    } else {
        ReadKind::Data
    })
}

/// A file named by a `read` command or an `*INCLUDE` card.
#[derive(Debug, Clone, PartialEq)]
struct ReadRef {
    line: usize,
    path: String,
    kind: ReadKind,
}

fn rejected(label: &str, line: usize, why: impl std::fmt::Display) -> ToolError {
    ToolError::invalid(format!("{}:{}: {}", label, line, why))
        .hint("Remove or rewrite that line. Call cgx_help for the supported commands; files may only be read and written inside the run directory.")
}

/// Screen one cgx command file. Returns the files it reads, in order.
fn scan_commands(label: &str, text: &[u8]) -> Result<Vec<ReadRef>, ToolError> {
    if text.contains(&0) {
        return Err(ToolError::invalid(format!("{}: contains a NUL byte", label)));
    }
    let mut reads = Vec::new();
    let mut values: HashSet<String> = HashSet::new();
    for (line, raw) in logical_lines(text) {
        let w = words(&raw);
        let Some(first) = w.first() else { continue };
        // cgx ignores a line whose keyword starts with one of these.
        if first.starts_with(['#', '!', '$']) {
            continue;
        }
        let kw = first.to_ascii_uppercase();
        let args = &w[1..];
        match kw.as_str() {
            "SYS" => return Err(rejected(label, line, "'sys' runs a shell command and is not allowed")),
            "GRAPH" => return Err(rejected(label, line, "'graph' runs gnuplot and a viewer through the shell and is not allowed")),
            "MOVI" | "MOVIE" => return Err(rejected(label, line, "'movi' starts ffmpeg and needs a window; it is not allowed")),
            "ASGN" => {
                let what = args.first().map(|a| a.to_ascii_lowercase()).unwrap_or_default();
                if ["viewer", "browser", "cgxhelpfile", "ccxhelpfile", "aflibhelpfile"].contains(&what.as_str()) {
                    return Err(rejected(label, line, format!("'asgn {}' sets a program or file cgx would open; it is not allowed", what)));
                }
            }
            "VALU" => {
                if let Some(name) = args.first() {
                    values.insert(name.trim_start_matches('!').to_string());
                }
                if args.iter().any(|a| a == "?") {
                    return Err(rejected(label, line, "'valu ... ?' waits for keyboard input, which a batch run cannot give"));
                }
                if args.get(1).is_some_and(|op| op == "&") {
                    if let Some(bad) = args[2..].iter().find(|a| !is_number(a) && a.contains(['.', '/', '\\', '~', ':'])) {
                        return Err(rejected(label, line, format!("concatenating '{}' could build a path; use plain names", bad)));
                    }
                }
            }
            _ => {}
        }
        for a in args {
            if let Some(why) = path_problem(a).or_else(|| shell_problem(a)) {
                return Err(rejected(label, line, format!("parameter '{}' {}", a, why)));
            }
        }
        if kw == "READ" {
            let Some(file) = args.first() else { continue };
            if values.contains(file.as_str()) {
                return Err(rejected(label, line, format!("'read {}' names a value; read takes a literal file name", file)));
            }
            let kind = read_kind(file, args.get(1).map_or("", String::as_str)).map_err(|why| rejected(label, line, why))?;
            reads.push(ReadRef { line, path: file.clone(), kind });
        }
    }
    Ok(reads)
}

/// Screen a deck that cgx reads with `read x.inp`: its set names become file names in later
/// `send` commands, and cgx follows its `*INCLUDE` cards.
fn scan_deck(label: &str, text: &[u8]) -> Result<Vec<ReadRef>, ToolError> {
    let mut includes = Vec::new();
    for (k, raw) in text.split(|&b| b == b'\n').enumerate() {
        let line = String::from_utf8_lossy(raw);
        let line = line.trim();
        if !line.starts_with('*') || line.starts_with("**") {
            continue;
        }
        let mut parts = line.split(',');
        let keyword = parts.next().unwrap_or("").trim().to_ascii_uppercase();
        for param in parts {
            let Some((key, value)) = param.split_once('=') else { continue };
            let key = key.trim().to_ascii_uppercase();
            let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
            if !matches!(key.as_str(), "INPUT" | "NSET" | "ELSET" | "NAME") {
                continue;
            }
            if let Some(why) = path_problem(value).or_else(|| shell_problem(value)) {
                return Err(rejected(label, k + 1, format!("{}={} {}", key, value, why)));
            }
            if keyword.starts_with("*INCL") && key == "INPUT" {
                includes.push(ReadRef { line: k + 1, path: value.to_string(), kind: ReadKind::Deck });
            }
        }
    }
    Ok(includes)
}

/// Files to copy into the run directory: (canonical source, path relative to the run directory).
#[derive(Debug, Default)]
struct Plan {
    files: Vec<(PathBuf, PathBuf)>,
    scanned: usize,
}

/// Relative path as written, without `.` components; None if it has anything else odd in it.
fn clean_relative(raw: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for c in Path::new(raw).components() {
        match c {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Screen `text` (a file of `kind` at `label`) and, recursively, every file it reads. cgx opens
/// these relative to its working directory, which is the top script's directory (`base`) in the
/// copy, so every path is resolved against `base`, also inside nested files.
#[allow(clippy::too_many_arguments)]
fn plan_reads(
    roots: &Roots,
    base: &Path,
    top_name: &str,
    label: &str,
    text: &[u8],
    kind: ReadKind,
    chain: &mut Vec<PathBuf>,
    plan: &mut Plan,
) -> Result<(), ToolError> {
    let refs = match kind {
        ReadKind::Commands => scan_commands(label, text)?,
        ReadKind::Deck => scan_deck(label, text)?,
        ReadKind::Data => return Ok(()),
    };
    for r in refs {
        let here = |why: String| rejected(label, r.line, why);
        if chain.len() >= MAX_DEPTH {
            return Err(here(format!("'{}' is nested deeper than {} reads", r.path, MAX_DEPTH)));
        }
        let rel = clean_relative(&r.path).ok_or_else(|| here(format!("'{}' is not a plain relative path", r.path)))?;
        let source = match base.join(&rel).canonicalize() {
            Ok(p) if p.is_file() => p,
            _ => {
                return Err(ToolError::new("FILE_NOT_FOUND", format!("{}:{}: no such file '{}'", label, r.line, r.path))
                    .hint("Paths in read and *INCLUDE are relative to the directory of the top script (for an inline script: the server root)."))
            }
        };
        if !roots.contains(&source) {
            return Err(ToolError::new("PATH_OUTSIDE_ROOTS", format!("{}:{}: '{}' resolves outside the allowed roots", label, r.line, r.path)));
        }
        if chain.contains(&source) {
            return Err(here(format!("'{}' reads itself (directly or through other files)", r.path)));
        }
        if rel == Path::new(top_name) {
            return Err(here(format!("'{}' is the top script itself", r.path)));
        }
        plan.scanned += 1;
        if plan.scanned > MAX_SCANNED_FILES {
            return Err(here(format!("more than {} file reads", MAX_SCANNED_FILES)));
        }
        if !plan.files.iter().any(|(_, d)| d == &rel) {
            plan.files.push((source.clone(), rel.clone()));
        }
        if r.kind != ReadKind::Data {
            let size = std::fs::metadata(&source).map(|m| m.len()).unwrap_or(0);
            if size > MAX_SCANNED_BYTES {
                return Err(here(format!("'{}' is larger than {} MiB", r.path, MAX_SCANNED_BYTES >> 20)));
            }
            let nested = std::fs::read(&source).map_err(ToolError::internal)?;
            chain.push(source);
            let nested_label = rel.display().to_string().replace('\\', "/"); // same label on every OS
            plan_reads(roots, base, top_name, &nested_label, &nested, r.kind, chain, plan)?;
            chain.pop();
        }
    }
    Ok(())
}

// ---------- input ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CgxRunIn {
    /// Inline CalculiX GraphiX commands (.fbd text, at most 64 KiB). Give this or script_path. Its read paths are relative to the server root
    pub script: Option<String>,
    /// A .fbd or .fbl file inside the roots. It and every file it reads (paths relative to its directory) are copied into the run directory
    pub script_path: Option<String>,
    /// Stop cgx after this many seconds (default 300, max 3600)
    pub timeout_s: Option<u64>,
}

/// Everything needed to start cgx in a staged run directory.
#[derive(Debug)]
struct Staged {
    run_id: String,
    run_dir: PathBuf,
    script_name: String,
    copied: Vec<String>,
    timeout: Duration,
    before: HashMap<PathBuf, (u64, Option<SystemTime>)>,
}

fn validate_timeout(t: Option<u64>) -> Result<Duration, ToolError> {
    match t.unwrap_or(DEFAULT_TIMEOUT_S) {
        s @ 1..=MAX_TIMEOUT_S => Ok(Duration::from_secs(s)),
        s => Err(ToolError::invalid(format!("timeout_s must be between 1 and {}, got {}", MAX_TIMEOUT_S, s))),
    }
}

/// cgx puts the script name into a shell command on `exit` and `save` (`mv <name> ...`), so only
/// plain names are accepted.
fn safe_script_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['-', '.'])
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Validate the input, screen the script and its reads, and return the top script (name, bytes,
/// directory reads resolve against, display label) and the copy plan. Nothing is written.
fn screen(roots: &Roots, p: &CgxRunIn) -> Result<(String, Vec<u8>, PathBuf, Plan), ToolError> {
    let (name, text, base, label) = match (&p.script, &p.script_path) {
        (Some(_), Some(_)) | (None, None) => return Err(ToolError::invalid("give exactly one of script and script_path")),
        (Some(s), None) => {
            if s.len() > MAX_INLINE_BYTES {
                return Err(ToolError::invalid(format!("script is {} bytes; the limit is {} (64 KiB)", s.len(), MAX_INLINE_BYTES))
                    .hint("Write the script to a .fbd file inside the root and pass script_path."));
            }
            (INLINE_NAME.to_string(), s.as_bytes().to_vec(), roots.first().to_path_buf(), "script".to_string())
        }
        (None, Some(raw)) => {
            let path = roots.existing(raw, &["fbd", "fbl"])?;
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if !safe_script_name(&name) {
                return Err(ToolError::invalid(format!("script file name '{}' may only contain letters, digits, '_', '-' and '.'", name)));
            }
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if size > MAX_SCANNED_BYTES {
                return Err(ToolError::invalid(format!("{} is larger than {} MiB", raw, MAX_SCANNED_BYTES >> 20)));
            }
            let text = std::fs::read(&path).map_err(ToolError::internal)?;
            let base = path.parent().map(Path::to_path_buf).unwrap_or_else(|| roots.first().to_path_buf());
            (name, text, base, roots.display(&path))
        }
    };
    let mut plan = Plan::default();
    plan_reads(roots, &base, &name, &label, &text, ReadKind::Commands, &mut Vec::new(), &mut plan)?;
    Ok((name, text, base, plan))
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Create `<root>/.calculix-mcp/cgx/<run_id>/` with a private `tmp/`.
fn new_run_dir(root: &Path) -> Result<(String, PathBuf), ToolError> {
    let base = root.join(".calculix-mcp").join("cgx");
    std::fs::create_dir_all(&base).map_err(ToolError::internal)?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    for salt in 0..100u32 {
        let id = format!("g{}-{:04x}", now_secs(), (nanos ^ std::process::id()).wrapping_add(salt) & 0xffff);
        let dir = base.join(&id);
        if std::fs::create_dir(&dir).is_ok() {
            std::fs::create_dir(dir.join("tmp")).map_err(ToolError::internal)?;
            return Ok((id, dir));
        }
    }
    Err(ToolError::internal("could not allocate a cgx run id"))
}

/// Size and modification time of every file under `dir`, except the server's own files.
fn snapshot(dir: &Path) -> HashMap<PathBuf, (u64, Option<SystemTime>)> {
    let mut out = HashMap::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let path = entry.path();
            if d == dir && OWN_FILES.iter().any(|o| entry.file_name() == *o) {
                continue;
            }
            let Ok(md) = std::fs::symlink_metadata(&path) else { continue };
            if md.is_dir() {
                pending.push(path);
            } else if md.is_file() {
                out.insert(path, (md.len(), md.modified().ok()));
            }
        }
    }
    out
}

fn stage(roots: &Roots, p: CgxRunIn) -> Result<Staged, ToolError> {
    let timeout = validate_timeout(p.timeout_s)?;
    let (name, text, _base, plan) = screen(roots, &p)?;
    let (run_id, run_dir) = new_run_dir(roots.first())?;
    std::fs::write(run_dir.join(&name), &text).map_err(ToolError::internal)?;
    let mut copied = Vec::new();
    if p.script_path.is_some() {
        copied.push(p.script_path.clone().unwrap_or_default());
    }
    for (source, rel) in &plan.files {
        let dest = run_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(ToolError::internal)?;
        }
        std::fs::copy(source, &dest).map_err(|e| ToolError::internal(format!("copy {}: {}", roots.display(source), e)))?;
        copied.push(roots.display(source));
    }
    let before = snapshot(&run_dir);
    Ok(Staged { run_id, run_dir, script_name: name, copied, timeout, before })
}

// ---------- output ----------

#[derive(Debug, Serialize, JsonSchema)]
pub struct FileOut {
    /// Relative to the server root
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SetOut {
    /// Upper-case, as in the file (cgx prefixes N for node sets, E for element sets, S for surfaces)
    pub name: String,
    /// node | element | surface
    pub kind: String,
    /// Nodes, elements or element faces
    pub size: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MeshSummary {
    /// Relative to the server root
    pub file: String,
    pub nodes: usize,
    pub elements: usize,
    /// Element TYPE= label -> count
    pub element_types: BTreeMap<String, usize>,
    /// Null when the file has no nodes
    pub bounding_box: Option<BBox>,
    /// Node sets, element sets and element-face surfaces defined in the file, by name
    pub sets: Vec<SetOut>,
    /// Problems the reader found (at most 5)
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CgxRunOut {
    pub run_id: String,
    /// Relative to the server root; cgx's output is in stdout.log and stderr.log there
    pub run_dir: String,
    /// SUCCEEDED (cgx exited 0; still check errors) | FAILED | TIMED_OUT
    pub status: String,
    /// EXIT_CODE | SIGNAL | SPAWN | DISK_QUOTA | TIMEOUT, with detail
    pub failure: Option<String>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub elapsed_s: f64,
    /// Input files copied into the run directory, relative to the root
    pub copied: Vec<String>,
    /// New or changed files in the run directory, sorted by path (at most 200)
    pub files: Vec<FileOut>,
    pub files_total: usize,
    /// One summary per produced .msh, .inp, .nam or .sur file (at most 20)
    pub meshes: Vec<MeshSummary>,
    /// cgx output lines containing ERROR (at most 20). cgx exits 0 even when commands fail
    pub errors: Vec<String>,
    /// The last 4 KiB of cgx output (stdout, then stderr)
    pub output_tail: String,
    pub next: String,
}

fn summarize(roots: &Roots, path: &Path) -> MeshSummary {
    let file = roots.display(path);
    match read_inp_mesh(path) {
        Err(e) => MeshSummary {
            file,
            nodes: 0,
            elements: 0,
            element_types: BTreeMap::new(),
            bounding_box: None,
            sets: Vec::new(),
            warnings: vec![e.to_string()],
        },
        Ok(mesh) => {
            let mut element_types = BTreeMap::new();
            for t in &mesh.element_types {
                *element_types.entry(t.clone()).or_insert(0usize) += 1;
            }
            let bounding_box = (!mesh.nodes.is_empty()).then(|| {
                let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
                for n in &mesh.nodes {
                    for (k, v) in [n.x, n.y, n.z].into_iter().enumerate() {
                        lo[k] = lo[k].min(v);
                        hi[k] = hi[k].max(v);
                    }
                }
                BBox { min: lo, max: hi }
            });
            let mut sets: Vec<SetOut> = Vec::new();
            for s in mesh.sets.values() {
                if !s.node_ids.is_empty() {
                    sets.push(SetOut { name: s.name.to_uppercase(), kind: "node".into(), size: s.node_ids.len() });
                }
                if !s.element_ids.is_empty() {
                    sets.push(SetOut { name: s.name.to_uppercase(), kind: "element".into(), size: s.element_ids.len() });
                }
            }
            for (name, faces) in &mesh.surfaces {
                sets.push(SetOut { name: name.clone(), kind: "surface".into(), size: faces.len() });
            }
            sets.sort_by(|a, b| (&a.name, &a.kind).cmp(&(&b.name, &b.kind)));
            let warnings = mesh.warnings.iter().take(5).map(|w| w.replace(&format!("{}/", roots.first().display()), "")).collect();
            MeshSummary { file, nodes: mesh.nodes.len(), elements: mesh.elements.len(), element_types, bounding_box, sets, warnings }
        }
    }
}

/// Last `max` bytes of a file as text, and the file's full length.
fn tail_of(path: &Path, max: u64) -> (String, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return (String::new(), 0) };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(max);
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_ok() {
        let _ = f.take(max).read_to_end(&mut buf);
    }
    (String::from_utf8_lossy(&buf).into_owned(), len)
}

/// The last `TAIL_BYTES` of stdout then stderr, with a marker when anything was cut.
fn output_tail(run_dir: &Path) -> String {
    let (out, out_len) = tail_of(&run_dir.join("stdout.log"), TAIL_BYTES);
    let (err, err_len) = tail_of(&run_dir.join("stderr.log"), TAIL_BYTES);
    let mut text = out;
    if err_len > 0 {
        text.push_str("\n--- stderr ---\n");
        text.push_str(&err);
    }
    let total = out_len + err_len + if err_len > 0 { 14 } else { 0 };
    let mut cut = text.len().saturating_sub(TAIL_BYTES as usize);
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    let kept = &text[cut..];
    let dropped = total.saturating_sub(kept.len() as u64);
    if dropped > 0 {
        format!("[... {} bytes of earlier output truncated ...]\n{}", dropped, kept)
    } else {
        kept.to_string()
    }
}

/// cgx output lines that report an error.
fn error_lines(run_dir: &Path) -> Vec<String> {
    use std::io::BufRead;
    let mut found = Vec::new();
    for name in ["stdout.log", "stderr.log"] {
        let Ok(file) = std::fs::File::open(run_dir.join(name)) else { continue };
        let mut reader = std::io::BufReader::new(file);
        let mut buf = Vec::new();
        while found.len() < MAX_ERROR_LINES {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    if line.contains("ERROR") {
                        found.push(line.trim().chars().take(300).collect());
                    }
                }
            }
        }
    }
    found
}

fn is_mesh_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| ["msh", "inp", "nam", "sur"].iter().any(|x| x.eq_ignore_ascii_case(e)))
}

struct Ending {
    status: &'static str,
    failure: Option<String>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    elapsed: Duration,
}

fn report(roots: &Roots, s: &Staged, end: Ending) -> CgxRunOut {
    let after = snapshot(&s.run_dir);
    let mut changed: Vec<(&PathBuf, u64)> =
        after.iter().filter(|(p, meta)| s.before.get(*p) != Some(meta)).map(|(p, (len, _))| (p, *len)).collect();
    changed.sort();
    let meshes: Vec<MeshSummary> =
        changed.iter().filter(|(p, _)| is_mesh_file(p)).take(MAX_MESH_SUMMARIES).map(|(p, _)| summarize(roots, p)).collect();
    let files: Vec<FileOut> = changed.iter().take(MAX_FILES_LISTED).map(|(p, len)| FileOut { path: roots.display(p), bytes: *len }).collect();
    let errors = error_lines(&s.run_dir);

    let mut next: Vec<String> = Vec::new();
    match end.status {
        "TIMED_OUT" => next.push("cgx did not finish in time: raise timeout_s, or look for a while loop that never ends.".into()),
        "FAILED" => next.push("cgx failed: read errors and output_tail, fix the script and run again.".into()),
        _ => {}
    }
    if !errors.is_empty() {
        next.push("cgx reported errors (it exits 0 anyway): fix the lines they name; call cgx_help for syntax.".into());
    }
    if let Some(mesh) = meshes.iter().find(|m| m.elements > 0) {
        if mesh.file.to_ascii_lowercase().ends_with(".inp") {
            next.push(format!("Call mesh_check on {}.", mesh.file));
        } else {
            next.push(format!(
                "Check the mesh: write a deck that includes {} (*INCLUDE, INPUT=...) with *MATERIAL and *SOLID SECTION, ELSET=Eall (cgx_help topic 'send' has one), then call mesh_check on the deck.",
                mesh.file
            ));
        }
    } else if end.status == "SUCCEEDED" && changed.is_empty() {
        next.push("The script wrote no files: end it with 'send all abq' to export the mesh.".into());
    }

    CgxRunOut {
        run_id: s.run_id.clone(),
        run_dir: roots.display(&s.run_dir),
        status: end.status.into(),
        failure: end.failure,
        exit_code: end.exit_code,
        signal: end.signal,
        elapsed_s: (end.elapsed.as_secs_f64() * 1000.0).round() / 1000.0,
        copied: s.copied.clone(),
        files_total: changed.len(),
        files,
        meshes,
        errors,
        output_tail: output_tail(&s.run_dir),
        next: next.join(" "),
    }
}

// ---------- running ----------

/// Kills the run's process group if the tool call is dropped (client cancel, server shutdown).
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        kill_group(self.0);
    }
}

enum Outcome {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    DiskQuota(u64, u64),
}

/// Run cgx on a staged directory and wait for it, killing its process group on timeout.
async fn execute(cfg: &CgxConfig, s: &Staged) -> Ending {
    let started = Instant::now();
    let open = |name: &str| std::fs::File::create(s.run_dir.join(name));
    let (stdout, stderr) = match (open("stdout.log"), open("stderr.log")) {
        (Ok(o), Ok(e)) => (o, e),
        _ => {
            return Ending { status: "FAILED", failure: Some("SPAWN: cannot create log files".into()), exit_code: None, signal: None, elapsed: Duration::ZERO }
        }
    };
    let mut cmd = isolated_command(&cfg.binary, &s.run_dir, &s.run_dir.join("tmp"), &[], &cfg.env_passthrough);
    cmd.arg("-bg").arg(&s.script_name).stdout(stdout).stderr(stderr);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ending {
                status: "FAILED",
                failure: Some(format!("SPAWN: {}: {}", cfg.binary.display(), e)),
                exit_code: None,
                signal: None,
                elapsed: started.elapsed(),
            }
        }
    };
    let pid = child.id();
    let _guard = GroupGuard(pid);
    let outcome = tokio::select! {
        status = child.wait() => Outcome::Exited(status),
        _ = tokio::time::sleep(s.timeout) => Outcome::TimedOut,
        (size, limit) = over_quota(s.run_dir.clone(), cfg.max_run_bytes, Duration::from_secs(2)) => Outcome::DiskQuota(size, limit),
    };
    let ending = |status, failure, (exit_code, signal): (Option<i32>, Option<i32>)| Ending {
        status,
        failure,
        exit_code,
        signal,
        elapsed: started.elapsed(),
    };
    match outcome {
        Outcome::Exited(Ok(status)) => {
            let (code, signal) = exit_parts(status);
            let failure = match (code, signal) {
                (_, Some(sig)) => Some(format!("SIGNAL: killed by signal {}", sig)),
                (Some(0), None) => None,
                (c, None) => Some(format!("EXIT_CODE: cgx exited with {}", c.unwrap_or(-1))),
            };
            ending(if failure.is_none() { "SUCCEEDED" } else { "FAILED" }, failure, (code, signal))
        }
        Outcome::Exited(Err(e)) => ending("FAILED", Some(format!("SPAWN: wait failed: {}", e)), (None, None)),
        Outcome::TimedOut | Outcome::DiskQuota(..) => {
            let parts = terminate_group(&mut child, pid, cfg.grace).await.map(exit_parts).unwrap_or((None, None));
            match outcome {
                Outcome::DiskQuota(size, limit) => ending(
                    "FAILED",
                    Some(format!("DISK_QUOTA: run directory reached {} MiB, limit {} MiB", size >> 20, limit >> 20)),
                    parts,
                ),
                _ => ending("TIMED_OUT", Some(format!("TIMEOUT: exceeded {} s", s.timeout.as_secs())), parts),
            }
        }
    }
}

/// The `cgx_run_batch` tool: screen, stage, run, report.
pub async fn run_batch(roots: Arc<Roots>, cfg: Option<Arc<CgxConfig>>, p: CgxRunIn) -> Result<CgxRunOut, ToolError> {
    let cfg = cfg.ok_or_else(|| {
        ToolError::new("TOOL_DISABLED", "no CalculiX GraphiX binary is configured")
            .hint("Start the server with --cgx <absolute path to the cgx binary>.")
    })?;
    let r = roots.clone();
    let staged = tokio::task::spawn_blocking(move || stage(&r, p)).await.map_err(ToolError::internal)??;
    let end = execute(&cfg, &staged).await;
    tokio::task::spawn_blocking(move || report(&roots, &staged, end)).await.map_err(ToolError::internal)
}

// ---------- help ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CgxHelpIn {
    /// Case-insensitive text to look for, e.g. "swep", "send", "sets". Sections whose title matches are returned; if no title matches, sections whose text does. Default: everything
    pub topic: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct HelpSection {
    pub title: String,
    /// Markdown; every fbd block was run with cgx and produced what its "# expect:" lines say
    pub text: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CgxHelpOut {
    pub sections: Vec<HelpSection>,
    /// Titles of all sections
    pub topics: Vec<String>,
}

/// The cheat sheet split at its `## ` headings; the text before the first one is "About".
fn sections() -> Vec<HelpSection> {
    let mut out: Vec<HelpSection> = vec![HelpSection { title: "About".into(), text: String::new() }];
    for line in CHEATSHEET.lines() {
        if let Some(title) = line.strip_prefix("## ") {
            out.push(HelpSection { title: title.trim().to_string(), text: String::new() });
        } else if let Some(last) = out.last_mut() {
            last.text.push_str(line);
            last.text.push('\n');
        }
    }
    for s in &mut out {
        s.text = s.text.trim().to_string();
    }
    out
}

pub fn help(p: CgxHelpIn) -> CgxHelpOut {
    let all = sections();
    let topics = all.iter().map(|s| s.title.clone()).collect();
    let wanted = p.topic.map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty());
    let sections = match wanted {
        None => all,
        Some(t) => {
            let (titled, rest): (Vec<HelpSection>, Vec<HelpSection>) = all.into_iter().partition(|s| s.title.to_lowercase().contains(&t));
            if titled.is_empty() {
                rest.into_iter().filter(|s| s.text.to_lowercase().contains(&t)).collect()
            } else {
                titled
            }
        }
    };
    CgxHelpOut { sections, topics }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every ```fbd block of the cheat sheet.
    fn examples() -> Vec<String> {
        let mut out = Vec::new();
        let mut cur: Option<String> = None;
        for line in CHEATSHEET.lines() {
            match (&mut cur, line.trim()) {
                (None, "```fbd") => cur = Some(String::new()),
                (Some(_), "```") => out.extend(cur.take()),
                (Some(c), _) => {
                    c.push_str(line);
                    c.push('\n');
                }
                _ => {}
            }
        }
        out
    }

    fn sandbox(tag: &str) -> (PathBuf, Roots) {
        let dir = std::env::temp_dir().join(format!("calculix-mcp-cgx-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let roots = Roots::new(vec![dir.clone()]).unwrap();
        (dir, roots)
    }

    fn scan(text: &str) -> Result<Vec<ReadRef>, ToolError> {
        scan_commands("t.fbd", text.as_bytes())
    }

    fn rejects(text: &str, wanted: &str) {
        let e = scan(text).expect_err(text);
        assert_eq!(e.code, "INVALID_ARGUMENT");
        assert!(e.message.contains(wanted), "{:?} -> {}", text, e.message);
    }

    #[test]
    fn sys_is_rejected_in_every_spelling() {
        rejects("pnt p1 0 0 0\nsys ls\n", "t.fbd:2: 'sys'");
        rejects("SyS ls", "t.fbd:1: 'sys'");
        rejects("  \t sys rm -rf x", "'sys'");
        // cgx joins a line ending in '=' with the next one, and ends lines at a lone CR.
        rejects("pnt p1 0 0 0\ns=\nys ls\n", "t.fbd:2: 'sys'");
        rejects("pnt p1 0 0 0\rsys ls", "t.fbd:2: 'sys'");
        rejects("pnt p1 0 0 0\r\n\r\nsys ls", "t.fbd:3: 'sys'");
        // Not a keyword: a comment, an argument, a longer word.
        assert!(scan("# sys ls\n! sys\nprnt sys\nsystem\n").is_ok());
        assert_eq!(scan("pnt a 0 0 0\0sys").unwrap_err().code, "INVALID_ARGUMENT");
    }

    #[test]
    fn programs_and_escaping_paths_are_rejected() {
        rejects("graph all time", "'graph'");
        rejects("movie start x.mp4", "'movi'");
        rejects("movi frames 2", "'movi'");
        rejects("asgn viewer evince", "'asgn viewer'");
        rejects("valu name ?", "keyboard");
        rejects("valu p & . .", "concatenating '.'");
        rejects("valu p & / etc", "concatenating '/'");
        rejects("read /etc/passwd", "'/etc/passwd' is an absolute path");
        rejects("read ../x.fbd", "contains '..'");
        rejects("read ~/x.fbd", "'~'");
        rejects("read C:\\x.fbd", "Windows");
        rejects("send ../all abq", "contains '..'");
        rejects("send all abq\nhcpy /tmp/shot", "t.fbd:2: parameter '/tmp/shot'");
        rejects("move /x tra 1 0 0", "absolute");
        rejects("seta a;touch n 1", "metacharacter");
        rejects("seta a&touch n 1", "metacharacter");
        rejects("seta a>b n 1", "metacharacter");
        rejects("seta a$(x) n 1", "metacharacter");
        rejects("read foam1 foam", "OpenFOAM");
        rejects("valu f x.fbd\nread f", "names a value");
        // Operators and the division by "/" stay usable.
        assert!(scan("valu a 1\nvalu b & a c\nif a < 2\nendif\nwhile a != 3\nendwhile\nds 1 / 2\n").is_ok());
        assert!(scan("save\nexit\nhcpy shot\nmove all tra 1 0 0\n").is_ok());
    }

    #[test]
    fn every_cheatsheet_example_passes_the_scan() {
        let all = examples();
        assert!(all.len() >= 12, "found {} examples", all.len());
        for ex in all {
            assert!(scan(&ex).is_ok(), "{}", ex);
        }
    }

    #[test]
    fn reads_are_classified_like_cgx() {
        let refs = scan("read a.fbd\nread b.fbl add\nread m.inp\nread n.msh inp\nread r.frd\nread s.stl\nread l.txt -n\nread t.txt stack\nread c\n").unwrap();
        let kinds: Vec<(&str, ReadKind)> = refs.iter().map(|r| (r.path.as_str(), r.kind)).collect();
        use ReadKind::*;
        assert_eq!(
            kinds,
            [("a.fbd", Commands), ("b.fbl", Commands), ("m.inp", Deck), ("n.msh", Deck), ("r.frd", Data), ("s.stl", Data), ("l.txt", Data), ("t.txt", Data), ("c", Commands)]
        );
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn input(path: &str) -> CgxRunIn {
        CgxRunIn { script: None, script_path: Some(path.into()), timeout_s: None }
    }

    #[test]
    fn nested_reads_are_screened_and_planned() {
        let (dir, roots) = sandbox("nested");
        write(&dir, "m/main.fbd", "read parts/a.fbl\nread mesh.inp\nread r.frd\nsend all abq\n");
        write(&dir, "m/parts/a.fbl", "# a\nread parts/b.fbl\n");
        write(&dir, "m/parts/b.fbl", "pnt p 0 0 0\n");
        write(&dir, "m/mesh.inp", "*INCLUDE, INPUT=nodes.inc\n*ELSET, ELSET=E1\n1\n");
        write(&dir, "m/nodes.inc", "*NODE\n1, 0, 0, 0\n");
        write(&dir, "m/r.frd", "x");
        let (name, _, _, plan) = screen(&roots, &input("m/main.fbd")).unwrap();
        assert_eq!(name, "main.fbd");
        let rels: Vec<String> = plan.files.iter().map(|(_, r)| r.display().to_string().replace('\\', "/")).collect();
        assert_eq!(rels, ["parts/a.fbl", "parts/b.fbl", "mesh.inp", "nodes.inc", "r.frd"]);

        // A forbidden line two levels down is reported with its own file and line.
        write(&dir, "m/parts/b.fbl", "pnt p 0 0 0\nsys ls\n");
        let e = screen(&roots, &input("m/main.fbd")).unwrap_err();
        assert!(e.message.starts_with("parts/b.fbl:2: 'sys'"), "{}", e.message);

        // Deck set names become file names in cgx: they are screened too.
        write(&dir, "m/parts/b.fbl", "pnt p 0 0 0\n");
        write(&dir, "m/mesh.inp", "*NSET, NSET=/tmp/evil\n1\n");
        let e = screen(&roots, &input("m/main.fbd")).unwrap_err();
        assert!(e.message.starts_with("mesh.inp:1: NSET=/tmp/evil"), "{}", e.message);
        write(&dir, "m/mesh.inp", "*INCLUDE, INPUT=../outside.inc\n");
        assert!(screen(&roots, &input("m/main.fbd")).unwrap_err().message.contains("'..'"));

        // Missing file, and a cycle.
        write(&dir, "m/mesh.inp", "*NODE\n1,0,0,0\n");
        write(&dir, "m/parts/b.fbl", "read nope.fbl\n");
        let e = screen(&roots, &input("m/main.fbd")).unwrap_err();
        assert_eq!(e.code, "FILE_NOT_FOUND");
        assert!(e.message.starts_with("parts/b.fbl:1:"), "{}", e.message);
        write(&dir, "m/parts/b.fbl", "read parts/a.fbl\n");
        assert!(screen(&roots, &input("m/main.fbd")).unwrap_err().message.contains("reads itself"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_depth_is_limited_to_eight() {
        let (dir, roots) = sandbox("depth");
        let chain = |n: usize| {
            for k in 0..n {
                write(&dir, &format!("f{}.fbl", k), &format!("read f{}.fbl\n", k + 1));
            }
            write(&dir, &format!("f{}.fbl", n), "pnt p 0 0 0\n");
        };
        chain(8); // f0 reads f1 ... f8: eight nested reads
        assert!(screen(&roots, &input("f0.fbl")).is_ok());
        chain(9);
        let e = screen(&roots, &input("f0.fbl")).unwrap_err();
        assert!(e.message.contains("nested deeper than 8"), "{}", e.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_read_through_a_symlink_out_of_the_roots_is_rejected() {
        let (dir, _) = sandbox("symlink");
        write(&dir, "outside.fbl", "pnt p 0 0 0\n");
        write(&dir, "root/main.fbd", "read link.fbl\n");
        std::os::unix::fs::symlink(dir.join("outside.fbl"), dir.join("root/link.fbl")).unwrap();
        let roots = Roots::new(vec![dir.join("root")]).unwrap();
        assert_eq!(screen(&roots, &input("main.fbd")).unwrap_err().code, "PATH_OUTSIDE_ROOTS");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn input_is_validated() {
        let (dir, roots) = sandbox("input");
        let inline = |s: &str| CgxRunIn { script: Some(s.into()), script_path: None, timeout_s: None };
        let code = |p: CgxRunIn| screen(&roots, &p).unwrap_err().code;
        assert_eq!(code(CgxRunIn { script: None, script_path: None, timeout_s: None }), "INVALID_ARGUMENT");
        assert_eq!(code(CgxRunIn { script: Some("x".into()), script_path: Some("a.fbd".into()), timeout_s: None }), "INVALID_ARGUMENT");
        assert_eq!(code(inline(&"#".repeat(MAX_INLINE_BYTES + 1))), "INVALID_ARGUMENT");
        assert!(screen(&roots, &inline(&format!("#{}", "x".repeat(MAX_INLINE_BYTES - 1)))).is_ok());
        write(&dir, "deck.inp", "*NODE\n");
        assert_eq!(code(input("deck.inp")), "BAD_EXTENSION");
        assert_eq!(code(input("missing.fbd")), "FILE_NOT_FOUND");
        write(&dir, "bad name.fbd", "pnt p 0 0 0\n");
        assert_eq!(code(input("bad name.fbd")), "INVALID_ARGUMENT");
        // Inline scripts read relative to the root.
        write(&dir, "geo.fbl", "pnt p 0 0 0\n");
        let (name, _, _, plan) = screen(&roots, &inline("read geo.fbl\n")).unwrap();
        assert_eq!((name.as_str(), plan.files.len()), ("script.fbd", 1));

        assert_eq!(validate_timeout(None).unwrap(), Duration::from_secs(300));
        assert_eq!(validate_timeout(Some(3600)).unwrap(), Duration::from_secs(3600));
        assert_eq!(validate_timeout(Some(0)).unwrap_err().code, "INVALID_ARGUMENT");
        assert_eq!(validate_timeout(Some(3601)).unwrap_err().code, "INVALID_ARGUMENT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn without_a_binary_the_tool_is_disabled() {
        let (dir, roots) = sandbox("disabled");
        let p = CgxRunIn { script: Some("pnt p 0 0 0".into()), script_path: None, timeout_s: None };
        let e = run_batch(Arc::new(roots), None, p).await.unwrap_err();
        assert_eq!(e.code, "TOOL_DISABLED");
        assert!(e.hint.unwrap().contains("--cgx"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn help_filters_by_title_then_text() {
        let all = help(CgxHelpIn { topic: None });
        assert!(all.topics.len() >= 10);
        assert_eq!(all.sections.len(), all.topics.len());
        let swep = help(CgxHelpIn { topic: Some("SWEEP".into()) });
        assert_eq!(swep.sections.len(), 1);
        assert!(swep.sections[0].text.contains("swep base top tra 0 0 10 4"));
        let enq = help(CgxHelpIn { topic: Some("enq".into()) });
        assert!(enq.sections.iter().any(|s| s.title.starts_with("Sets")));
        assert!(help(CgxHelpIn { topic: Some("no such thing".into()) }).sections.is_empty());
    }

    // ---------- process handling, with a fake cgx ----------

    #[cfg(unix)]
    mod process {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        const FAKE_CGX: &str = r##"#!/bin/sh
# Fake cgx: "$1" is -bg, "$2" the script; behaviour from a "# MODE=<mode>" line in it.
mode=$(sed -n 's/^# MODE=//p' "$2" | head -1)
echo "args=$* home=$HOME"
case "$mode" in
  mesh) printf '*NODE, NSET=Nall\n1,0,0,0\n2,1,0,0\n3,0,1,0\n4,0,0,1\n*ELEMENT, TYPE=C3D4, ELSET=Eall\n1,1,2,3,4\n' > all.msh
        printf '*NSET,NSET=Nfix\n1,\n2,\n' > fix.nam; echo ready ;;
  error) echo " ERROR in senddata, set:nope does not exist"; exit 0 ;;
  fail) echo "starting"; echo "boom" >&2; exit 3 ;;
  hang) (trap '' TERM; exec sleep 300) & echo $! > child.pid; trap '' TERM; wait ;;
  flood) i=0; while [ $i -lt 400 ]; do echo "line $i of a long and chatty output ............................"; i=$((i+1)); done ;;
esac
"##;

        fn fake(dir: &Path) -> Arc<CgxConfig> {
            let bin = dir.join("fake_cgx");
            std::fs::write(&bin, FAKE_CGX).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            Arc::new(CgxConfig { binary: bin, env_passthrough: Vec::new(), max_run_bytes: 1 << 30, grace: Duration::from_millis(300) })
        }

        async fn run(dir: &Path, roots: &Roots, mode: &str, timeout_s: u64) -> CgxRunOut {
            let p = CgxRunIn { script: Some(format!("# MODE={}\npnt p 0 0 0\n", mode)), script_path: None, timeout_s: Some(timeout_s) };
            run_batch(Arc::new(roots.clone()), Some(fake(dir)), p).await.unwrap()
        }

        fn alive(pid: u32) -> bool {
            crate::jobs::alive(pid)
        }

        #[tokio::test]
        async fn produced_files_are_listed_and_summarised() {
            let (dir, roots) = sandbox("proc-mesh");
            let out = run(&dir, &roots, "mesh", 30).await;
            assert_eq!((out.status.as_str(), out.exit_code, out.failure.as_deref()), ("SUCCEEDED", Some(0), None));
            let files: Vec<&str> = out.files.iter().map(|f| f.path.as_str()).collect();
            let rd = &out.run_dir;
            assert_eq!(files, [format!("{}/all.msh", rd), format!("{}/fix.nam", rd)], "the unchanged script and the logs are not listed");
            assert!(out.run_dir.starts_with(".calculix-mcp/cgx/g"));
            let msh = &out.meshes[0];
            assert_eq!((msh.nodes, msh.elements, msh.element_types.get("C3D4")), (4, 1, Some(&1)));
            assert_eq!(msh.bounding_box.as_ref().unwrap().max, [1.0, 1.0, 1.0]);
            let fix = &out.meshes[1];
            assert_eq!((fix.sets[0].name.as_str(), fix.sets[0].kind.as_str(), fix.sets[0].size), ("NFIX", "node", 2));
            assert!(out.next.contains("mesh_check"), "{}", out.next);
            // Arguments and isolation, as seen by the child.
            let home = dir.join(&out.run_dir).join("tmp");
            assert!(out.output_tail.contains(&format!("args=-bg script.fbd home={}", home.display())), "{}", out.output_tail);
            assert!(out.errors.is_empty());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn errors_and_non_zero_exit_are_reported() {
            let (dir, roots) = sandbox("proc-fail");
            let err = run(&dir, &roots, "error", 30).await;
            assert_eq!(err.status, "SUCCEEDED");
            assert_eq!(err.errors, ["ERROR in senddata, set:nope does not exist"]);
            assert!(err.next.contains("reported errors"));

            let fail = run(&dir, &roots, "fail", 30).await;
            assert_eq!((fail.status.as_str(), fail.exit_code), ("FAILED", Some(3)));
            assert!(fail.failure.unwrap().starts_with("EXIT_CODE"));
            assert!(fail.output_tail.contains("starting\n\n--- stderr ---\nboom"), "{}", fail.output_tail);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn timeout_kills_the_whole_group() {
            let (dir, roots) = sandbox("proc-hang");
            let out = run(&dir, &roots, "hang", 1).await;
            assert_eq!(out.status, "TIMED_OUT");
            assert!(out.failure.unwrap().starts_with("TIMEOUT"));
            assert!(out.elapsed_s >= 1.0 && out.elapsed_s < 10.0, "{}", out.elapsed_s);
            let pid: u32 = std::fs::read_to_string(dir.join(&out.run_dir).join("child.pid")).unwrap().trim().parse().unwrap();
            for _ in 0..40 {
                if !alive(pid) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(!alive(pid), "grandchild that ignored SIGTERM is still alive");
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn long_output_is_cut_to_a_marked_tail() {
            let (dir, roots) = sandbox("proc-flood");
            let out = run(&dir, &roots, "flood", 30).await;
            assert!(out.output_tail.starts_with("[... "), "{}", &out.output_tail[..80]);
            assert!(out.output_tail.len() < TAIL_BYTES as usize + 80);
            assert!(out.output_tail.trim_end().ends_with("line 399 of a long and chatty output ............................"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[tokio::test]
        async fn a_rejected_script_never_starts() {
            let (dir, roots) = sandbox("proc-reject");
            let p = CgxRunIn { script: Some("# MODE=mesh\nsys touch pwned\n".into()), script_path: None, timeout_s: None };
            let e = run_batch(Arc::new(roots.clone()), Some(fake(&dir)), p).await.unwrap_err();
            assert_eq!(e.code, "INVALID_ARGUMENT");
            assert!(!dir.join(".calculix-mcp/cgx").exists(), "no run directory is created");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // ---------- the real cgx (run with: cargo test -p calculix-mcp -- --ignored cgx) ----------

    fn real_cgx() -> Option<Arc<CgxConfig>> {
        let bin = PathBuf::from(std::env::var("CGX_BIN").unwrap_or_else(|_| "/Users/carlo/code/CalculiX/cgx/bin/cgx_glfw".into()));
        if !bin.is_file() {
            eprintln!("skipped: no cgx binary at {} (set CGX_BIN)", bin.display());
            return None;
        }
        Some(Arc::new(CgxConfig { binary: bin, env_passthrough: Vec::new(), max_run_bytes: 1 << 30, grace: Duration::from_secs(2) }))
    }

    async fn run_real(roots: &Roots, cfg: &Arc<CgxConfig>, script: &str) -> CgxRunOut {
        let p = CgxRunIn { script: Some(script.into()), script_path: None, timeout_s: Some(120) };
        run_batch(Arc::new(roots.clone()), Some(cfg.clone()), p).await.unwrap()
    }

    fn mesh<'a>(out: &'a CgxRunOut, file: &str) -> &'a MeshSummary {
        out.meshes.iter().find(|m| m.file.ends_with(&format!("/{}", file))).unwrap_or_else(|| panic!("no {} in {:?}", file, out.files))
    }

    fn set_size(m: &MeshSummary, name: &str) -> Option<usize> {
        m.sets.iter().find(|s| s.name == name).map(|s| s.size)
    }

    /// The minimal deck of the cheat sheet (the plain ``` block after the export example).
    fn cheatsheet_deck() -> String {
        let start = CHEATSHEET.find("*INCLUDE, INPUT=all.msh").unwrap();
        let end = start + CHEATSHEET[start..].find("```").unwrap();
        CHEATSHEET[start..end].to_string()
    }

    /// Check one `# expect: <file> key=value ...` line against a run.
    fn check_expectation(out: &CgxRunOut, line: &str) {
        let mut parts = line.split_whitespace();
        let file = parts.next().unwrap();
        let listed = out.files.iter().any(|f| f.path.ends_with(&format!("/{}", file)));
        assert!(listed, "{} was not produced: {:?}\n{}", file, out.files, out.output_tail);
        for check in parts {
            if check == "exists" {
                continue;
            }
            let m = mesh(out, file);
            if let Some((key, min)) = check.split_once('>') {
                let n = *m.element_types.get(key).unwrap_or(&0);
                assert!(n > min.parse::<usize>().unwrap(), "{} {}: {} has {}", file, check, key, n);
                continue;
            }
            let (key, value) = check.split_once('=').unwrap();
            match key {
                "nodes" => assert_eq!(m.nodes.to_string(), value, "{} nodes", file),
                "elements" => assert_eq!(m.elements.to_string(), value, "{} elements", file),
                "bbox" => {
                    let want: Vec<f64> = value.split(',').map(|v| v.parse().unwrap()).collect();
                    let bb = m.bounding_box.as_ref().unwrap();
                    let got: Vec<f64> = bb.min.iter().chain(bb.max.iter()).copied().collect();
                    for (g, w) in got.iter().zip(&want) {
                        assert!((g - w).abs() < 1e-6, "{} bbox {:?} != {:?}", file, got, want);
                    }
                }
                k if m.element_types.contains_key(k) => assert_eq!(m.element_types[k].to_string(), value, "{} {}", file, k),
                k => assert_eq!(set_size(m, k).map(|s| s.to_string()).as_deref(), Some(value), "{} set {}: {:?}", file, k, m.sets),
            }
        }
    }

    #[tokio::test]
    #[ignore = "needs the cgx binary (CGX_BIN)"]
    async fn cgx_real_every_cheatsheet_example_does_what_it_says() {
        let Some(cfg) = real_cgx() else { return };
        let (dir, roots) = sandbox("real-examples");
        for ex in examples() {
            let out = run_real(&roots, &cfg, &ex).await;
            assert_eq!(out.status, "SUCCEEDED", "{}\n{}", ex, out.output_tail);
            assert!(out.errors.is_empty(), "{}\n{:?}", ex, out.errors);
            let expectations: Vec<&str> = ex.lines().filter_map(|l| l.strip_prefix("# expect: ")).collect();
            assert!(!expectations.is_empty(), "example without expectations:\n{}", ex);
            for e in expectations {
                check_expectation(&out, e);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn block_example() -> String {
        examples().into_iter().find(|e| e.contains("nodes=321 elements=40") && e.contains("swep base top tra 0 0 10 4") && !e.contains("enq")).unwrap()
    }

    #[tokio::test]
    #[ignore = "needs the cgx binary (CGX_BIN)"]
    async fn cgx_real_block_he20_passes_mesh_check() {
        let Some(cfg) = real_cgx() else { return };
        let (dir, roots) = sandbox("real-block");
        let out = run_real(&roots, &cfg, &block_example()).await;
        let m = mesh(&out, "all.msh");
        eprintln!("block: {} nodes, {} elements {:?}, {:.3} s", m.nodes, m.elements, m.element_types, out.elapsed_s);
        assert_eq!(m.element_types.get("C3D20"), Some(&40));
        assert_eq!(m.nodes, 321);
        assert!(out.next.contains("mesh_check"));

        // The cheat sheet's deck, without the set files this run did not write.
        let deck: String = cheatsheet_deck().lines().filter(|l| !l.contains("fix.nam") && !l.contains("load.sur")).collect::<Vec<_>>().join("\n");
        let deck = deck.replace("*BOUNDARY\nNfix, 1, 3\n*DLOAD\nSload, P, 1.5\n", "");
        let deck_path = dir.join(&out.run_dir).join("check.inp");
        std::fs::write(&deck_path, deck).unwrap();
        let check = crate::mesh_tools::check(&roots, crate::mesh_tools::MeshCheckIn { path: roots.display(&deck_path) }).unwrap();
        eprintln!("mesh_check: verdict {} volume {} problems {:?}", check.verdict, check.mass.volume, check.problems.items);
        assert_eq!(check.verdict, "OK", "{:?}", check.problems.items);
        assert!((check.mass.volume - 20000.0).abs() / 20000.0 < 1e-6, "volume {}", check.mass.volume);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[ignore = "needs the cgx binary (CGX_BIN)"]
    async fn cgx_real_face_set_for_loading_is_exported() {
        let Some(cfg) = real_cgx() else { return };
        let (dir, roots) = sandbox("real-faces");
        let script = format!(
            "{}seta nodes n all\nenq nodes load rec 100 _ _ 0.01 a\nenq nodes fix rec 0 _ _ 0.01 a\ncomp load f\nsend load abq sur\nsend fix abq nam\n",
            block_example()
        );
        let out = run_real(&roots, &cfg, &script).await;
        let sur = mesh(&out, "load.sur");
        let nam = mesh(&out, "fix.nam");
        eprintln!("faces: load.sur {:?}, fix.nam {:?}", sur.sets.iter().map(|s| (&s.name, &s.kind, s.size)).collect::<Vec<_>>(), nam.sets.iter().map(|s| (&s.name, s.size)).collect::<Vec<_>>());
        assert_eq!(set_size(sur, "SLOAD"), Some(4), "2 x 2 he20 faces at x = 100");
        assert_eq!(sur.sets[0].kind, "surface");
        assert_eq!(set_size(nam, "NFIX"), Some(21), "5 x 5 face nodes minus the 4 face centres");

        // The full deck of the cheat sheet, with the set files, passes mesh_check too.
        let deck_path = dir.join(&out.run_dir).join("check.inp");
        std::fs::write(&deck_path, cheatsheet_deck()).unwrap();
        let check = crate::mesh_tools::check(&roots, crate::mesh_tools::MeshCheckIn { path: roots.display(&deck_path) }).unwrap();
        assert_eq!(check.verdict, "OK", "{:?}", check.problems.items);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[ignore = "needs the cgx binary (CGX_BIN)"]
    async fn cgx_real_sys_is_rejected_before_running() {
        let Some(cfg) = real_cgx() else { return };
        let (dir, roots) = sandbox("real-sys");
        let p = CgxRunIn { script: Some("pnt p1 0 0 0\nsys ls\n".into()), script_path: None, timeout_s: None };
        let e = run_batch(Arc::new(roots.clone()), Some(cfg), p).await.unwrap_err();
        eprintln!("sys: {} {}", e.code, e.message);
        assert_eq!(e.code, "INVALID_ARGUMENT");
        assert!(e.message.starts_with("script:2: 'sys'"));
        assert!(!dir.join(".calculix-mcp/cgx").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
