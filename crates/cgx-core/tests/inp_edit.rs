//! `inp_edit`: located edits of decks, written to new files only.

use cgx_core::{plan_edit, read_card_data, scan_cards, CardAddress, EditError, EditOp, ScanLimits};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn sandbox() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("cgx-inp-edit-test-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const DECK: &str = "\
*HEADING
beam
*NODE
1,0.,0.,0.
2,1.,0.,0.
*ELEMENT, TYPE=B31, ELSET=EALL
1,1,2
*MATERIAL, NAME=STEEL
*ELASTIC
210000.,0.3
*STEP, NLGEOM, INC=100
*STATIC
0.1,1.
*BOUNDARY
1,1,6
*END STEP
*STEP, PERTURBATION
*FREQUENCY
5
*BOUNDARY
1,1,6
*END STEP
";

fn addr(kw: &str) -> CardAddress {
    CardAddress { keyword: kw.into(), ..Default::default() }
}

fn at(kw: &str, step: u32) -> CardAddress {
    CardAddress { keyword: kw.into(), step: Some(step), ..Default::default() }
}

fn run(root: &Path, ops: &[EditOp]) -> (cgx_core::EditPlan, String) {
    let out = root.with_file_name("out_edit.inp");
    let plan = plan_edit(root, ops, &out).unwrap();
    plan.write(false).unwrap();
    let text = String::from_utf8(std::fs::read(&out).unwrap()).unwrap();
    (plan, text)
}

fn deck_in(dir: &Path, text: &str) -> PathBuf {
    let p = dir.join("deck.inp");
    std::fs::write(&p, text).unwrap();
    p
}

/// All files under `dir`, with bytes.
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

fn err(root: &Path, ops: &[EditOp]) -> EditError {
    plan_edit(root, ops, &root.with_file_name("out_edit.inp")).unwrap_err()
}

#[test]
fn set_param_replaces_adds_and_removes() {
    let dir = sandbox();
    let root = deck_in(&dir, DECK);
    let step1 = |p: &str, v: Option<&str>| EditOp::SetParam { card: at("STEP", 1), param: p.into(), value: v.map(String::from) };
    // One op per card (several ops on one card are rejected), so chain the requests.
    let (_, t1) = run(&root, &[step1("inc", Some("200"))]);
    assert_eq!(t1.replace("INC=200", "INC=100"), DECK);
    let r2 = dir.join("deck2.inp");
    std::fs::write(&r2, &t1).unwrap();
    let out2 = dir.join("out2.inp");
    let p2 = plan_edit(&r2, &[step1("NLGEOM", None)], &out2).unwrap();
    p2.write(false).unwrap();
    let t2 = std::fs::read_to_string(&out2).unwrap();
    assert!(t2.contains("*STEP, INC=200\n"), "{t2}");
    let r3 = dir.join("deck3.inp");
    std::fs::write(&r3, &t2).unwrap();
    let out3 = dir.join("out3.inp");
    plan_edit(&r3, &[step1("name", Some("S1"))], &out3).unwrap().write(false).unwrap();
    let scan = scan_cards(&out3, &ScanLimits::default()).unwrap();
    let s = scan.find("STEP", Some(1))[0];
    assert_eq!(s.params, vec![("INC".to_string(), Some("200".to_string())), ("NAME".to_string(), Some("S1".to_string()))]);
}

#[test]
fn set_param_on_header_with_continuation_line() {
    let dir = sandbox();
    let deck = "*STEP, NLGEOM,\n   INC=100,\n   UNSYMM=YES\n*STATIC\n0.1,1.\n*END STEP\n";
    let root = deck_in(&dir, deck);
    let op = |p: &str, v: Option<&str>| vec![EditOp::SetParam { card: addr("STEP"), param: p.into(), value: v.map(String::from) }];

    let (_, t) = run(&root, &op("INC", Some("5")));
    assert_eq!(t, "*STEP, NLGEOM,\n   INC=5,\n   UNSYMM=YES\n*STATIC\n0.1,1.\n*END STEP\n");

    let _ = std::fs::remove_file(dir.join("out_edit.inp"));
    let (_, t) = run(&root, &op("UNSYMM", None)); // last continuation line: no dangling comma
    assert_eq!(t, "*STEP, NLGEOM,\n   INC=100\n*STATIC\n0.1,1.\n*END STEP\n");

    let _ = std::fs::remove_file(dir.join("out_edit.inp"));
    let (_, t) = run(&root, &op("INC", None)); // a middle line vanishes, header stays one card
    assert_eq!(t, "*STEP, NLGEOM,\n   UNSYMM=YES\n*STATIC\n0.1,1.\n*END STEP\n");

    let _ = std::fs::remove_file(dir.join("out_edit.inp"));
    let (_, t) = run(&root, &op("PERTURBATION", Some(""))); // bare flag appended to the last line
    assert_eq!(t, "*STEP, NLGEOM,\n   INC=100,\n   UNSYMM=YES, PERTURBATION\n*STATIC\n0.1,1.\n*END STEP\n");
    let scan = scan_cards(&dir.join("out_edit.inp"), &ScanLimits::default()).unwrap();
    assert_eq!(scan.find("STEP", None)[0].params.len(), 4);
    assert_eq!(scan.find("STATIC", None)[0].data_lines, 1);
}

#[test]
fn removing_a_missing_param_lists_the_present_ones() {
    let dir = sandbox();
    let root = deck_in(&dir, DECK);
    match err(&root, &[EditOp::SetParam { card: at("STEP", 1), param: "TIME".into(), value: None }]) {
        EditError::Invalid { valid, .. } => assert_eq!(valid, ["NLGEOM", "INC"]),
        e => panic!("{e}"),
    }
}

#[test]
fn replace_data_insert_after_delete_and_append_step() {
    let dir = sandbox();
    let root = deck_in(&dir, DECK);
    let ops = vec![
        EditOp::ReplaceData { card: addr("FREQUENCY"), lines: vec!["12".into()] },
        EditOp::InsertAfter { card: at("BOUNDARY", 1), text: "*CLOAD\n2,2,-1.".into() },
        EditOp::DeleteCard { card: CardAddress { keyword: "MATERIAL".into(), ..Default::default() } },
        EditOp::AppendStep { text: "*STEP\n*STATIC\n*END STEP".into() },
    ];
    // MATERIAL is followed by ELASTIC (its own card): only the MATERIAL line goes.
    let (plan, text) = run(&root, &ops);
    assert_eq!(plan.applied.len(), 4);
    let want = DECK
        .replace("*FREQUENCY\n5\n", "*FREQUENCY\n12\n")
        .replace("*BOUNDARY\n1,1,6\n*END STEP\n*STEP, PERT", "*BOUNDARY\n1,1,6\n*CLOAD\n2,2,-1.\n*END STEP\n*STEP, PERT")
        .replace("*MATERIAL, NAME=STEEL\n", "")
        + "*STEP\n*STATIC\n*END STEP\n";
    assert_eq!(text, want);
    let scan = scan_cards(&dir.join("out_edit.inp"), &ScanLimits::default()).unwrap();
    assert_eq!(read_card_data(scan.find("FREQUENCY", None)[0], 5).unwrap(), ["12"]);
    assert_eq!(scan.steps().len(), 3);
    assert_eq!(scan.find("CLOAD", Some(1)).len(), 1);
}

#[test]
fn replace_data_on_card_without_data_and_delete_with_data() {
    let dir = sandbox();
    let root = deck_in(&dir, "*HEADING\nx\n*NODE\n1,0,0,0\n2,1,0,0\n*ELSET, ELSET=A\n*STEP\n*STATIC\n*END STEP\n");
    let (_, t) = run(
        &root,
        &[
            EditOp::ReplaceData { card: addr("ELSET"), lines: vec!["1, 2".into()] },
            EditOp::DeleteCard { card: addr("NODE") },
        ],
    );
    assert_eq!(t, "*HEADING\nx\n*ELSET, ELSET=A\n1, 2\n*STEP\n*STATIC\n*END STEP\n");
}

#[test]
fn invalid_inputs_are_rejected() {
    let dir = sandbox();
    let root = deck_in(&dir, DECK);
    let bad_insert = EditOp::InsertAfter { card: addr("FREQUENCY"), text: "1,2,3".into() };
    assert!(matches!(err(&root, &[bad_insert]), EditError::Invalid { .. }));
    let bad_step = EditOp::AppendStep { text: "*STEP\n*STATIC".into() };
    assert!(matches!(err(&root, &[bad_step]), EditError::Invalid { .. }));
    let kw_in_data = EditOp::ReplaceData { card: addr("FREQUENCY"), lines: vec!["*NODE".into()] };
    assert!(matches!(err(&root, &[kw_in_data]), EditError::Invalid { .. }));
    assert!(matches!(err(&root, &[]), EditError::Invalid { .. }));
}

#[test]
fn unknown_and_ambiguous_addresses() {
    let dir = sandbox();
    let root = deck_in(&dir, DECK);
    match err(&root, &[EditOp::DeleteCard { card: addr("CONTACT PAIR") }]) {
        EditError::UnknownCard { valid, .. } => assert_eq!(&valid[..2], ["BOUNDARY", "END STEP"][..].iter().map(|s| s.to_string()).collect::<Vec<_>>().as_slice()),
        e => panic!("{e}"),
    }
    match err(&root, &[EditOp::DeleteCard { card: addr("BOUNDARY") }]) {
        EditError::AmbiguousCard { candidates, .. } => {
            assert_eq!(candidates.len(), 2);
            assert_eq!((candidates[0].line, candidates[0].header.as_str()), (14, "*BOUNDARY"));
        }
        e => panic!("{e}"),
    }
    // index, step and param disambiguate; an index out of range is an unknown card.
    let by_index = CardAddress { keyword: "boundary".into(), index: Some(1), ..Default::default() };
    let (_, t) = run(&root, &[EditOp::DeleteCard { card: by_index }]);
    assert_eq!(t.matches("*BOUNDARY").count(), 1);
    let _ = std::fs::remove_file(dir.join("out_edit.inp"));
    let by_param = CardAddress { keyword: "STEP".into(), param: Some(("perturbation".into(), "".into())), ..Default::default() };
    let (_, t) = run(&root, &[EditOp::SetParam { card: by_param, param: "NAME".into(), value: Some("B".into()) }]);
    assert!(t.contains("*STEP, PERTURBATION, NAME=B\n"));
    let _ = std::fs::remove_file(dir.join("out_edit.inp"));
    let by_value = CardAddress { keyword: "STEP".into(), param: Some(("inc".into(), "100".into())), ..Default::default() };
    let (_, t) = run(&root, &[EditOp::SetParam { card: by_value, param: "INC".into(), value: Some("7".into()) }]);
    assert!(t.contains("INC=7\n"));
    let far = CardAddress { keyword: "BOUNDARY".into(), index: Some(5), ..Default::default() };
    assert!(matches!(err(&root, &[EditOp::DeleteCard { card: far }]), EditError::UnknownCard { .. }));
}

#[test]
fn two_ops_on_the_same_card_are_rejected() {
    let dir = sandbox();
    let root = deck_in(&dir, DECK);
    let ops = [
        EditOp::ReplaceData { card: addr("FREQUENCY"), lines: vec!["12".into()] },
        EditOp::SetParam { card: addr("FREQUENCY"), param: "SORTING".into(), value: Some("NO".into()) },
    ];
    assert!(matches!(err(&root, &ops), EditError::Conflict(_)));
    // Two append_step ops are fine, in order.
    let two = [
        EditOp::AppendStep { text: "*STEP\n*STATIC\n1.,1.\n*END STEP".into() },
        EditOp::AppendStep { text: "*STEP\n*STATIC\n2.,2.\n*END STEP".into() },
    ];
    let (_, t) = run(&root, &two);
    assert!(t.ends_with("*STEP\n*STATIC\n1.,1.\n*END STEP\n*STEP\n*STATIC\n2.,2.\n*END STEP\n"));
}

#[test]
fn card_inside_an_include_writes_a_new_include_and_retargets() {
    let dir = sandbox();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("mesh.msh"), "*NODE\n1,0,0,0\n").unwrap();
    std::fs::write(dir.join("sub/load.inc"), "*CLOAD\n2,2,-1.\n*INCLUDE, INPUT=\"../mesh.msh\"\n").unwrap();
    std::fs::write(dir.join("mid.inc"), "** mid\n*include, input=sub/load.inc\n").unwrap();
    let root = deck_in(&dir, "*INCLUDE, INPUT=mid.inc\n*INCLUDE, INPUT=mesh.msh\n*STEP\n*STATIC\n*END STEP\n");
    let before = snapshot(&dir);

    let ops = [EditOp::ReplaceData { card: addr("CLOAD"), lines: vec!["2,2,-5.".into()] }];
    let (plan, text) = run(&root, &ops);
    assert_eq!(text, "*INCLUDE, INPUT=mid_edit.inc\n*INCLUDE, INPUT=mesh.msh\n*STEP\n*STATIC\n*END STEP\n");
    assert_eq!(std::fs::read_to_string(dir.join("mid_edit.inc")).unwrap(), "** mid\n*include, input=sub/load_edit.inc\n");
    assert_eq!(std::fs::read_to_string(dir.join("sub/load_edit.inc")).unwrap(), "*CLOAD\n2,2,-5.\n*INCLUDE, INPUT=\"../mesh.msh\"\n");
    assert_eq!(plan.files.len(), 3);

    // Only new files appeared; every original is byte-identical.
    let after = snapshot(&dir);
    for (p, bytes) in &before {
        assert_eq!(&after[p], bytes, "{} changed", p.display());
    }
    assert_eq!(after.len(), before.len() + 3);

    // The result re-parses and sees the edit through the include chain.
    let scan = plan.scan_written().unwrap();
    assert!(scan.includes.iter().all(|i| i.exists));
    assert_eq!(read_card_data(scan.find("CLOAD", None)[0], 3).unwrap(), ["2,2,-5."]);
}

