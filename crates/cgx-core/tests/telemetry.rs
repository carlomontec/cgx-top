use cgx_core::{diagnose, parse_cvg, parse_sta, DiagCtx, Diagnosis, Severity, Tail, Thresholds, Verdict};
use std::fs;
use std::io::Write;
use std::path::PathBuf;

fn fixture(name: &str, ext: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/telemetry").join(format!("{name}.{ext}"));
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn ctx() -> DiagCtx {
    DiagCtx { running: false, exit_failed: false, json_steps: vec![], stdout_tail: String::new() }
}

fn run_with(name: &str, ctx: &DiagCtx, step: Option<u32>) -> Diagnosis {
    let sta = parse_sta(&fixture(name, "sta"));
    let cvg = parse_cvg(&fixture(name, "cvg"));
    diagnose(&sta, &cvg, ctx, &Thresholds::default(), step, 10)
}

fn run(name: &str) -> Diagnosis {
    run_with(name, &ctx(), None)
}

fn rules(d: &Diagnosis) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = d.findings.iter().map(|f| f.rule).collect();
    v.sort();
    v
}

/// Asserts the verdict and the exact set of fired rule ids.
fn check(name: &str, verdict: Verdict, expect: &[&str]) -> Diagnosis {
    let d = run(name);
    assert_eq!(d.verdict, verdict, "{name}: verdict, findings: {:#?}", d.findings);
    let mut e: Vec<&str> = expect.to_vec();
    e.sort();
    assert_eq!(rules(&d), e, "{name}: rules, findings: {:#?}", d.findings);
    d
}

fn sev(d: &Diagnosis, rule: &str) -> Severity {
    d.findings.iter().find(|f| f.rule == rule).unwrap().severity
}

fn ev(d: &Diagnosis, rule: &str, key: &str) -> f64 {
    d.findings.iter().find(|f| f.rule == rule).unwrap().evidence.iter().find(|(k, _)| *k == key).unwrap_or_else(|| panic!("no evidence {key}")).1
}

#[test]
fn smooth() {
    let d = check("smooth", Verdict::Smooth, &["S1"]);
    assert_eq!(d.verdict.as_str(), "SMOOTH");
}

#[test]
fn slow_iters() {
    let d = check("slow_iters", Verdict::Slow, &["W1"]);
    assert_eq!(d.findings[0].occurrences, 4);
    assert_eq!(ev(&d, "W1", "K"), 12.0);
}

#[test]
fn slow_rho() {
    let d = check("slow_rho", Verdict::Slow, &["W2"]);
    assert_eq!(d.findings[0].occurrences, 10);
    assert!((ev(&d, "W2", "rho") - 0.8).abs() < 1e-3);
}

#[test]
fn chatter_converged() {
    let d = check("chatter_converged", Verdict::ContactChatter, &["C1"]);
    assert_eq!(sev(&d, "C1"), Severity::Warn);
    assert_eq!(ev(&d, "C1", "flips"), 4.0);
}

#[test]
fn chatter_failed() {
    let d = check("chatter_failed", Verdict::ContactChatter, &["B1", "C1"]);
    assert_eq!(sev(&d, "C1"), Severity::Error);
    assert_eq!(ev(&d, "B1", "cutbacks"), 1.0);
}

#[test]
fn chatter_plateau() {
    let d = check("chatter_plateau", Verdict::ContactChatter, &["C2"]);
    assert_eq!(sev(&d, "C2"), Severity::Warn);
}

#[test]
fn cutback_single() {
    let d = check("cutback_single", Verdict::Cutback, &["B1"]);
    assert_eq!(d.findings[0].inc, Some(4));
    assert_eq!(d.findings[0].att, Some(2));
}

#[test]
fn cutback_loop_consecutive() {
    let d = check("cutback_loop_consecutive", Verdict::CutbackLoop, &["B1", "B2"]);
    assert_eq!(ev(&d, "B2", "consecutive"), 4.0);
    assert_eq!(sev(&d, "B2"), Severity::Error);
}

