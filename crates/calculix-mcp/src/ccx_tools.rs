//! Solver tools: `ccx_solve`, `ccx_job_status`, `ccx_job_cancel` (background jobs, see `jobs.rs`)
//! and `ccx_diagnose`, which explains convergence from `<job>.sta` / `<job>.cvg` / `<job>.json`.

use crate::error::ToolError;
use crate::frd_tools::{num, Page, Table};
use crate::paths::Roots;
use crate::jobs::{dir_size, rss_mb, JobManager, Manifest, Prepared, Solver, State, TelemetrySource};
use cgx_core::{consolidate_inp, diagnose, parse_cvg, parse_sta, scan_cards, DiagCtx, ScanLimits, Thresholds};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

const JOB_EXTENSIONS: [&str; 6] = ["inp", "sta", "cvg", "frd", "dat", "json"];

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CcxDiagnoseIn {
    /// Path to the job, with or without extension: "run/beam", "run/beam.inp", "run/beam.sta". Give this or job_id
    pub job_path: Option<String>,
    /// Id returned by ccx_solve. Adds process state and solver output to the diagnosis
    pub job_id: Option<String>,
    /// Step to analyse. Default: all steps
    pub step: Option<u32>,
    /// Number of most recent increments to analyse and list (default 10, max 50)
    pub last_increments: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Coverage {
    pub sta_rows: usize,
    pub cvg_rows: usize,
    /// Steps present in the .sta file
    pub steps: Vec<u32>,
    /// Whether <job>.json (ccx -j telemetry) was found
    pub json: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FindingOut {
    /// Rule id, e.g. C1 (contact chatter), B2 (cutback loop), D1 (divergence)
    pub rule: String,
    /// info | warn | error
    pub severity: String,
    pub step: Option<u32>,
    pub inc: Option<u32>,
    pub att: Option<u32>,
    pub evidence: Map<String, Value>,
    pub occurrences: usize,
    pub advice: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CcxDiagnoseOut {
    /// NO_TELEMETRY | LINEAR | SMOOTH | SLOW | CONTACT_CHATTER | CUTBACK | CUTBACK_LOOP | DIVERGED
    pub verdict: String,
    pub coverage: Coverage,
    pub findings: Vec<FindingOut>,
    /// Most recent completed increments. att > 1 means the increment needed cutbacks
    pub increments: Table,
    pub solver_messages: Vec<String>,
    pub notes: Vec<String>,
}

/// `<dir>/<jobname>` without extension, confined to the roots.
fn job_base(roots: &Roots, raw: &str) -> Result<PathBuf, ToolError> {
    let resolved = roots.creatable(raw, &[])?;
    let known = resolved
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| JOB_EXTENSIONS.iter().any(|k| k.eq_ignore_ascii_case(e)));
    Ok(if known { resolved.with_extension("") } else { resolved })
}

/// Sibling file `<base>.<ext>`, appended rather than substituted so dotted job names survive.
fn with_ext(base: &Path, ext: &str) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

/// `(step, converged)` pairs and the overall success flag from ccx's JSON telemetry.
fn read_json(path: &Path) -> Option<(Vec<(u32, bool)>, Option<bool>)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let steps = v["steps"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| Some((s["step_number"].as_u64()? as u32, s["converged"].as_bool()?)))
                .collect()
        })
        .unwrap_or_default();
    Some((steps, v["meta"]["success"].as_bool()))
}

/// What the job manager knows about a job started by `ccx_solve`.
pub struct JobView {
    pub base: PathBuf,
    pub running: bool,
    pub failed: bool,
    pub stdout_tail: String,
    pub telemetry_source: TelemetrySource,
}

impl JobView {
    pub fn of(m: &Manifest) -> Self {
        Self {
            base: m.base(),
            running: !m.state.terminal(),
            failed: matches!(m.state, State::Failed | State::TimedOut),
            stdout_tail: tail_text(&m.run_dir.join("stdout.log"), 200, 16 * 1024).unwrap_or_default(),
            telemetry_source: m.telemetry_source,
        }
    }
}

/// Last `lines` lines of a text file, at most `max_bytes`, with a marker when cut.
pub fn tail_text(path: &Path, lines: usize, max_bytes: usize) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes as u64);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().collect();
    // When the read started mid-file the first line is partial: drop it.
    let usable = if start > 0 && all.len() > 1 { &all[1..] } else { &all[..] };
    let kept = &usable[usable.len().saturating_sub(lines)..];
    let cut = start > 0 || kept.len() < usable.len();
    let mut out = String::new();
    if cut {
        out.push_str("\u{2026}[truncated: earlier output omitted]\n");
    }
    out.push_str(&kept.join("\n"));
    Some(out)
}