#[test]
fn dry_run_scan_matches_the_written_scan_and_writes_nothing() {
    let dir = sandbox();
    std::fs::write(dir.join("load.inc"), "*CLOAD\n2,2,-1.\n").unwrap();
    std::fs::write(dir.join("mesh.msh"), "*NODE\n1,0,0,0\n").unwrap();
    let root = deck_in(&dir, "*INCLUDE, INPUT=mesh.msh\n*STEP\n*STATIC\n*INCLUDE, INPUT=load.inc\n*END STEP\n");
    let before = snapshot(&dir);
    let ops = [
        EditOp::ReplaceData { card: addr("CLOAD"), lines: vec!["2,2,-9.".into()] },
        EditOp::AppendStep { text: "*STEP\n*STATIC\n*END STEP".into() },
    ];
    let plan = plan_edit(&root, &ops, &dir.join("deck_edit.inp")).unwrap();
    let preview = plan.scan_preview().unwrap();
    assert_eq!(snapshot(&dir), before, "dry run must not write");
    assert_eq!(preview.steps().len(), 2);
    assert!(preview.includes.iter().all(|i| i.exists));
    assert_eq!(preview.cards.len(), 10);
    plan.write(false).unwrap();
    let written = plan.scan_written().unwrap();
    assert_eq!(preview.cards.len(), written.cards.len());
    assert_eq!(preview.census(), written.census());
}