#[test]
fn cutback_loop_att4() {
    let d = check("cutback_loop_att4", Verdict::CutbackLoop, &["B1", "B3"]);
    assert_eq!(ev(&d, "B3", "att"), 5.0);
}

#[test]
fn cutback_loop_shrink() {
    let d = check("cutback_loop_shrink", Verdict::CutbackLoop, &["B1", "B4"]);
    assert!(ev(&d, "B4", "inc_time") <= 1.01e-4);
    assert!((ev(&d, "B4", "inc_time_max") - 0.1).abs() < 1e-9);
}

#[test]
fn diverge_growth() {
    let d = check("diverge_growth", Verdict::Diverged, &["D1"]);
    assert_eq!(sev(&d, "D1"), Severity::Error);
}

#[test]
fn diverge_recovered() {
    let d = check("diverge_recovered", Verdict::Cutback, &["B1", "D1"]);
    assert_eq!(sev(&d, "D1"), Severity::Warn);
}

#[test]
fn diverge_blowup() {
    let d = check("diverge_blowup", Verdict::Diverged, &["D2"]);
    assert_eq!(sev(&d, "D2"), Severity::Error);
}

#[test]
fn diverge_nan() {
    let cvg = parse_cvg(&fixture("diverge_nan", "cvg"));
    let last = cvg.last().unwrap();
    assert_eq!((last.resid_force, last.corr_disp), (None, None));
    check("diverge_nan", Verdict::Diverged, &["D3"]);
}

#[test]
fn partial_last_line() {
    let sta_txt = fixture("partial_last_line", "sta");
    let cvg_txt = fixture("partial_last_line", "cvg");
    assert!(!sta_txt.ends_with('\n') && !cvg_txt.ends_with('\n'));
    assert_eq!(parse_sta(&sta_txt).len(), 5);
    assert_eq!(parse_cvg(&cvg_txt).len(), 26); // 5 incs of K = 5,6,4,5,6
    let d = check("partial_last_line", Verdict::Smooth, &["S1"]);
    assert!(d.findings[0].occurrences == 5);
}

#[test]
fn multi_step_headers() {
    let sta = parse_sta(&fixture("multi_step_headers", "sta"));
    assert_eq!(sta.len(), 12);
    let d = check("multi_step_headers", Verdict::Cutback, &["B1"]);
    assert_eq!(d.findings[0].step, Some(2));
    let c = ctx();
    let d1 = run_with("multi_step_headers", &c, Some(1));
    assert_eq!((d1.verdict, rules(&d1)), (Verdict::Smooth, vec!["S1"]));
    let d2 = run_with("multi_step_headers", &c, Some(2));
    assert_eq!((d2.verdict, rules(&d2)), (Verdict::Cutback, vec!["B1"]));
    let d3 = run_with("multi_step_headers", &c, Some(3));
    assert_eq!((d3.verdict, rules(&d3)), (Verdict::Smooth, vec!["S1"]));
}

#[test]
fn header_only() {
    assert!(parse_sta(&fixture("header_only", "sta")).is_empty());
    assert!(parse_cvg(&fixture("header_only", "cvg")).is_empty());
    check("header_only", Verdict::NoTelemetry, &["N1"]);
}

#[test]
fn real_lavffcyc_linear() {
    let mut c = ctx();
    c.json_steps = vec![(1, true), (2, true)];
    let d = run_with("lavffcyc", &c, None);
    assert_eq!((d.verdict, rules(&d)), (Verdict::Linear, vec!["L1"]));
    // The real .sta holds a single linear row (att 1, itrs 1), which is enough for L1 without json.
    check("lavffcyc", Verdict::Linear, &["L1"]);
    assert_eq!(parse_sta(&fixture("lavffcyc", "sta")).len(), 1);
    assert!(parse_cvg(&fixture("lavffcyc", "cvg")).is_empty());
}

