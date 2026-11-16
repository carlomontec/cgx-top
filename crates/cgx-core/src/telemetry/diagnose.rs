//! Convergence diagnosis from `.sta` / `.cvg` telemetry (rules N1, L1, S1, W*, C*, B*, D*).
//!
//! All numeric thresholds are ASSUMED (see `Thresholds`) and must be recalibrated on real
//! nonlinear telemetry.

use super::{CvgRow, StaRow};
use std::collections::{BTreeMap, BTreeSet, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    NoTelemetry,
    Linear,
    Smooth,
    Slow,
    ContactChatter,
    Cutback,
    CutbackLoop,
    Diverged,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::NoTelemetry => "NO_TELEMETRY",
            Verdict::Linear => "LINEAR",
            Verdict::Smooth => "SMOOTH",
            Verdict::Slow => "SLOW",
            Verdict::ContactChatter => "CONTACT_CHATTER",
            Verdict::Cutback => "CUTBACK",
            Verdict::CutbackLoop => "CUTBACK_LOOP",
            Verdict::Diverged => "DIVERGED",
        }
    }

    /// Ranking of spec 4.3: DIVERGED > CUTBACK_LOOP > CONTACT_CHATTER > CUTBACK > SLOW > SMOOTH > LINEAR > NO_TELEMETRY.
    fn rank(&self) -> u8 {
        match self {
            Verdict::NoTelemetry => 0,
            Verdict::Linear => 1,
            Verdict::Smooth => 2,
            Verdict::Slow => 3,
            Verdict::Cutback => 4,
            Verdict::ContactChatter => 5,
            Verdict::CutbackLoop => 6,
            Verdict::Diverged => 7,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warn,
    Error,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub rule: &'static str,
    pub severity: Severity,
    pub step: Option<u32>,
    pub inc: Option<u32>,
    pub att: Option<u32>,
    pub evidence: Vec<(&'static str, f64)>,
    pub occurrences: usize,
    pub advice: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnosis {
    pub verdict: Verdict,
    pub findings: Vec<Finding>,
    pub solver_messages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiagCtx {
    pub running: bool,
    pub exit_failed: bool,
    /// `(step, converged)` as reported by the solver JSON.
    pub json_steps: Vec<(u32, bool)>,
    pub stdout_tail: String,
}

/// Every numeric threshold of spec 4.3 / 4.4 (all ASSUMED).
#[derive(Debug, Clone, PartialEq)]
pub struct Thresholds {
    /// S1: max iterations of a smooth attempt; W1: attempts with more iterations are slow.
    pub slow_k: usize,
    /// S1: max contraction where defined.
    pub smooth_rho_max: f64,
    /// W1/W2: min number of increments in the window.
    pub slow_min_incs: usize,
    /// W2: min iterations.
    pub slow_rho_k_min: usize,
    /// W2: contraction range (exclusive).
    pub slow_rho_lo: f64,
    pub slow_rho_hi: f64,
    /// Contraction needs at least this many iterations.
    pub rho_k_min: usize,
    /// C1: min iterations, analysed tail length, min sign flips.
    pub chatter_k_min: usize,
    pub chatter_window: usize,
    pub chatter_flips_min: usize,
    /// C1: contact count span must be >= max(abs, rel * mean n).
    pub chatter_span_abs: f64,
    pub chatter_span_rel: f64,
    /// C1: min r / max r over the tail must exceed this (stagnation).
    pub chatter_stagnation: f64,
    /// C2: min iterations, tail length, max max/min factor and residual floor (%).
    pub plateau_k_min: usize,
    pub plateau_window: usize,
    pub plateau_factor: f64,
    pub plateau_floor_pct: f64,
    /// B2: consecutive increments with att > 1.
    pub loop_consecutive: usize,
    /// B3: attempt number that signals a loop.
    pub loop_att: u32,
    /// B4: latest inc_time <= ratio * max inc_time of the step, with cutbacks in the last N increments.
    pub shrink_ratio: f64,
    pub shrink_cutbacks: u32,
    pub shrink_window: usize,
    /// D1: first iteration (1-based) at which growth is checked (two consecutive iterations).
    pub d1_k_min: usize,
    /// D2: r_k >= factor * min earlier r (and r_k above `d2_floor_pct`), or r_k > abs.
    pub d2_factor: f64,
    /// D2: relative growth only counts above this residual level (%). Added after real data showed
    /// residuals bouncing from 1e-6 to 1e-3 % in healthy contact runs.
    pub d2_floor_pct: f64,
    pub d2_abs: f64,
    /// Stdout: number of trailing lines scanned and max distinct `*ERROR` lines reported.
    pub stdout_lines: usize,
    pub max_error_messages: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            slow_k: 8,
            smooth_rho_max: 0.5,
            slow_min_incs: 3,
            slow_rho_k_min: 6,
            slow_rho_lo: 0.5,
            slow_rho_hi: 1.0,
            rho_k_min: 3,
            chatter_k_min: 6,
            chatter_window: 6,
            chatter_flips_min: 3,
            chatter_span_abs: 2.0,
            chatter_span_rel: 0.01,
            chatter_stagnation: 0.1,
            plateau_k_min: 10,
            plateau_window: 6,
            plateau_factor: 3.0,
            plateau_floor_pct: 0.5,
            loop_consecutive: 3,
            loop_att: 4,
            shrink_ratio: 0.01,
            shrink_cutbacks: 2,
            shrink_window: 5,
            d1_k_min: 4,
            d2_factor: 1000.0,
            d2_floor_pct: 1.0,
            d2_abs: 1e6,
            stdout_lines: 200,
            max_error_messages: 5,
        }
    }
}

const ADV_CHATTER: &str = "Inspect *SURFACE BEHAVIOR and *CONTACT PAIR with inp_cards. Consider a softer pressure-overclosure, a smaller initial increment, or contact stabilisation.";
const ADV_CUTBACK: &str = "Cutbacks recovered. Reduce the initial increment in the procedure card if they recur.";
const ADV_LOOP: &str = "The increment keeps shrinking. Check the procedure card increments and *CONTROLS; look for abrupt load or contact changes at the reported increment.";
const ADV_DIVERGED: &str = "Check for rigid-body motion (missing *BOUNDARY), material instability or excessive load. Run inp_inspect and review the step's cards.";
const ADV_SLOW: &str = "Converging but expensive. A smaller increment or a tighter contact definition usually reduces iterations.";

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Converged,
    Failed,
    Open,
}

/// One run of `.cvg` rows sharing `(step, inc, att)`.
struct Attempt {
    step: u32,
    inc: u32,
    att: u32,
    r: Vec<Option<f64>>,
    c: Vec<Option<f64>>,
    flux: Vec<Option<f64>>,
    temp: Vec<Option<f64>>,
    n: Vec<u64>,
    status: Status,
    /// A later attempt of the same increment converged.
    later_converged: bool,
}

impl Attempt {
    fn k(&self) -> usize {
        self.r.len()
    }
}

type Ev = Vec<(&'static str, f64)>;

/// A finding instance plus a score used to pick the "worst" instance when merging.
struct Cand {
    f: Finding,
    score: f64,
}

fn rho(r: &[Option<f64>], th: &Thresholds) -> Option<f64> {
    let k = r.len();
    if k < th.rho_k_min {
        return None;
    }
    let (a, b) = (r[0]?, r[k - 1]?);
    if a > 0.0 && b > 0.0 {
        Some((b / a).powf(1.0 / (k as f64 - 1.0)))
    } else {
        None
    }
}

fn att_evidence(a: &Attempt, th: &Thresholds) -> Ev {
    let mut e: Ev = vec![("K", a.k() as f64)];
    if let Some(x) = rho(&a.r, th) {
        e.push(("rho", x));
    }
    if let Some(Some(x)) = a.r.first() {
        e.push(("r_first", *x));
    }
    if let Some(Some(x)) = a.r.last() {
        e.push(("r_last", *x));
    }
    if let Some(m) = a.r.iter().flatten().copied().reduce(f64::min) {
        e.push(("r_min", m));
    }
    e.push(("att", a.att as f64));
    e
}

fn finding(rule: &'static str, severity: Severity, loc: Option<(u32, u32, u32)>, evidence: Ev, occurrences: usize, advice: &'static str) -> Finding {
    Finding {
        rule,
        severity,
        step: loc.map(|l| l.0),
        inc: loc.map(|l| l.1),
        att: loc.map(|l| l.2),
        evidence,
        occurrences,
        advice,
    }
}

/// Diagnoses convergence behaviour. `step = None` considers all steps; the window is the last
/// `last_increments` increments (0 = all) of the considered data.
pub fn diagnose(
    sta: &[StaRow],
    cvg: &[CvgRow],
    ctx: &DiagCtx,
    th: &Thresholds,
    step: Option<u32>,
    last_increments: usize,
) -> Diagnosis {
    let sel = |s: u32| step.map_or(true, |x| x == s);
    let sta_f: Vec<&StaRow> = sta.iter().filter(|r| sel(r.step)).collect();
    let cvg_f: Vec<&CvgRow> = cvg.iter().filter(|r| sel(r.step)).collect();
    let json_f: Vec<(u32, bool)> = ctx.json_steps.iter().copied().filter(|(s, _)| sel(*s)).collect();
    let job_failed = ctx.exit_failed || json_f.iter().any(|(_, c)| !c);

    let mut cands: Vec<Cand> = Vec::new();
    let mut messages: Vec<String> = Vec::new();

    scan_stdout(ctx, th, &mut cands, &mut messages);

    if sta.is_empty() && cvg.is_empty() && ctx.json_steps.is_empty() {
        cands.push(Cand { f: finding("N1", Severity::Info, None, vec![], 1, ""), score: 0.0 });
    }

    // Index the telemetry.
    let sta_keys: HashSet<(u32, u32, u32)> = sta_f.iter().map(|r| (r.step, r.inc, r.att)).collect();
    let mut sta_inc: BTreeMap<(u32, u32), &StaRow> = BTreeMap::new();
    for r in &sta_f {
        let e = sta_inc.entry((r.step, r.inc)).or_insert(r);
        if r.att >= e.att {
            *e = r;
        }
    }
    let mut groups: BTreeMap<(u32, u32, u32), Vec<&CvgRow>> = BTreeMap::new();
    for r in &cvg_f {
        groups.entry((r.step, r.inc, r.att)).or_default().push(r);
    }
    let mut max_att: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    for &(s, i, a) in groups.keys().chain(sta_keys.iter()) {
        let e = max_att.entry((s, i)).or_insert(a);
        *e = (*e).max(a);
    }

    let n_groups = groups.len();
    let mut attempts: Vec<Attempt> = Vec::new();
    for (idx, (&(s, i, a), rows)) in groups.iter().enumerate() {
        let converged = sta_keys.contains(&(s, i, a));
        let later = max_att[&(s, i)] > a;
        let status = if converged {
            Status::Converged
        } else if later || (job_failed && idx + 1 == n_groups) {
            Status::Failed
        } else {
            Status::Open
        };
        attempts.push(Attempt {
            step: s,
            inc: i,
            att: a,
            r: rows.iter().map(|r| r.resid_force).collect(),
            c: rows.iter().map(|r| r.corr_disp).collect(),
            flux: rows.iter().map(|r| r.resid_flux).collect(),
            temp: rows.iter().map(|r| r.corr_temp).collect(),
            n: rows.iter().map(|r| r.contact_el).collect(),
            status,
            later_converged: sta_inc.get(&(s, i)).map_or(false, |r| r.att > a),
        });
    }

    // Window: last N increments of the selection.
    let all_incs: BTreeSet<(u32, u32)> = max_att.keys().copied().collect();
    let mut win: Vec<(u32, u32)> = all_incs.into_iter().collect();
    if last_increments > 0 && win.len() > last_increments {
        win.drain(..win.len() - last_increments);
    }
    let in_win: HashSet<(u32, u32)> = win.iter().copied().collect();
    let wa: Vec<&Attempt> = attempts.iter().filter(|a| in_win.contains(&(a.step, a.inc))).collect();

    // Per-attempt rules: C1, C2, D1, D2, D3.
    for a in &wa {
        let loc = Some((a.step, a.inc, a.att));
        let k = a.k();

        // C1
        if k >= th.chatter_k_min {
            let w = th.chatter_window.min(k);
            let nt: Vec<i64> = a.n[k - w..].iter().map(|&x| x as i64).collect();
            let rt: Vec<Option<f64>> = a.r[k - w..].to_vec();
            let mut flips = 0usize;
            let mut prev_sign = 0i64;
            for p in nt.windows(2) {
                let s = (p[1] - p[0]).signum();
                if s != 0 {
                    if prev_sign != 0 && s != prev_sign {
                        flips += 1;
                    }
                    prev_sign = s;
                }
            }
            let nmin = *nt.iter().min().unwrap() as f64;
            let nmax = *nt.iter().max().unwrap() as f64;
            let mean = nt.iter().sum::<i64>() as f64 / nt.len() as f64;
            let span_ok = nmax - nmin >= th.chatter_span_abs.max(th.chatter_span_rel * mean);
            let stagnates = if rt.iter().all(|x| x.is_some()) {
                let v: Vec<f64> = rt.iter().flatten().copied().collect();
                let (mn, mx) = (v.iter().cloned().fold(f64::INFINITY, f64::min), v.iter().cloned().fold(0.0, f64::max));
                mx > 0.0 && mn / mx > th.chatter_stagnation
            } else {
                false
            };
            if flips >= th.chatter_flips_min && span_ok && stagnates {
                let sev = if a.status == Status::Failed { Severity::Error } else { Severity::Warn };
                let mut ev = att_evidence(a, th);
                ev.push(("n_min", nmin));
                ev.push(("n_max", nmax));
                ev.push(("flips", flips as f64));
                cands.push(Cand { f: finding("C1", sev, loc, ev, 1, ADV_CHATTER), score: flips as f64 });
            }
        }

        // C2
        if k >= th.plateau_k_min && a.n.iter().all(|&x| x > 0) {
            let rt = &a.r[k - th.plateau_window.min(k)..];
            if rt.iter().all(|x| x.is_some()) {
                let v: Vec<f64> = rt.iter().flatten().copied().collect();
                let mn = v.iter().cloned().fold(f64::INFINITY, f64::min);
                let mx = v.iter().cloned().fold(0.0, f64::max);
                if mn > th.plateau_floor_pct && mx <= th.plateau_factor * mn {
                    let sev = if a.status == Status::Failed { Severity::Error } else { Severity::Warn };
                    let mut ev = att_evidence(a, th);
                    ev.push(("n_min", *a.n.iter().min().unwrap() as f64));
                    ev.push(("n_max", *a.n.iter().max().unwrap() as f64));
                    cands.push(Cand { f: finding("C2", sev, loc, ev, 1, ADV_CHATTER), score: mn });
                }
            }
        }

        // D1: two consecutive iterations (both >= d1_k_min) growing over the previous two.
        let grows = |i: usize| -> bool {
            match (a.r[i], a.r[i - 1], a.r[i - 2]) {
                (Some(x), Some(y), Some(z)) => x > y && x > z,
                _ => false,
            }
        };
        let first = th.d1_k_min.max(3); // 1-based k -> 0-based index k-1
        if k > first && (first - 1..k - 1).any(|i| grows(i) && grows(i + 1)) {
            let sev = if a.status != Status::Converged && !a.later_converged { Severity::Error } else { Severity::Warn };
            let mn = a.r.iter().flatten().copied().reduce(f64::min).unwrap_or(f64::NAN);
            let last = a.r.last().copied().flatten().unwrap_or(f64::NAN);
            let g = if mn > 0.0 { last / mn } else { 0.0 };
            cands.push(Cand { f: finding("D1", sev, loc, att_evidence(a, th), 1, ADV_DIVERGED), score: if g.is_finite() { g } else { 0.0 } });
        }

        // D2: blow-up relative to the minimum of earlier iterations, or absolute.
        let mut run_min: Option<f64> = None;
        let mut worst: Option<f64> = None;
        for x in a.r.iter() {
            if let Some(x) = *x {
                let rel = run_min.map_or(false, |m| m > 0.0 && x > th.d2_floor_pct && x >= th.d2_factor * m);
                if x > th.d2_abs || rel {
                    let ratio = run_min.filter(|m| *m > 0.0).map_or(x, |m| x / m);
                    worst = Some(worst.map_or(ratio, |w: f64| w.max(ratio)));
                }
                run_min = Some(run_min.map_or(x, |m| m.min(x)));
            }
        }
        if let Some(w) = worst {
            cands.push(Cand { f: finding("D2", Severity::Error, loc, att_evidence(a, th), 1, ADV_DIVERGED), score: w });
        }

        // D3: missing (NaN / overflow) value after the first iteration.
        let bad = (1..k).any(|i| a.r[i].is_none() || a.c[i].is_none() || a.flux[i].is_none() || a.temp[i].is_none());
        if bad {
            cands.push(Cand { f: finding("D3", Severity::Error, loc, att_evidence(a, th), 1, ADV_DIVERGED), score: 0.0 });
        }
    }

    // W1 / W2: slow convergence over the window.
    {
        let mut w1: Option<(&Attempt, usize)> = None;
        let mut w2: Option<(&Attempt, f64)> = None;
        let (mut c1, mut c2) = (0usize, 0usize);
        for &inc in &win {
            let mut hit1 = false;
            let mut hit2 = false;
            for a in wa.iter().filter(|a| (a.step, a.inc) == inc && a.status == Status::Converged) {
                if a.att == 1 && a.k() > th.slow_k {
                    hit1 = true;
                    if w1.map_or(true, |(_, k)| a.k() >= k) {
                        w1 = Some((a, a.k()));
                    }
                }
                if let Some(rh) = rho(&a.r, th) {
                    if rh > th.slow_rho_lo && rh < th.slow_rho_hi && a.k() >= th.slow_rho_k_min {
                        hit2 = true;
                        if w2.map_or(true, |(_, x)| rh >= x) {
                            w2 = Some((a, rh));
                        }
                    }
                }
            }
            c1 += hit1 as usize;
            c2 += hit2 as usize;
        }
        if c1 >= th.slow_min_incs {
            let a = w1.unwrap().0;
            cands.push(Cand { f: finding("W1", Severity::Warn, Some((a.step, a.inc, a.att)), att_evidence(a, th), c1, ADV_SLOW), score: a.k() as f64 });
        }
        if c2 >= th.slow_min_incs {
            let (a, rh) = w2.unwrap();
            cands.push(Cand { f: finding("W2", Severity::Warn, Some((a.step, a.inc, a.att)), att_evidence(a, th), c2, ADV_SLOW), score: rh });
        }
    }

    // B1 / B2 / B3 / B4: cutbacks.
    {
        let atts: Vec<((u32, u32), u32)> = win.iter().map(|k| (*k, max_att[k])).collect();
        let cut: Vec<&((u32, u32), u32)> = atts.iter().filter(|(_, a)| *a > 1).collect();
        if !cut.is_empty() {
            let total: u32 = cut.iter().map(|(_, a)| a - 1).sum();
            let worst = cut.iter().fold(cut[0], |w, c| if c.1 >= w.1 { c } else { w });
            let ev: Ev = vec![("att", worst.1 as f64), ("cutbacks", total as f64)];
            cands.push(Cand { f: finding("B1", Severity::Warn, Some(((worst.0).0, (worst.0).1, worst.1)), ev, cut.len(), ADV_CUTBACK), score: worst.1 as f64 });
        }

        // B2: runs of consecutive increments with att > 1.
        let (mut run, mut best, mut best_end, mut runs) = (0usize, 0usize, None, 0usize);
        for (idx, (k, a)) in atts.iter().enumerate() {
            if *a > 1 {
                run += 1;
                if run == th.loop_consecutive {
                    runs += 1;
                }
                if run >= best {
                    best = run;
                    best_end = Some((idx, *k));
                }
            } else {
                run = 0;
            }
        }
        if let (true, Some((idx, k))) = (best >= th.loop_consecutive, best_end) {
            let ev: Ev = vec![("consecutive", best as f64), ("att", atts[idx].1 as f64)];
            cands.push(Cand { f: finding("B2", Severity::Error, Some((k.0, k.1, atts[idx].1)), ev, runs, ADV_LOOP), score: best as f64 });
        }

        // B3: any increment reaching att >= loop_att.
        let deep: Vec<&((u32, u32), u32)> = atts.iter().filter(|(_, a)| *a >= th.loop_att).collect();
        if !deep.is_empty() {
            let worst = deep.iter().fold(deep[0], |w, c| if c.1 >= w.1 { c } else { w });
            let ev: Ev = vec![("att", worst.1 as f64)];
            cands.push(Cand { f: finding("B3", Severity::Error, Some(((worst.0).0, (worst.0).1, worst.1)), ev, deep.len(), ADV_LOOP), score: worst.1 as f64 });
        }

        // B4: increment size collapsed relative to the step maximum, with recent cutbacks.
        let latest = win.iter().rev().find_map(|k| sta_inc.get(k).copied());
        if let Some(l) = latest {
            let step_max = sta_f.iter().filter(|r| r.step == l.step && r.inc_time.is_finite()).map(|r| r.inc_time).fold(0.0, f64::max);
            let tail = &atts[atts.len().saturating_sub(th.shrink_window)..];
            let cutbacks: u32 = tail.iter().map(|(_, a)| a.saturating_sub(1)).sum();
            if step_max > 0.0 && l.inc_time <= th.shrink_ratio * step_max && cutbacks >= th.shrink_cutbacks {
                let ev: Ev = vec![("inc_time", l.inc_time), ("inc_time_max", step_max), ("cutbacks", cutbacks as f64), ("att", l.att as f64)];
                cands.push(Cand { f: finding("B4", Severity::Error, Some((l.step, l.inc, l.att)), ev, 1, ADV_LOOP), score: step_max / l.inc_time.max(f64::MIN_POSITIVE) });
            }
        }
    }

    // D4: the job failed and the last attempt did not converge.
    if job_failed && (!sta_f.is_empty() || !cvg_f.is_empty()) {
        match attempts.last() {
            Some(a) if a.status == Status::Converged => {}
            Some(a) => cands.push(Cand { f: finding("D4", Severity::Error, Some((a.step, a.inc, a.att)), att_evidence(a, th), 1, ADV_DIVERGED), score: 0.0 }),
            None => cands.push(Cand { f: finding("D4", Severity::Error, None, vec![], 1, ADV_DIVERGED), score: 0.0 }),
        }
    }

    // L1 / S1 only when nothing worse was found.
    if cands.iter().all(|c| c.f.severity == Severity::Info && c.f.rule == "N1") {
        let sta_win: Vec<&StaRow> = win.iter().filter_map(|k| sta_inc.get(k).copied()).collect();
        if cvg_f.is_empty() {
            let json_ok = !json_f.is_empty() && json_f.iter().all(|(_, c)| *c);
            let sta_linear = !sta_f.is_empty() && sta_f.iter().all(|r| r.att == 1 && r.itrs <= 1);
            if json_ok || sta_linear {
                cands.push(Cand { f: finding("L1", Severity::Info, None, vec![], 1, ""), score: 0.0 });
            }
        } else if !wa.is_empty() {
            let rhos: Vec<f64> = wa.iter().filter_map(|a| rho(&a.r, th)).collect();
            let kmax = wa.iter().map(|a| a.k()).max().unwrap_or(0);
            // The last increment of a step may legitimately be shorter (it lands on the step end), so it is not compared.
            let monotone = (1..sta_win.len()).all(|i| {
                let (p, q) = (sta_win[i - 1], sta_win[i]);
                let last_of_step = i + 1 == sta_win.len() || sta_win[i + 1].step != q.step;
                p.step != q.step || last_of_step || q.inc_time >= p.inc_time
            });
            let all_att1 = win.iter().all(|k| max_att[k] == 1);
            if all_att1 && kmax <= th.slow_k && rhos.iter().all(|r| *r <= th.smooth_rho_max) && monotone {
                let mut ev: Ev = vec![("K", kmax as f64)];
                if let Some(m) = rhos.iter().copied().reduce(f64::max) {
                    ev.push(("rho", m));
                }
                ev.push(("att", 1.0));
                cands.push(Cand { f: finding("S1", Severity::Info, None, ev, win.len(), ""), score: 0.0 });
            }
        }
    }

    let findings = merge(cands);
    let verdict = verdict_of(&findings, !sta.is_empty() || !cvg.is_empty());
    Diagnosis { verdict, findings, solver_messages: messages }
}

/// Merges instances of the same rule: summed occurrences, worst instance (severity, then score; later wins ties) as location.
fn merge(cands: Vec<Cand>) -> Vec<Finding> {
    let mut order: Vec<&'static str> = Vec::new();
    let mut best: BTreeMap<&'static str, (Cand, usize)> = BTreeMap::new();
    for c in cands {
        let rule = c.f.rule;
        match best.remove(rule) {
            None => {
                order.push(rule);
                let occ = c.f.occurrences;
                best.insert(rule, (c, occ));
            }
            Some((mut b, total)) => {
                let total = total + c.f.occurrences;
                if (c.f.severity, c.score) >= (b.f.severity, b.score) {
                    b = c;
                }
                best.insert(rule, (b, total));
            }
        }
    }
    order
        .into_iter()
        .map(|r| {
            let (mut b, total) = best.remove(r).unwrap();
            b.f.occurrences = total;
            b.f
        })
        .collect()
}

fn verdict_of(findings: &[Finding], have_telemetry: bool) -> Verdict {
    let mut v: Option<Verdict> = None;
    for f in findings {
        let cand = match f.rule {
            "N1" => Verdict::NoTelemetry,
            "L1" => Verdict::Linear,
            "S1" => Verdict::Smooth,
            "W1" | "W2" => Verdict::Slow,
            "C1" | "C2" => Verdict::ContactChatter,
            "B1" => Verdict::Cutback,
            "B2" | "B3" | "B4" => Verdict::CutbackLoop,
            // A D1 whose increment later converged supports B1 but does not make the run DIVERGED.
            "D1" if f.severity != Severity::Error => continue,
            _ => Verdict::Diverged,
        };
        if v.map_or(true, |x| cand.rank() > x.rank()) {
            v = Some(cand);
        }
    }
    // No rule fired: SMOOTH when there is telemetry, otherwise NO_TELEMETRY.
    v.unwrap_or(if have_telemetry { Verdict::Smooth } else { Verdict::NoTelemetry })
}

/// Stdout patterns of spec 4.4 (case-insensitive substring match on the last lines).
fn scan_stdout(ctx: &DiagCtx, th: &Thresholds, cands: &mut Vec<Cand>, messages: &mut Vec<String>) {
    let lines: Vec<&str> = ctx.stdout_tail.lines().collect();
    let lines = &lines[lines.len().saturating_sub(th.stdout_lines)..];
    let mut d5 = 0usize;
    let mut errors = 0usize;
    let push = |m: String, messages: &mut Vec<String>| {
        if !messages.contains(&m) {
            messages.push(m);
        }
    };
    for line in lines {
        let l = line.to_lowercase();
        let mut hit = false;
        if l.contains("increment size smaller than minimum") {
            hit = true;
            push("time increment fell below the minimum".to_string(), messages);
        }
        if l.contains("too many attempts") {
            hit = true;
            push(line.trim().to_string(), messages);
        }
        if l.contains("*error") {
            let t = line.trim().to_string();
            if errors < th.max_error_messages && !messages.contains(&t) {
                errors += 1;
                messages.push(t);
            }
            hit |= ctx.exit_failed;
        }
        if l.contains("best solution and residuals are in the frd file") {
            push(line.trim().to_string(), messages);
            push("hint: inspect the last step in the .frd file".to_string(), messages);
        }
        d5 += hit as usize;
    }
    if d5 > 0 {
        cands.push(Cand { f: finding("D5", Severity::Error, None, vec![], d5, ADV_DIVERGED), score: 0.0 });
    }
}