#[test]
fn crlf_deck_keeps_crlf_and_mixed_includes_keep_their_own_endings() {
    let dir = sandbox();
    std::fs::write(dir.join("lf.inc"), "*CLOAD\n2,2,-1.\n").unwrap();
    std::fs::write(dir.join("crlf.inc"), "*BOUNDARY\r\n1,1,6\r\n").unwrap();
    let deck = "*INCLUDE, INPUT=lf.inc\r\n*INCLUDE, INPUT=crlf.inc\r\n*STEP,\r\n INC=3\r\n*STATIC\r\n0.1,1.\r\n*END STEP";
    let root = deck_in(&dir, deck); // no final newline either
    let before = snapshot(&dir);
    let ops = [
        EditOp::SetParam { card: addr("STEP"), param: "INC".into(), value: Some("9".into()) },
        EditOp::ReplaceData { card: addr("STATIC"), lines: vec!["0.5, 2.".into()] },
        EditOp::ReplaceData { card: addr("CLOAD"), lines: vec!["3,3,-2.".into(), "4,3,-2.".into()] },
        EditOp::InsertAfter { card: addr("BOUNDARY"), text: "*DLOAD\r\nEALL, GRAV, 9.81, 0., 0., -1.".into() },
        EditOp::AppendStep { text: "*STEP\n*STATIC\n*END STEP\n".into() },
    ];
    let (_, text) = run(&root, &ops);
    assert_eq!(
        text,
        "*INCLUDE, INPUT=lf_edit.inc\r\n*INCLUDE, INPUT=crlf_edit.inc\r\n*STEP,\r\n INC=9\r\n*STATIC\r\n0.5, 2.\r\n*END STEP\r\n*STEP\r\n*STATIC\r\n*END STEP\r\n"
    );
    assert_eq!(std::fs::read_to_string(dir.join("lf_edit.inc")).unwrap(), "*CLOAD\n3,3,-2.\n4,3,-2.\n");
    assert_eq!(
        std::fs::read_to_string(dir.join("crlf_edit.inc")).unwrap(),
        "*BOUNDARY\r\n1,1,6\r\n*DLOAD\r\nEALL, GRAV, 9.81, 0., 0., -1.\r\n"
    );
    for (p, bytes) in &before {
        assert_eq!(&std::fs::read(p).unwrap(), bytes, "{} changed", p.display());
    }
    let scan = scan_cards(&dir.join("out_edit.inp"), &ScanLimits::default()).unwrap();
    assert_eq!(scan.steps().len(), 2);
}