#[test]
fn real_beam_benchmark_linear() {
    check("beam_benchmark", Verdict::Linear, &["L1"]);
}

#[test]
fn real_nonlinear_layout_and_verdict() {
    let cvg = parse_cvg(&fixture("real_nonlinear", "cvg"));
    let sta = parse_sta(&fixture("real_nonlinear", "sta"));
    assert_eq!(sta.len(), 57);
    let first = &cvg[0];
    assert_eq!((first.step, first.inc, first.att, first.iter, first.contact_el), (1, 1, 1, 1, 0));
    assert_eq!(first.resid_force, Some(0.0));
    assert_eq!(first.corr_disp, Some(100.0));
    assert_eq!(first.resid_flux, Some(200.0));
    assert_eq!(first.corr_temp, Some(100.0));
    check("real_nonlinear", Verdict::Smooth, &["S1"]);
}

#[test]
fn real_contact_rows() {
    let cvg = parse_cvg(&fixture("real_contact", "cvg"));
    let sta = parse_sta(&fixture("real_contact", "sta"));
    assert_eq!(sta.len(), 3);
    assert_eq!(cvg.len(), 7);
    let r = &cvg[2];
    assert_eq!((r.step, r.inc, r.att, r.iter, r.contact_el), (2, 1, 1, 1, 56));
    assert_eq!(r.resid_force, Some(700.1));
    let d = run("real_contact");
    assert_eq!(d.verdict, Verdict::Smooth, "{:#?}", d.findings);
}

// ---------- parser tests ----------

#[test]
fn parse_repeated_headers_and_spellings() {
    let txt = "SUMMARY OF JOB INFORMATION\n  STEP INC ATT ITRS TOT TIME STEP TIME INC TIME\n\
               1 1 1 1 0.1E+01 0.1E+01 0.1E+01\n\nSUMMARY OF JOB INFORMATION\n  STEP INC ATT ITRS TOT TIME STEP TIME INC TIME\n\
               2 1 1 3 0.2E+01 0.1E+01 0.1E+01\n";
    let rows = parse_sta(txt);
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[1].step, rows[1].itrs, rows[1].tot_time), (2, 3, 2.0));
    let cvg = "SUMMARY OF CONVERGENCE INFORMATION\nSUMMARY OF C0NVERGENCE INFORMATION\n 1 1 1 1 0 0.1E+01 0.2E+01 0.0E+00 0.0E+00\n";
    assert_eq!(parse_cvg(cvg).len(), 1);
}

#[test]
fn parse_wrong_token_count_and_non_integer_first_token() {
    assert!(parse_sta("1 1 1 1 0.1 0.1\n1 1 1 1 0.1 0.1 0.1 0.1\nx 1 1 1 0.1 0.1 0.1\n").is_empty());
    assert!(parse_cvg("1 1 1 1 0 0.1 0.1 0.1\n1 1 1 1 0 0.1 0.1 0.1 0.1 0.1\n").is_empty());
}

#[test]
fn parse_partial_last_line_is_dropped() {
    let full = "1 1 1 1 0.1E+01 0.1E+01 0.1E+01\n";
    let partial = "1 2 1 1 0.2E+01 0.1E+01 0.1E+0"; // 7 tokens but cut inside the last float
    assert_eq!(parse_sta(&format!("{full}{partial}")).len(), 1);
    assert_eq!(parse_sta(&format!("{full}{partial}1\n")).len(), 2);
}

