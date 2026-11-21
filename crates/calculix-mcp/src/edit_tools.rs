//! Deck editing tool: `inp_edit`.

use crate::error::ToolError;
use crate::paths::Roots;
use cgx_core::{plan_edit, CardAddress, EditError, EditOp, EditPlan, FileRole, InpScan};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
const DIFF_CAP_LINES: usize = 200;
const DIFF_CONTEXT: usize = 3;

/// Accept a string, number, boolean or null where a parameter value is expected.
fn value_or_null<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    match serde_json::Value::deserialize(d)? {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(s) => Ok(Some(s)),
        serde_json::Value::Number(n) => Ok(Some(n.to_string())),
        serde_json::Value::Bool(b) => Ok(Some(if b { "YES" } else { "NO" }.to_string())),
        other => Err(serde::de::Error::custom(format!("expected a string or null, got {other}"))),
    }
}

fn string_like<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    value_or_null(d)?.ok_or_else(|| serde::de::Error::custom("expected a string"))
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ParamFilter {
    /// Parameter name, case-insensitive
    pub name: String,
    /// Required value, case-insensitive; "" matches a bare flag
    #[serde(deserialize_with = "string_like")]
    #[schemars(with = "String")]
    pub value: String,
}

/// Address of one keyword card. Resolved against the original deck.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CardIn {
    /// Keyword, case-insensitive, leading * optional: "FREQUENCY", "*boundary"
    pub keyword: String,
    /// 0 = model level only, n = inside step n, omit = anywhere
    pub step: Option<u32>,
    /// 0-based index among the matches; required when several cards match
    pub index: Option<usize>,
    /// Only cards having this parameter value
    pub param: Option<ParamFilter>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OpIn {
    /// Set a parameter of a card header; null removes it, "" makes it a bare flag. A missing parameter is appended as ", PARAM=value"
    SetParam {
        card: CardIn,
        /// Parameter name
        param: String,
        /// New value (string or number); null removes the parameter
        #[serde(deserialize_with = "value_or_null")]
        #[schemars(required, with = "Option<String>")]
        value: Option<String>,
    },
    /// Replace all data lines of a card (comments between its data lines go too)
    ReplaceData {
        card: CardIn,
        /// New data lines, without keyword lines
        lines: Vec<String>,
    },
    /// Insert text after a card and its data lines
    InsertAfter {
        card: CardIn,
        /// Must start with a * keyword line or a ** comment
        text: String,
    },
    /// Delete a card, its header continuation lines and its data lines
    DeleteCard { card: CardIn },
    /// Append a step after the last *END STEP of the root deck
    AppendStep {
        /// Must contain *STEP and *END STEP
        text: String,
    },
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InpEditIn {
    /// Path to the root .inp deck (never modified)
    pub path: String,
    /// Operations, applied in order; every card address is resolved against the original deck, and two ops may not touch the same card
    pub ops: Vec<OpIn>,
    /// New root deck. Default: <stem>_edit.inp beside the deck. Must be in the deck's directory when the deck has includes
    pub output_path: Option<String>,
    /// Replace existing output files (default false)
    #[serde(default)]
    pub overwrite: bool,
    /// Return the diff and the re-check without writing anything
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FileOut {
    pub path: String,
    /// root | include
    pub role: String,
    pub bytes: usize,
}

#[derive(Debug, Serialize, JsonSchema, PartialEq)]
pub struct CountDelta {
    pub before: usize,
    pub after: usize,
}

#[derive(Debug, Serialize, JsonSchema, PartialEq)]
pub struct StepPlanOut {
    pub n: u32,
    pub params: String,
    pub procedure: Option<String>,
    /// Keyword -> count inside the step
    pub cards: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CheckOut {
    /// False when the edited deck could not be scanned; see `error`
    pub ok: bool,
    pub error: Option<String>,
    pub flags_before: Vec<String>,
    pub flags_after: Vec<String>,
    /// Model-level keyword -> counts, only keywords whose count changed
    pub census_delta: BTreeMap<String, CountDelta>,
    /// The step plan of the edited deck, only when it differs from the original
    pub step_plan: Option<Vec<StepPlanOut>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct InpEditOut {
    /// False for a dry run: nothing was written
    pub written: bool,
    /// Files created (or that would be created), root first
    pub files_written: Vec<FileOut>,
    /// One line per op: what was edited, with file:line in the original deck
    pub applied: Vec<String>,
    /// Unified diff per file against its original, context 3, at most 200 lines in total
    pub diff: String,
    pub diff_lines: usize,
    pub diff_truncated: bool,
    pub check: CheckOut,
}

fn card_addr(c: CardIn) -> CardAddress {
    CardAddress { keyword: c.keyword, step: c.step, index: c.index, param: c.param.map(|p| (p.name, p.value)) }
}

fn to_op(op: OpIn) -> EditOp {
    match op {
        OpIn::SetParam { card, param, value } => EditOp::SetParam { card: card_addr(card), param, value },
        OpIn::ReplaceData { card, lines } => EditOp::ReplaceData { card: card_addr(card), lines },
        OpIn::InsertAfter { card, text } => EditOp::InsertAfter { card: card_addr(card), text },
        OpIn::DeleteCard { card } => EditOp::DeleteCard { card: card_addr(card) },
        OpIn::AppendStep { text } => EditOp::AppendStep { text },
    }
}

/// Remove the absolute root prefix from paths embedded in a message.
fn relative(roots: &Roots, msg: &str) -> String {
    let prefix = format!("{}{}", roots.first().display(), std::path::MAIN_SEPARATOR);
    msg.replace(&prefix, "")
}

fn map_error(roots: &Roots, e: EditError) -> ToolError {
    match e {
        EditError::UnknownCard { message, valid } => ToolError::new("UNKNOWN_CARD", relative(roots, &message))
            .valid(valid.iter().map(|v| relative(roots, v)).collect())
            .hint("Call inp_cards to list the cards of the deck."),
        EditError::AmbiguousCard { message, candidates } => ToolError::new("AMBIGUOUS_CARD", relative(roots, &message))
            .valid(candidates.iter().map(|c| format!("{}:{}  {}", roots.display(&c.file), c.line, c.header)).collect())
            .hint("Repeat the call with index (0-based, in deck order), step, or param to select one card."),
        EditError::Invalid { message, valid } => ToolError::invalid(relative(roots, &message)).valid(valid),
        EditError::Conflict(m) => ToolError::new("CONFLICTING_OPS", relative(roots, &m)),
        EditError::OutputExists(p) => ToolError::new("RESULTS_EXIST", format!("{} already exists", roots.display(&p)))
            .hint("Pass overwrite=true or choose another output_path."),
        EditError::Deck(e) => ToolError::new("DECK_INVALID", relative(roots, &e.to_string())),
        EditError::Io(e) => ToolError::internal(relative(roots, &e.to_string())),
    }
}

fn step_plan(scan: &InpScan) -> Vec<StepPlanOut> {
    scan.steps()
        .into_iter()
        .map(|s| StepPlanOut { n: s.number, params: s.params, procedure: s.procedure, cards: s.cards })
        .collect()
}

fn census_delta(before: &InpScan, after: &InpScan) -> BTreeMap<String, CountDelta> {
    let (b, a) = (before.census(), after.census());
    let mut out = BTreeMap::new();
    for k in b.keys().chain(a.keys()) {
        let (x, y) = (b.get(k).copied().unwrap_or(0), a.get(k).copied().unwrap_or(0));
        if x != y {
            out.insert(k.clone(), CountDelta { before: x, after: y });
        }
    }
    out
}

fn check(before: &InpScan, after: Result<InpScan, EditError>, roots: &Roots) -> CheckOut {
    let flags_before: Vec<String> = before.flags().into_iter().map(String::from).collect();
    match after {
        Ok(a) => {
            let (sb, sa) = (step_plan(before), step_plan(&a));
            CheckOut {
                ok: true,
                error: None,
                flags_before,
                flags_after: a.flags().into_iter().map(String::from).collect(),
                census_delta: census_delta(before, &a),
                step_plan: (sb != sa).then_some(sa),
            }
        }
        Err(e) => CheckOut {
            ok: false,
            error: Some(relative(roots, &e.to_string())),
            flags_after: Vec::new(),
            flags_before,
            census_delta: BTreeMap::new(),
            step_plan: None,
        },
    }
}

fn diff_of(roots: &Roots, plan: &EditPlan) -> (String, usize, bool) {
    let all: String = plan
        .files
        .iter()
        .map(|f| f.unified_diff(&format!("a/{}", roots.display(&f.original)), &format!("b/{}", roots.display(&f.path)), DIFF_CONTEXT))
        .collect();
    let lines: Vec<&str> = all.lines().collect();
    let total = lines.len();
    if total <= DIFF_CAP_LINES {
        return (all.clone(), total, false);
    }
    let mut out = lines[..DIFF_CAP_LINES].join("\n");
    out.push_str(&format!("\n... diff truncated: {} more lines\n", lines.len() - DIFF_CAP_LINES));
    (out, total, true)
}

pub fn edit(roots: &Roots, p: InpEditIn) -> Result<InpEditOut, ToolError> {
    let deck = roots.existing(&p.path, &["inp"])?;
    let default_out = deck.with_file_name(format!("{}_edit.inp", deck.file_stem().and_then(|s| s.to_str()).unwrap_or("deck")));
    let output = match &p.output_path {
        Some(raw) => roots.creatable(raw, &["inp"])?,
        None => roots.creatable(&default_out.to_string_lossy(), &["inp"])?,
    };
    let ops: Vec<EditOp> = p.ops.into_iter().map(to_op).collect();
    let plan = plan_edit(&deck, &ops, &output).map_err(|e| map_error(roots, e))?;

    // Every file read or created must be inside the roots.
    for f in &plan.files {
        if !roots.contains(&f.original) {
            return Err(ToolError::new("PATH_OUTSIDE_ROOTS", format!("{} is outside the allowed roots", f.original.display()))
                .hint("Edit decks whose edited files are inside the server roots."));
        }
        roots.creatable(&f.path.to_string_lossy(), &[])?;
    }
    if !p.overwrite {
        if let Some(f) = plan.files.iter().find(|f| f.path.exists()) {
            return Err(map_error(roots, EditError::OutputExists(f.path.clone())));
        }
    }

    let after = if p.dry_run {
        plan.scan_preview()
    } else {
        plan.write(p.overwrite).map_err(|e| map_error(roots, e))?;
        plan.scan_written()
    };
    let (diff, diff_lines, diff_truncated) = diff_of(roots, &plan);
    Ok(InpEditOut {
        written: !p.dry_run,
        files_written: plan
            .files
            .iter()
            .map(|f| FileOut {
                path: roots.display(&f.path),
                role: if f.role == FileRole::Root { "root" } else { "include" }.to_string(),
                bytes: f.new_bytes().len(),
            })
            .collect(),
        applied: plan
            .applied
            .iter()
            .map(|a| format!("op {} {}: {}:{}  {}", a.op, a.kind, roots.display(&a.file), a.line, a.header).trim_end().to_string())
            .collect(),
        diff,
        diff_lines,
        diff_truncated,
        check: check(&plan.before, after, roots),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cgx_core::{read_card_data, scan_cards, ScanLimits};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn sandbox() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("calculix-mcp-edit-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("root")).unwrap();
        dir
    }

    const DECK: &str = "*NODE\n1,0,0,0\n*ELEMENT, TYPE=B31\n1,1,1\n*BOUNDARY\n1,1,6\n*STEP\n*FREQUENCY\n5\n*END STEP\n";

    fn setup(deck: &str) -> (PathBuf, Roots) {
        let dir = sandbox();
        std::fs::write(dir.join("root/deck.inp"), deck).unwrap();
        let roots = Roots::new(vec![dir.join("root")]).unwrap();
        (dir, roots)
    }

    fn input(json: serde_json::Value) -> InpEditIn {
        serde_json::from_value(json).unwrap()
    }

    fn freq_op(lines: &[&str]) -> serde_json::Value {
        serde_json::json!({"op": "replace_data", "card": {"keyword": "FREQUENCY"}, "lines": lines})
    }

    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut m = BTreeMap::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_file() {
                m.insert(p.clone(), std::fs::read(&p).unwrap());
            } else if p.is_dir() {
                m.extend(snapshot(&p));
            }
        }
        m
    }

    #[test]
    fn response_shape_and_census_delta() {
        let (dir, roots) = setup(DECK);
        let ops = serde_json::json!([
            freq_op(&["12"]),
            {"op": "insert_after", "card": {"keyword": "BOUNDARY"}, "text": "*CLOAD\n1,2,-1."},
            {"op": "append_step", "text": "*STEP\n*STATIC\n*END STEP"},
        ]);
        let out = edit(&roots, input(serde_json::json!({"path": "deck.inp", "ops": ops}))).unwrap();
        assert!(out.written);
        assert_eq!((out.files_written.len(), out.files_written[0].path.as_str(), out.files_written[0].role.as_str()), (1, "deck_edit.inp", "root"));
        assert_eq!(out.files_written[0].bytes, std::fs::metadata(dir.join("root/deck_edit.inp")).unwrap().len() as usize);
        assert_eq!(out.applied.len(), 3);
        assert!(out.applied[0].starts_with("op 0 replace_data: deck.inp:8  *FREQUENCY"), "{:?}", out.applied);
        assert!(out.diff.starts_with("--- a/deck.inp\n+++ b/deck_edit.inp\n@@"), "{}", out.diff);
        assert!(out.diff.contains("-5\n+12\n") && out.diff.contains("+*CLOAD\n"), "{}", out.diff);
        assert!(!out.diff_truncated);
        // The census counts model-level cards: CLOAD was inserted at model level, the new step adds a step.
        assert_eq!(out.check.census_delta["CLOAD"], CountDelta { before: 0, after: 1 });
        assert_eq!(out.check.census_delta.len(), 1);
        assert!(out.check.ok && out.check.flags_before == out.check.flags_after);
        let plan = out.check.step_plan.unwrap();
        assert_eq!((plan.len(), plan[1].procedure.as_deref()), (2, Some("STATIC")));
        // The original deck is untouched.
        assert_eq!(std::fs::read_to_string(dir.join("root/deck.inp")).unwrap(), DECK);
        let scan = scan_cards(&dir.join("root/deck_edit.inp"), &ScanLimits::default()).unwrap();
        assert_eq!(read_card_data(scan.find("FREQUENCY", None)[0], 3).unwrap(), ["12"]);
        // A pure data edit changes no count and leaves the step plan out.
        let _ = std::fs::remove_file(dir.join("root/deck_edit.inp"));
        let out = edit(&roots, input(serde_json::json!({"path": "deck.inp", "ops": [freq_op(&["3"])]}))).unwrap();
        assert!(out.check.census_delta.is_empty() && out.check.step_plan.is_none());
    }

    #[test]
    fn dry_run_writes_nothing_and_matches_the_real_run() {
        let (dir, roots) = setup(DECK);
        let before = snapshot(&dir);
        let req = |dry: bool| input(serde_json::json!({"path": "deck.inp", "ops": [freq_op(&["12"]), {"op": "delete_card", "card": {"keyword": "BOUNDARY"}}], "dry_run": dry}));
        let dry = edit(&roots, req(true)).unwrap();
        assert!(!dry.written);
        assert_eq!(snapshot(&dir), before);
        let real = edit(&roots, req(false)).unwrap();
        assert_eq!(dry.diff, real.diff);
        assert_eq!(dry.check.census_delta, real.check.census_delta);
        assert_eq!(real.check.census_delta["BOUNDARY"], CountDelta { before: 1, after: 0 });
    }

    #[test]
    fn diff_is_capped_at_200_lines_with_a_marker() {
        let (_dir, roots) = setup(DECK);
        let lines: Vec<String> = (0..400).map(|i| format!("{i}")).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let out = edit(&roots, input(serde_json::json!({"path": "deck.inp", "ops": [freq_op(&refs)], "dry_run": true}))).unwrap();
        assert!(out.diff_truncated);
        assert_eq!(out.diff.lines().count(), 201);
        assert!(out.diff.lines().last().unwrap().starts_with("... diff truncated: "), "{}", out.diff.lines().last().unwrap());
        assert!(out.diff_lines > 400);
    }

    #[test]
    fn overwrite_is_refused_unless_requested() {
        let (dir, roots) = setup(DECK);
        let req = |overwrite: bool| input(serde_json::json!({"path": "deck.inp", "ops": [freq_op(&["12"])], "overwrite": overwrite}));
        edit(&roots, req(false)).unwrap();
        let e = edit(&roots, req(false)).unwrap_err();
        assert_eq!(e.code, "RESULTS_EXIST");
        let dry = edit(&roots, input(serde_json::json!({"path": "deck.inp", "ops": [freq_op(&["12"])], "dry_run": true}))).unwrap_err();
        assert_eq!(dry.code, "RESULTS_EXIST");
        std::fs::write(dir.join("root/deck_edit.inp"), "stale").unwrap();
        edit(&roots, req(true)).unwrap();
        assert!(std::fs::read_to_string(dir.join("root/deck_edit.inp")).unwrap().contains("12"));
        // The deck itself can never be the output.
        let same = input(serde_json::json!({"path": "deck.inp", "ops": [freq_op(&["1"])], "output_path": "deck.inp", "overwrite": true}));
        assert_eq!(edit(&roots, same).unwrap_err().code, "INVALID_ARGUMENT");
        assert_eq!(std::fs::read_to_string(dir.join("root/deck.inp")).unwrap(), DECK);
    }

    #[test]
    fn paths_outside_the_roots_are_rejected() {
        let (dir, roots) = setup(DECK);
        std::fs::write(dir.join("outside.inp"), DECK).unwrap();
        let e = edit(&roots, input(serde_json::json!({"path": "../outside.inp", "ops": [freq_op(&["1"])]}))).unwrap_err();
        assert_eq!(e.code, "PATH_OUTSIDE_ROOTS");
        let e = edit(&roots, input(serde_json::json!({"path": "deck.inp", "ops": [freq_op(&["1"])], "output_path": "../x.inp"}))).unwrap_err();
        assert_eq!(e.code, "PATH_OUTSIDE_ROOTS");
        assert!(!dir.join("x.inp").exists());
        let e = edit(&roots, input(serde_json::json!({"path": "deck.inp", "ops": [freq_op(&["1"])], "output_path": "o.txt"}))).unwrap_err();
        assert_eq!(e.code, "BAD_EXTENSION");
    }

    #[test]
    fn card_errors_carry_candidates() {
        let (_dir, roots) = setup("*NODE\n1,0,0,0\n*BOUNDARY\n1,1,6\n*BOUNDARY\n2,1,6\n*STEP\n*STATIC\n*END STEP\n");
        let run = |card: serde_json::Value| {
            edit(&roots, input(serde_json::json!({"path": "deck.inp", "ops": [{"op": "delete_card", "card": card}], "dry_run": true})))
        };
        let e = run(serde_json::json!({"keyword": "boundary"})).unwrap_err();
        assert_eq!(e.code, "AMBIGUOUS_CARD");
        assert_eq!(e.valid, ["deck.inp:3  *BOUNDARY", "deck.inp:5  *BOUNDARY"]);
        let e = run(serde_json::json!({"keyword": "CLOAD"})).unwrap_err();
        assert_eq!(e.code, "UNKNOWN_CARD");
        assert_eq!(e.valid[0], "BOUNDARY");
        assert!(run(serde_json::json!({"keyword": "boundary", "index": 1})).is_ok());
    }

    #[test]
    fn set_param_value_may_be_a_number_and_must_be_present() {
        let (_dir, roots) = setup(DECK);
        let ok = input(serde_json::json!({"path": "deck.inp", "dry_run": true, "ops": [{"op": "set_param", "card": {"keyword": "STEP"}, "param": "INC", "value": 50}]}));
        assert!(edit(&roots, ok).unwrap().diff.contains("+*STEP, INC=50\n"));
        let missing: Result<InpEditIn, _> =
            serde_json::from_value(serde_json::json!({"path": "d.inp", "ops": [{"op": "set_param", "card": {"keyword": "STEP"}, "param": "INC"}]}));
        assert!(missing.is_err(), "value must be given (null removes)");
    }

    // ---------- real decks, on copies ----------

    #[test]
    fn real_beam_modal_frequency_to_12_modes() {
        let src = Path::new("/Users/carlo/code/CalculiX/test/beam_modal.inp");
        if !src.exists() {
            return;
        }
        let (dir, roots) = setup("");
        std::fs::copy(src, dir.join("root/beam_modal.inp")).unwrap();
        let original = std::fs::read(dir.join("root/beam_modal.inp")).unwrap();
        let out = edit(
            &roots,
            input(serde_json::json!({"path": "beam_modal.inp", "ops": [freq_op(&["12"])]})),
        )
        .unwrap();
        eprintln!("{}", out.diff);
        assert!(out.diff.contains("-5\n+12\n"), "{}", out.diff);
        assert_eq!(std::fs::read(dir.join("root/beam_modal.inp")).unwrap(), original);
        let scan = scan_cards(&dir.join("root/beam_modal_edit.inp"), &ScanLimits::default()).unwrap();
        assert_eq!(read_card_data(scan.find("FREQUENCY", None)[0], 3).unwrap(), ["12"]);
        assert!(out.check.ok && out.check.census_delta.is_empty());
        // Optionally solve the edited copy: 12 modes must come out.
        let ccx = Path::new("/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/build_mumps/CalculiX");
        if ccx.exists() && std::env::var_os("INP_EDIT_SOLVE").is_some() {
            let work = dir.join("root");
            let status = std::process::Command::new(ccx).args(["-j", "beam_modal_edit"]).current_dir(&work).output().unwrap();
            assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
            let dat = std::fs::read_to_string(work.join("beam_modal_edit.dat")).unwrap();
            let frd = std::fs::read(work.join("beam_modal_edit.frd")).unwrap();
            // Each mode is written as several field blocks (DISP, STRESS, ...) with the same 100CL header.
            let text = String::from_utf8_lossy(&frd);
            let modes = text.lines().filter(|l| l.starts_with("  100CL")).collect::<std::collections::BTreeSet<_>>().len();
            eprintln!("solved: {} modes in .frd, .dat has {} lines", modes, dat.lines().count());
            assert_eq!(modes, 12);
        }
    }

    #[test]
    fn real_rotor_deck_set_param_and_append_step_reference_includes() {
        let src = Path::new("/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor");
        if !src.join("lavffcyc.inp").exists() {
            return;
        }
        let (dir, roots) = setup("");
        let work = dir.join("root");
        for name in ["lavffcyc.inp", "all.msh", "CYC1.nam", "CYC2.nam", "SPC_23.bou"] {
            std::fs::copy(src.join(name), work.join(name)).unwrap();
        }
        let before = snapshot(&work);
        let ops = serde_json::json!([
            {"op": "set_param", "card": {"keyword": "FREQUENCY"}, "param": "EIGENSOLVER", "value": "ARPACK"},
            {"op": "append_step", "text": "*STEP\n*STATIC\n*END STEP"},
        ]);
        let out = edit(&roots, input(serde_json::json!({"path": "lavffcyc.inp", "ops": ops}))).unwrap();
        eprintln!("{}\n{:?}\n{:?}", out.diff, out.files_written.iter().map(|f| (&f.path, f.bytes)).collect::<Vec<_>>(), out.check.step_plan);
        assert_eq!(out.files_written.len(), 1, "include files are referenced, not copied");
        assert!(!work.join("all_edit.msh").exists());
        assert!(out.check.ok && out.check.flags_before == out.check.flags_after);
        assert_eq!(out.check.step_plan.as_ref().map(Vec::len), Some(3));
        let text = std::fs::read_to_string(work.join("lavffcyc_edit.inp")).unwrap();
        assert!(text.contains("*include, input=all.msh") && text.contains("*FREQUENCY,EIGENSOLVER=ARPACK"));
        for (p, bytes) in &before {
            assert_eq!(&std::fs::read(p).unwrap(), bytes, "{} changed", p.display());
        }
        assert_eq!(snapshot(&work).len(), before.len() + 1);
    }
}