pub fn run_diagnose(roots: &Roots, p: CcxDiagnoseIn, job: Option<JobView>) -> Result<CcxDiagnoseOut, ToolError> {
    let base = match (&job, &p.job_path) {
        (Some(j), _) => j.base.clone(),
        (None, Some(path)) => job_base(roots, path)?,
        (None, None) => return Err(ToolError::invalid("give job_path or job_id")),
    };
    let read = |ext: &str| std::fs::read_to_string(with_ext(&base, ext)).ok();
    let (sta_text, cvg_text) = (read("sta"), read("cvg"));
    let telemetry = read_json(&with_ext(&base, "json"));
    let stock_job = job.as_ref().is_some_and(|j| j.telemetry_source == TelemetrySource::StaCvg);

    if sta_text.is_none() && cvg_text.is_none() && telemetry.is_none() {
        // List the jobs that do exist in that directory, closest names first.
        let wanted = base.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut jobs: Vec<String> = std::fs::read_dir(base.parent().unwrap_or(Path::new(".")))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|f| f.extension().is_some_and(|e| e == "sta" || e == "cvg"))
            .filter_map(|f| f.file_stem().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        jobs.sort();
        jobs.dedup();
        return Err(ToolError::new("FILE_NOT_FOUND", format!("no .sta, .cvg or .json telemetry for job '{}'", wanted))
            .valid(crate::error::nearest(&wanted, jobs.into_iter()))
            .hint("Pass the job path without extension; valid lists jobs with telemetry in that directory."));
    }

    let sta = sta_text.as_deref().map(parse_sta).unwrap_or_default();
    let cvg = cvg_text.as_deref().map(parse_cvg).unwrap_or_default();
    let (json_steps, success) = telemetry.clone().unwrap_or_default();
    let ctx = DiagCtx {
        running: job.as_ref().is_some_and(|j| j.running),
        exit_failed: success == Some(false) || job.as_ref().is_some_and(|j| j.failed),
        json_steps,
        stdout_tail: job.map(|j| j.stdout_tail).unwrap_or_default(),
    };
    let window = p.last_increments.unwrap_or(10).clamp(1, 50);
    let d = diagnose(&sta, &cvg, &ctx, &Thresholds::default(), p.step, window);

    let mut steps: Vec<u32> = sta.iter().map(|r| r.step).collect();
    steps.dedup();
    if let Some(wanted) = p.step {
        if !steps.is_empty() && !steps.contains(&wanted) && !cvg.iter().any(|r| r.step == wanted) {
            return Err(ToolError::new("UNKNOWN_STEP", format!("step {} has no telemetry", wanted))
                .valid(steps.iter().map(u32::to_string).collect()));
        }
    }

    let selected: Vec<_> = sta.iter().filter(|r| p.step.is_none_or(|s| r.step == s)).collect();
    let rows = selected[selected.len().saturating_sub(window)..]
        .iter()
        .map(|r| {
            // Contact count and force residual of the last iteration of the converged attempt.
            let last = cvg.iter().rfind(|c| (c.step, c.inc, c.att) == (r.step, r.inc, r.att));
            vec![
                json!(r.step), json!(r.inc), json!(r.att), json!(r.itrs),
                num(r.tot_time as f32), num(r.inc_time as f32),
                last.map(|c| json!(c.contact_el)).unwrap_or(Value::Null),
                last.and_then(|c| c.resid_force).map(|v| num(v as f32)).unwrap_or(Value::Null),
            ]
        })
        .collect();

    let mut notes = vec!["Thresholds are heuristics calibrated on synthetic cases plus a few real contact runs; treat WARN findings as leads, not proof.".to_string()];
    if telemetry.is_none() {
        if stock_job {
            notes.push("Stock CalculiX: no JSON telemetry; this diagnosis uses .sta/.cvg only.".into());
        } else {
            notes.push("No <job>.json found: per-step convergence flags are not available. The CalculiX MultiSolver build writes it when run with -j.".into());
        }
    }

    Ok(CcxDiagnoseOut {
        verdict: d.verdict.as_str().to_string(),
        coverage: Coverage { sta_rows: sta.len(), cvg_rows: cvg.len(), steps, json: telemetry.is_some() },
        findings: d
            .findings
            .iter()
            .take(10)
            .map(|f| FindingOut {
                rule: f.rule.to_string(),
                severity: f.severity.as_str().to_string(),
                step: f.step,
                inc: f.inc,
                att: f.att,
                evidence: f.evidence.iter().map(|(k, v)| (k.to_string(), num(*v as f32))).collect(),
                occurrences: f.occurrences,
                advice: f.advice.to_string(),
            })
            .collect(),
        increments: Table {
            columns: ["step", "inc", "att", "itrs", "tot_time", "inc_time", "contact_el", "resid_force_pct"].map(String::from).to_vec(),
            rows,
        },
        solver_messages: d.solver_messages.into_iter().take(5).collect(),
        notes,
    })
}