#[test]
fn parse_nan_and_stars_become_none() {
    let txt = "1 1 1 1 0 NaN Infinity ******** 0.1E+01\n1 1 1 2 3 0.5E+00 -inf 0.0E+00 1.0-101\n";
    let rows = parse_cvg(txt);
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].resid_force, rows[0].corr_disp, rows[0].resid_flux, rows[0].corr_temp), (None, None, None, Some(1.0)));
    assert_eq!((rows[1].resid_force, rows[1].corr_disp, rows[1].resid_flux, rows[1].corr_temp), (Some(0.5), None, Some(0.0), None));
    assert_eq!(rows[1].contact_el, 3);
    let sta = parse_sta("1 1 1 1 0.1E+01 ******** NaN\n");
    assert_eq!(sta.len(), 1);
    assert_eq!(sta[0].tot_time, 1.0);
    assert!(sta[0].step_time.is_nan() && sta[0].inc_time.is_nan());
}

fn tmp_file(tag: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(&dir).unwrap();
    dir.join(format!("tail_{tag}_{}.sta", std::process::id()))
}

#[test]
fn tail_appends_partial_lines_and_truncation() {
    let path = tmp_file("a");
    let _ = fs::remove_file(&path);
    let mut t = Tail::new(path.clone());
    // Missing file: empty, no error.
    assert_eq!(t.poll().unwrap(), "");

    fs::write(&path, "line one\nline tw").unwrap();
    assert_eq!(t.poll().unwrap(), "line one\n");
    assert_eq!(t.poll().unwrap(), "");

    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"o\nline three\npart").unwrap();
    drop(f);
    assert_eq!(t.poll().unwrap(), "line two\nline three\n");
    assert_eq!(t.poll().unwrap(), "");

    // Shrinking file: offset resets and the new content is returned from the start.
    fs::write(&path, "new\n").unwrap();
    assert_eq!(t.poll().unwrap(), "new\n");
    assert_eq!(t.poll().unwrap(), "");

    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"more\n").unwrap();
    drop(f);
    assert_eq!(t.poll().unwrap(), "more\n");
    let _ = fs::remove_file(&path);
}

#[test]
fn tail_feeds_parser_incrementally() {
    let path = tmp_file("b");
    let _ = fs::remove_file(&path);
    let full = fixture("smooth", "sta");
    let cut = full.len() - 20; // ends inside the last row
    fs::write(&path, &full[..cut]).unwrap();
    let mut t = Tail::new(path.clone());
    let mut rows = parse_sta(&t.poll().unwrap());
    assert_eq!(rows.len(), 9);
    fs::write(&path, &full).unwrap();
    // Header lines and complete rows only; the previously cut row is delivered whole in the tail text? No:
    // the first poll stopped at the last '\n', so the cut row is re-read from its start.
    rows.extend(parse_sta(&t.poll().unwrap()));
    assert_eq!(rows.len(), 10);
    let _ = fs::remove_file(&path);
}

// ---------- stdout / context rules ----------

#[test]
fn stdout_patterns_and_job_failure() {
    let mut c = ctx();
    c.exit_failed = true;
    c.stdout_tail = "*ERROR in nonlingeo: increment size smaller than minimum\n*ERROR in nonlingeo: something else\nToo Many Attempts\n".to_string();
    let d = run_with("smooth", &c, None);
    assert_eq!(d.verdict, Verdict::Diverged);
    assert_eq!(rules(&d), vec!["D5"]); // last attempt converged, so no D4
    assert_eq!(d.findings[0].occurrences, 3);
    assert_eq!(d.solver_messages.len(), 4);
    let d4 = run_with("diverge_growth", &c, None);
    assert!(rules(&d4).contains(&"D4") && rules(&d4).contains(&"D1"));

    // *ERROR alone is only a message when the job did not fail.
    let mut c2 = ctx();
    c2.stdout_tail = "*ERROR in x: oops\nbest solution and residuals are in the frd file\n".to_string();
    let d2 = run_with("smooth", &c2, None);
    assert_eq!(d2.verdict, Verdict::Smooth);
    assert_eq!(d2.solver_messages.len(), 3);
}

#[test]
fn d4_json_step_not_converged() {
    let mut c = ctx();
    c.json_steps = vec![(1, false)];
    let d = run_with("diverge_growth", &c, None);
    assert!(rules(&d).contains(&"D4"));
}