#[test]
fn outputs_never_overwrite_the_deck_tree() {
    let dir = sandbox();
    std::fs::write(dir.join("a.inc"), "*CLOAD\n1,1,1.\n").unwrap();
    let root = deck_in(&dir, "*INCLUDE, INPUT=a.inc\n*STEP\n*STATIC\n*END STEP\n");
    let ops = [EditOp::ReplaceData { card: addr("CLOAD"), lines: vec!["1,1,2.".into()] }];
    // The root itself, or an include of the tree, as output.
    assert!(matches!(plan_edit(&root, &ops, &root).unwrap_err(), EditError::Invalid { .. }));
    assert!(matches!(plan_edit(&root, &ops, &dir.join("a.inc")).unwrap_err(), EditError::Invalid { .. }));
    // Output in another directory breaks the relative includes.
    let other = sandbox();
    assert!(matches!(plan_edit(&root, &ops, &other.join("o.inp")).unwrap_err(), EditError::Invalid { .. }));
    // An existing output is refused unless overwrite is set; nothing is written then.
    let plan = plan_edit(&root, &ops, &dir.join("o.inp")).unwrap();
    std::fs::write(dir.join("a_edit.inc"), "keep").unwrap();
    assert!(matches!(plan.write(false).unwrap_err(), EditError::OutputExists(_)));
    assert!(!dir.join("o.inp").exists());
    plan.write(true).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("a_edit.inc")).unwrap(), "*CLOAD\n1,1,2.\n");
}

#[test]
fn unified_diff_shows_context_and_hunk_headers() {
    let dir = sandbox();
    let root = deck_in(&dir, DECK);
    let (plan, _) = run(&root, &[EditOp::ReplaceData { card: addr("FREQUENCY"), lines: vec!["12".into()] }]);
    let d = plan.files[0].unified_diff("a/deck.inp", "b/deck_edit.inp", 3);
    assert_eq!(
        d,
        "--- a/deck.inp\n+++ b/deck_edit.inp\n@@ -16,7 +16,7 @@\n *END STEP\n *STEP, PERTURBATION\n *FREQUENCY\n-5\n+12\n *BOUNDARY\n 1,1,6\n *END STEP\n"
    );
    // No change, no diff.
    let (plan, _) = run_noop(&dir);
    assert_eq!(plan.files[0].unified_diff("a", "b", 3), "");
}

fn run_noop(dir: &Path) -> (cgx_core::EditPlan, ()) {
    let root = dir.join("deck.inp");
    let out = dir.join("noop.inp");
    let plan = plan_edit(&root, &[EditOp::SetParam { card: at("STEP", 1), param: "INC".into(), value: Some("100".into()) }], &out).unwrap();
    (plan, ())
}