// ---------- ccx_solve ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CcxSolveIn {
    /// Path to the root .inp deck
    pub inp_path: String,
    /// One of the solver names the server was started with. Default: the first one
    pub solver: Option<String>,
    /// Threads for the solver (default: up to 4, capped at the CPU count)
    pub threads: Option<usize>,
    /// Wall-clock limit in seconds (default 3600, max 86400)
    pub timeout_s: Option<u64>,
    /// Free-text label to recognise the job later
    pub label: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CcxSolveOut {
    pub job_id: String,
    /// QUEUED when another job holds the single run slot, else STARTING
    pub state: State,
    /// Isolated directory holding a copy of the deck and all results
    pub run_dir: String,
    pub jobname: String,
    pub solver: String,
    /// json: CalculiX MultiSolver, per-step convergence, timings and mode tables in <job>.json.
    /// sta_cvg: stock CalculiX, progress and diagnosis from .sta/.cvg only
    pub telemetry_source: TelemetrySource,
    pub threads: usize,
    pub files_copied: usize,
    pub flags: Vec<String>,
    pub next: String,
}

fn valid_jobname(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['-', '.'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Preflight the deck and copy it, with every file it references, into a fresh run directory.
/// Blocking. The original deck directory is never written to.
pub fn prepare_solve(roots: &Roots, jobs: &JobManager, p: CcxSolveIn) -> Result<(Prepared, usize, Vec<String>), ToolError> {
    let Solver { name: solver, path: binary, json } = jobs.solver(p.solver.as_deref())?;
    let deck = roots.existing(&p.inp_path, &["inp"])?;
    let jobname = deck.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
    if !valid_jobname(&jobname) {
        return Err(ToolError::invalid(format!("deck name '{}' is not usable as a job name (letters, digits, _ - . only)", jobname)));
    }

    let scan = scan_cards(&deck, &ScanLimits::default())
        .map_err(|e| ToolError::new("DECK_INVALID", format!("{}: {}", roots.display(&deck), e)))?;
    let refs = scan.external_refs();
    let missing: Vec<String> = scan
        .includes
        .iter()
        .filter(|i| !i.exists)
        .map(|i| format!("{}:{} -> {}", roots.display(&i.included_from), i.line_number, i.raw_target))
        .chain(refs.iter().filter(|r| !r.exists).map(|r| format!("{}:{} -> {}", roots.display(&r.file), r.line, r.target)))
        .collect();
    if !missing.is_empty() {
        return Err(ToolError::new("DECK_INVALID", format!("{} referenced file(s) are missing", missing.len()))
            .valid(missing)
            .hint("Fix the references, then call inp_inspect to verify."));
    }
    let flags: Vec<String> = scan.flags().into_iter().map(String::from).collect();
    if flags.iter().any(|f| f == "NO_STEPS") {
        return Err(ToolError::new("DECK_INVALID", "the deck has no *STEP").hint("Call inp_inspect to review the deck."));
    }

    // Nothing the deck pulls in may come from outside the allowed roots.
    let deck_dir = deck.parent().unwrap_or(Path::new("/")).to_path_buf();
    let mut sources: Vec<(PathBuf, bool)> = vec![(deck.clone(), true)]; // (file, inlinable by consolidation)
    sources.extend(scan.includes.iter().map(|i| (i.resolved_path.clone(), true)));
    sources.extend(refs.iter().map(|r| (r.resolved_path.clone(), r.param.eq_ignore_ascii_case("INPUT"))));
    let mut files: Vec<(PathBuf, Option<PathBuf>, bool)> = Vec::new(); // (source, path under the deck dir, inlinable)
    for (source, inlinable) in sources {
        if !roots.contains(&source) {
            return Err(ToolError::new("PATH_OUTSIDE_ROOTS", format!("the deck references {} outside the allowed roots", source.display())));
        }
        let canonical = source.canonicalize().map_err(ToolError::internal)?;
        let rel = canonical.strip_prefix(&deck_dir).ok().map(Path::to_path_buf);
        if !files.iter().any(|f| f.0 == canonical) {
            files.push((canonical, rel, inlinable));
        }
    }

    // Normal case: mirror the deck directory layout. If a file lies outside the deck directory
    // the layout cannot be mirrored, so the deck is flattened into one file instead.
    let flatten = files.iter().any(|f| f.1.is_none());
    if let Some(stuck) = files.iter().find(|f| flatten && f.1.is_none() && !f.2) {
        return Err(ToolError::new("DECK_INVALID", format!("{} is referenced by FILE= and lies outside the deck directory; it cannot be inlined", stuck.0.display()))
            .hint("Move the file under the deck directory."));
    }
    let consolidated = if flatten {
        Some(consolidate_inp(&deck).map_err(|e| ToolError::new("DECK_INVALID", e.to_string()))?.0)
    } else {
        None
    };

    let (job_id, run_dir) = jobs.new_run_dir(roots.first())?;
    let mut copied = 0;
    for (source, rel, inlinable) in &files {
        let target = match (&consolidated, rel) {
            (Some(_), _) if *inlinable => continue, // inlined into the flattened deck
            (_, Some(rel)) => run_dir.join(rel),
            (_, None) => continue,
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(ToolError::internal)?;
        }
        std::fs::copy(source, &target).map_err(|e| ToolError::internal(format!("copying {}: {}", source.display(), e)))?;
        copied += 1;
    }
    if let Some(text) = &consolidated {
        std::fs::write(run_dir.join(format!("{}.inp", jobname)), text).map_err(ToolError::internal)?;
        copied += 1;
    }

    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let prepared = Prepared {
        job_id,
        run_dir,
        source_inp: deck,
        jobname,
        solver,
        binary,
        json,
        threads: p.threads.unwrap_or(4).clamp(1, cpus),
        timeout_s: p.timeout_s.unwrap_or(3600).clamp(1, 86_400),
        label: p.label.map(|l| l.chars().take(60).collect()),
    };
    Ok((prepared, copied, flags))
}

pub fn solve_out(roots: &Roots, m: &Manifest, files_copied: usize, flags: Vec<String>) -> CcxSolveOut {
    CcxSolveOut {
        job_id: m.job_id.clone(),
        state: m.state,
        run_dir: roots.display(&m.run_dir),
        jobname: m.jobname.clone(),
        solver: m.solver.clone(),
        telemetry_source: m.telemetry_source,
        threads: m.threads,
        files_copied,
        flags,
        next: "Call ccx_job_status with this job_id and wait_s=30.".into(),
    }
}

// ---------- ccx_job_status / ccx_job_cancel ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CcxJobStatusIn {
    pub job_id: String,
    /// Block up to this many seconds (max 50) and return early when the state changes
    pub wait_s: Option<u64>,
    /// Include the last N lines of solver output (max 100)
    pub tail_lines: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Progress {
    pub step: u32,
    pub inc: u32,
    pub att: u32,
    pub tot_time: Value,
    pub inc_time: Value,
    pub increments_done: usize,
    /// Sum of (att - 1) over completed increments
    pub cutbacks: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CcxJobStatusOut {
    pub job_id: String,
    pub state: State,
    pub label: Option<String>,
    pub jobname: String,
    pub solver: String,
    /// json | sta_cvg (stock CalculiX, see `notes`)
    pub telemetry_source: TelemetrySource,
    pub threads: usize,
    pub elapsed_s: f64,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// Why the job failed: SPAWN | EXIT_CODE | SIGNAL | SOLVER_REPORTED | TIMEOUT | LOST, with detail
    pub failure: Option<String>,
    /// Resident memory of the solver process while it runs
    pub rss_mb: Option<u64>,
    /// Latest completed increment, from <job>.sta
    pub progress: Option<Progress>,
    /// Convergence verdict so far (same values as ccx_diagnose)
    pub verdict: Option<String>,
    /// 'meta' and 'steps' of <job>.json once the solver has written it. Per-node and per-element
    /// arrays (from *NODE PRINT / *EL PRINT) are reduced to their totals and counts, at most 20 modes
    /// and the last 5 increments per step are kept; `reduced` says what was cut
    pub telemetry: Option<Value>,
    /// Result files that exist, by kind
    pub files: Map<String, Value>,
    pub tail: Option<String>,
    /// Limits of this job's telemetry, when there are any
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub next: String,
}

pub const STOCK_NOTE: &str = "Stock CalculiX: no JSON telemetry, so per-step convergence flags, solver name, timings and mode tables are not available; progress and diagnosis use .sta/.cvg only. The CalculiX MultiSolver build (https://github.com/carlomontec/CalculiX-CrunchiX-MultiSolver) writes this telemetry with -j.";

/// Build the status view. Blocking (reads the run directory).
pub fn job_status(roots: &Roots, m: &Manifest, tail_lines: usize) -> CcxJobStatusOut {
    let base = m.base();
    let read = |ext: &str| std::fs::read_to_string(with_ext(&base, ext)).ok();
    let sta = read("sta").as_deref().map(parse_sta).unwrap_or_default();
    let cvg = read("cvg").as_deref().map(parse_cvg).unwrap_or_default();
    let json: Option<Value> = read("json").and_then(|t| serde_json::from_str(&t).ok());

    let progress = sta.last().map(|r| Progress {
        step: r.step,
        inc: r.inc,
        att: r.att,
        tot_time: num(r.tot_time as f32),
        inc_time: num(r.inc_time as f32),
        increments_done: sta.len(),
        cutbacks: sta.iter().map(|r| r.att.saturating_sub(1)).sum(),
    });

    let view = JobView::of(m);
    let verdict = (!sta.is_empty() || !cvg.is_empty() || json.is_some()).then(|| {
        let json_steps = read_json(&with_ext(&base, "json")).map(|j| j.0).unwrap_or_default();
        let ctx = DiagCtx { running: view.running, exit_failed: view.failed, json_steps, stdout_tail: view.stdout_tail.clone() };
        diagnose(&sta, &cvg, &ctx, &Thresholds::default(), None, 10).verdict.as_str().to_string()
    });

    let mut files = Map::new();
    for ext in ["frd", "dat", "sta", "cvg", "json"] {
        let path = with_ext(&base, ext);
        if path.exists() {
            files.insert(ext.into(), json!(roots.display(&path)));
        }
    }
    for (kind, name) in [("stdout", "stdout.log"), ("stderr", "stderr.log")] {
        let path = m.run_dir.join(name);
        if std::fs::metadata(&path).is_ok_and(|md| md.len() > 0) {
            files.insert(kind.into(), json!(roots.display(&path)));
        }
    }

    let tail = (tail_lines > 0).then(|| {
        let out = tail_text(&m.run_dir.join("stdout.log"), tail_lines.min(100), 4096).unwrap_or_default();
        let err = tail_text(&m.run_dir.join("stderr.log"), 10, 1024).filter(|e| !e.trim().is_empty());
        match err {
            Some(e) => format!("{}\n--- stderr ---\n{}", out, e),
            None => out,
        }
    });

    let next = match m.state {
        State::Queued | State::Starting | State::Running | State::Cancelling => "Call ccx_job_status again with wait_s=30.",
        State::Succeeded => "Call frd_summary on files.frd.",
        State::Failed | State::TimedOut => "Call ccx_diagnose with this job_id, and ccx_job_status with tail_lines=40 to read the solver output.",
        State::Cancelled | State::Lost => "The job did not finish; call ccx_solve to run it again.",
    };

    CcxJobStatusOut {
        job_id: m.job_id.clone(),
        state: m.state,
        label: m.label.clone(),
        jobname: m.jobname.clone(),
        solver: m.solver.clone(),
        telemetry_source: m.telemetry_source,
        threads: m.threads,
        elapsed_s: m.elapsed_s(),
        exit_code: m.exit_code,
        signal: m.signal,
        failure: m.failure.clone(),
        rss_mb: m.pid.filter(|_| m.state == State::Running).and_then(rss_mb),
        progress,
        verdict,
        telemetry: json.map(compact_telemetry),
        files,
        tail,
        notes: if m.telemetry_source == TelemetrySource::StaCvg { vec![STOCK_NOTE.to_string()] } else { Vec::new() },
        next: next.into(),
    }
}

const TELEMETRY_MAX_MODES: usize = 20;
const TELEMETRY_MAX_INCREMENTS: usize = 5;

/// Keep ccx's JSON telemetry small enough for a model's context. The MultiSolver JSON carries the
/// full *NODE PRINT / *EL PRINT output (one row per node and increment), which on a real model is
/// thousands of rows: those arrays are replaced by their length, totals are kept.
fn compact_telemetry(mut j: Value) -> Value {
    // ccx writes a constant 1 here regardless of the threads used; drop it rather than mislead.
    if let Some(meta) = j["meta"].as_object_mut() {
        meta.remove("num_threads");
    }
    let mut cut: Vec<String> = Vec::new();
    let mut steps = j["steps"].take();
    if let Some(list) = steps.as_array_mut() {
        for step in list.iter_mut() {
            let n = step["step_number"].as_u64().unwrap_or(0);
            if let Some(obj) = step.as_object_mut() {
                if let Some(Value::Array(modes)) = obj.get_mut("modes") {
                    if modes.len() > TELEMETRY_MAX_MODES {
                        cut.push(format!("step {}: first {} of {} modes", n, TELEMETRY_MAX_MODES, modes.len()));
                        let total = modes.len();
                        modes.truncate(TELEMETRY_MAX_MODES);
                        obj.insert("modes_total".into(), json!(total));
                    }
                }
                if let Some(Value::Array(incs)) = obj.get_mut("increments") {
                    if incs.len() > TELEMETRY_MAX_INCREMENTS {
                        cut.push(format!("step {}: last {} of {} increments", n, TELEMETRY_MAX_INCREMENTS, incs.len()));
                        let total = incs.len();
                        incs.drain(..total - TELEMETRY_MAX_INCREMENTS);
                        obj.insert("increments_total".into(), json!(total));
                    }
                }
            }
            if drop_bulk_arrays(step) {
                cut.push(format!("step {}: per-node and per-element values replaced by counts (read <job>.dat, or query the .frd)", n));
            }
        }
    }
    let mut out = json!({ "meta": j["meta"].take(), "steps": steps });
    if !cut.is_empty() {
        out["reduced"] = json!(cut);
    }
    out
}

/// Replace every array under the keys `values`, `nodes` or `elements` by `<key>_count`.
/// Returns whether anything was replaced.
fn drop_bulk_arrays(v: &mut Value) -> bool {
    let mut changed = false;
    match v {
        Value::Object(obj) => {
            for key in ["values", "nodes", "elements"] {
                if let Some(Value::Array(a)) = obj.get(key) {
                    let len = a.len();
                    obj.remove(key);
                    obj.insert(format!("{}_count", key), json!(len));
                    changed = true;
                }
            }
            for child in obj.values_mut() {
                changed |= drop_bulk_arrays(child);
            }
        }
        Value::Array(a) => {
            for child in a.iter_mut() {
                changed |= drop_bulk_arrays(child);
            }
        }
        _ => {}
    }
    changed
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CcxJobCancelIn {
    pub job_id: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CcxJobCancelOut {
    pub job_id: String,
    /// State before the request
    pub was: State,
    /// State now. Already finished jobs are returned unchanged
    pub state: State,
}

// ---------- ccx_job_list ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CcxJobListIn {
    /// Jobs to return (1..50, default 10)
    pub limit: Option<usize>,
    /// Jobs to skip, for paging (default 0)
    pub offset: Option<usize>,
    /// Only jobs in this state, e.g. RUNNING, FAILED, SUCCEEDED
    pub state: Option<State>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CcxJobListOut {
    /// Newest first. Columns: job_id, state, jobname, label, elapsed_s, run_dir_mb, telemetry_source
    pub jobs: Table,
    pub page: Page,
}

/// Jobs of the models folder `root` (also finished ones on disk) and every job of this server run. Blocking (measures run directories).
pub fn job_list(jobs: &JobManager, root: &Path, p: CcxJobListIn) -> CcxJobListOut {
    let limit = p.limit.unwrap_or(10).clamp(1, 50);
    let offset = p.offset.unwrap_or(0);
    let (found, total) = jobs.list(Some(root), p.state, offset, limit);
    let rows = found
        .iter()
        .map(|m| {
            let mb = dir_size(&m.run_dir) as f64 / (1024.0 * 1024.0);
            vec![
                json!(m.job_id),
                json!(m.state),
                json!(m.jobname),
                json!(m.label),
                json!(m.elapsed_s()),
                json!((mb * 10.0).round() / 10.0),
                json!(m.telemetry_source),
            ]
        })
        .collect();
    let end = offset + found.len();
    let page = Page { returned: found.len(), total, truncated: found.len() < total, next_offset: (end < total).then_some(end) };
    CcxJobListOut {
        jobs: Table { columns: ["job_id", "state", "jobname", "label", "elapsed_s", "run_dir_mb", "telemetry_source"].map(String::from).to_vec(), rows },
        page,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Roots {
        Roots::new(vec![PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../cgx-core/tests/fixtures/telemetry"))]).unwrap()
    }

    fn run(job: &str) -> Result<CcxDiagnoseOut, ToolError> {
        run_diagnose(&fixtures(), CcxDiagnoseIn { job_path: Some(job.into()), job_id: None, step: None, last_increments: None }, None)
    }

    #[test]
    fn verdicts_on_fixtures_with_any_extension() {
        assert_eq!(run("smooth").unwrap().verdict, "SMOOTH");
        assert_eq!(run("lavffcyc.inp").unwrap().verdict, "LINEAR");
        assert!(run("lavffcyc").unwrap().coverage.json);

        let chatter = run("chatter_failed.sta").unwrap();
        assert_eq!(chatter.verdict, "CONTACT_CHATTER");
        let c1 = chatter.findings.iter().find(|f| f.rule == "C1").unwrap();
        assert_eq!((c1.severity.as_str(), c1.step, c1.inc), ("error", Some(1), Some(5)));
        assert!(c1.evidence.contains_key("flips") && !c1.advice.is_empty());
        assert_eq!(chatter.increments.rows.len(), 10);
        assert_eq!(chatter.increments.rows[4][2], json!(2), "increment 5 converged on attempt 2");

        assert_eq!(run("cutback_loop_att4.cvg").unwrap().verdict, "CUTBACK_LOOP");
        assert_eq!(run("diverge_blowup").unwrap().verdict, "DIVERGED");
    }

    /// A stock-ccx job has .sta/.cvg and no .json: diagnosis must not need JSON.
    #[test]
    fn diagnose_works_without_json_for_a_stock_job() {
        let base = fixtures().creatable("chatter_failed", &[]).unwrap();
        assert!(!with_ext(&base, "json").exists());
        let job = JobView { base, running: false, failed: false, stdout_tail: String::new(), telemetry_source: TelemetrySource::StaCvg };
        let d = run_diagnose(&fixtures(), CcxDiagnoseIn { job_path: None, job_id: Some("x".into()), step: None, last_increments: None }, Some(job)).unwrap();
        assert_eq!(d.verdict, "CONTACT_CHATTER");
        assert!(!d.coverage.json && d.coverage.sta_rows > 0);
        assert!(d.notes.iter().any(|n| n.starts_with("Stock CalculiX: no JSON telemetry")), "{:?}", d.notes);
    }

    #[test]
    fn jobname_rules_and_tail() {
        assert!(valid_jobname("beam_v2.1-a") && !valid_jobname("-j") && !valid_jobname(".hidden") && !valid_jobname("a b") && !valid_jobname(""));
        let path = std::env::temp_dir().join(format!("calculix-mcp-tail-{}", std::process::id()));
        std::fs::write(&path, (1..=50).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n")).unwrap();
        let t = tail_text(&path, 3, 4096).unwrap();
        assert!(t.starts_with('\u{2026}') && t.ends_with("line 48\nline 49\nline 50"), "{}", t);
        assert_eq!(tail_text(&path, 100, 4096).unwrap().lines().count(), 50);
        assert!(tail_text(&path, 100, 40).unwrap().lines().count() < 8);
        let _ = std::fs::remove_file(&path);
    }

    /// A deck whose include lies outside its own directory is flattened into the run directory;
    /// one that reaches outside the roots is refused.
    #[test]
    fn solve_preflight_mirrors_or_flattens() {
        let dir = std::env::temp_dir().join(format!("calculix-mcp-prep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("root/case/sub")).unwrap();
        std::fs::create_dir_all(dir.join("root/shared")).unwrap();
        let step = "*STEP\n*STATIC\n*END STEP\n";
        std::fs::write(dir.join("root/case/sub/mesh.inc"), "*NODE\n1,0.,0.,0.\n").unwrap();
        std::fs::write(dir.join("root/shared/mat.inc"), "*MATERIAL, NAME=STEEL\n").unwrap();
        std::fs::write(dir.join("secret.inc"), "*MATERIAL, NAME=SECRET\n").unwrap();
        std::fs::write(dir.join("root/case/local.inp"), format!("*INCLUDE, INPUT=sub/mesh.inc\n{}", step)).unwrap();
        std::fs::write(dir.join("root/case/shared.inp"), format!("*INCLUDE, INPUT=sub/mesh.inc\n*INCLUDE, INPUT=../shared/mat.inc\n{}", step)).unwrap();
        std::fs::write(dir.join("root/case/escape.inp"), format!("*INCLUDE, INPUT=../../secret.inc\n{}", step)).unwrap();
        std::fs::write(dir.join("root/case/nostep.inp"), "*NODE\n1,0.,0.,0.\n").unwrap();
        std::fs::write(dir.join("root/case/broken.inp"), format!("*INCLUDE, INPUT=gone.inc\n{}", step)).unwrap();
        let fake = dir.join("fake_ccx");
        std::fs::write(&fake, "").unwrap();

        let roots = Roots::new(vec![dir.join("root")]).unwrap();
        let jobs = JobManager::new(roots.first(), vec![Solver { name: "fake".into(), path: fake, json: true }], Vec::new(), std::time::Duration::from_millis(100));
        let prep = |deck: &str| prepare_solve(&roots, &jobs, CcxSolveIn { inp_path: format!("case/{}", deck), solver: None, threads: Some(1), timeout_s: None, label: None });

        let (local, copied, _) = prep("local.inp").unwrap();
        assert_eq!(copied, 2);
        assert!(local.run_dir.join("sub/mesh.inc").exists() && local.run_dir.join("local.inp").exists());

        let (shared, copied, _) = prep("shared.inp").unwrap();
        assert_eq!(copied, 1, "flattened into a single deck");
        let text = std::fs::read_to_string(shared.run_dir.join("shared.inp")).unwrap();
        assert!(text.contains("NAME=STEEL") && text.contains("1,0.,0.,0.") && !text.to_uppercase().contains("*INCLUDE"));

        assert_eq!(prep("escape.inp").unwrap_err().code, "PATH_OUTSIDE_ROOTS");
        assert_eq!(prep("nostep.inp").unwrap_err().code, "DECK_INVALID");
        let broken = prep("broken.inp").unwrap_err();
        assert!(broken.code == "DECK_INVALID" && broken.valid[0].contains("gone.inc"));
        assert_eq!(prepare_solve(&roots, &jobs, CcxSolveIn { inp_path: "case/local.inp".into(), solver: Some("pardiso".into()), threads: None, timeout_s: None, label: None }).err().unwrap().code, "UNKNOWN_SOLVER");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Found in a live test: a 53-node *NODE PRINT set was passed through verbatim.
    #[test]
    fn telemetry_is_compacted() {
        let rows: Vec<Value> = (0..300).map(|i| json!([i as f64, 0.0, 1.0])).collect();
        let ids: Vec<u32> = (1..=300).collect();
        let incs: Vec<Value> = (1..=8)
            .map(|i| json!({"increment_number": i, "node_sets": {"NFIX_RF": {"set": "NFIX", "field": "RF",
                "components": ["fx", "fy", "fz"], "nodes": ids, "values": rows, "total": [0.0, 0.0, 25.0]}}}))
            .collect();
        let modes: Vec<Value> = (1..=30).map(|i| json!({"mode_number": i, "frequency_hz": i as f64})).collect();
        let raw = json!({"meta": {"solver": "MUMPS", "num_threads": 1, "success": true},
            "steps": [{"step_number": 1, "type": "STATIC", "converged": true, "increments": incs},
                      {"step_number": 2, "type": "FREQUENCY", "converged": true, "modes": modes}]});
        let raw_len = raw.to_string().len();
        let out = compact_telemetry(raw);
        let text = out.to_string();
        assert!(text.len() * 10 < raw_len, "{} vs {}", text.len(), raw_len);
        assert!(out["meta"].get("num_threads").is_none());
        let step1 = &out["steps"][0];
        assert_eq!((step1["increments"].as_array().unwrap().len(), step1["increments_total"].as_u64()), (5, Some(8)));
        assert_eq!(step1["increments"][0]["increment_number"], json!(4), "the last increments are kept");
        let set = &step1["increments"][4]["node_sets"]["NFIX_RF"];
        assert_eq!((set["nodes_count"].as_u64(), set["values_count"].as_u64()), (Some(300), Some(300)));
        assert_eq!(set["total"], json!([0.0, 0.0, 25.0]));
        assert!(set.get("values").is_none() && set.get("nodes").is_none());
        assert_eq!((out["steps"][1]["modes"].as_array().unwrap().len(), out["steps"][1]["modes_total"].as_u64()), (20, Some(30)));
        assert_eq!(out["reduced"].as_array().unwrap().len(), 3);

        // Small, flat telemetry passes through without a `reduced` note.
        let small = compact_telemetry(json!({"meta": {"success": true}, "steps": [{"step_number": 1, "converged": true}]}));
        assert!(small.get("reduced").is_none());
    }

    #[test]
    fn errors_are_actionable() {
        let missing = run("smoth").unwrap_err();
        assert_eq!((missing.code, missing.valid[0].as_str()), ("FILE_NOT_FOUND", "smooth"));
        assert!(missing.valid_total.unwrap() > 10 && missing.valid.len() == 10);
        assert_eq!(run("../../../Cargo").unwrap_err().code, "PATH_OUTSIDE_ROOTS");
        let step = run_diagnose(&fixtures(), CcxDiagnoseIn { job_path: Some("smooth".into()), job_id: None, step: Some(9), last_increments: None }, None).unwrap_err();
        assert_eq!((step.code, step.valid.as_slice()), ("UNKNOWN_STEP", &["1".to_string()][..]));
    }
}
