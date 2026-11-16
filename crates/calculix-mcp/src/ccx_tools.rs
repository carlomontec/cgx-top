//! Solver tools. `ccx_diagnose` explains convergence from `<job>.sta` / `<job>.cvg` / `<job>.json`.

use crate::error::ToolError;
use crate::frd_tools::{num, Table};
use crate::paths::Roots;
use cgx_core::{diagnose, parse_cvg, parse_sta, DiagCtx, Thresholds};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

const JOB_EXTENSIONS: [&str; 6] = ["inp", "sta", "cvg", "frd", "dat", "json"];

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CcxDiagnoseIn {
    /// Path to the job, with or without extension: "run/beam", "run/beam.inp", "run/beam.sta"
    pub job_path: String,
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

pub fn run_diagnose(roots: &Roots, p: CcxDiagnoseIn) -> Result<CcxDiagnoseOut, ToolError> {
    let base = job_base(roots, &p.job_path)?;
    let read = |ext: &str| std::fs::read_to_string(with_ext(&base, ext)).ok();
    let (sta_text, cvg_text) = (read("sta"), read("cvg"));
    let telemetry = read_json(&with_ext(&base, "json"));

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
    let ctx = DiagCtx { running: false, exit_failed: success == Some(false), json_steps, stdout_tail: String::new() };
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
        notes.push("No <job>.json found (run ccx with -j for per-step convergence flags).".into());
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixtures() -> Roots {
        Roots::new(vec![PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../cgx-core/tests/fixtures/telemetry"))]).unwrap()
    }

    fn run(job: &str) -> Result<CcxDiagnoseOut, ToolError> {
        run_diagnose(&fixtures(), CcxDiagnoseIn { job_path: job.into(), step: None, last_increments: None })
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

    #[test]
    fn errors_are_actionable() {
        let missing = run("smoth").unwrap_err();
        assert_eq!((missing.code, missing.valid[0].as_str()), ("FILE_NOT_FOUND", "smooth"));
        assert!(missing.valid_total.unwrap() > 10 && missing.valid.len() == 10);
        assert_eq!(run("../../../Cargo").unwrap_err().code, "PATH_OUTSIDE_ROOTS");
        let step = run_diagnose(&fixtures(), CcxDiagnoseIn { job_path: "smooth".into(), step: Some(9), last_increments: None }).unwrap_err();
        assert_eq!((step.code, step.valid.as_slice()), ("UNKNOWN_STEP", &["1".to_string()][..]));
    }
}
