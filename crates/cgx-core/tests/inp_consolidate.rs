use cgx_core::*;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn tmp_dir(name: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "cgx_inp_consolidate_{}_{}_{}",
        std::process::id(),
        n,
        name
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, content).unwrap();
    p
}

fn count_lines(text: &str, needle: &str) -> usize {
    text.lines().filter(|l| l.trim() == needle).count()
}

#[test]
fn diamond_include_consolidates_twice() {
    let dir = tmp_dir("diamond");
    write(&dir, "common.inc", "*NSET, NSET=COMMON\n1, 2\n");
    write(&dir, "a.inc", "*INCLUDE, INPUT=common.inc\n");
    write(&dir, "b.inc", "*INCLUDE, INPUT=common.inc\n");
    let root = write(
        &dir,
        "root.inp",
        "*INCLUDE, INPUT=a.inc\n*INCLUDE, INPUT=b.inc\n*STEP\n*STATIC\n*END STEP\n",
    );

    let (text, stats) = consolidate_inp(&root).unwrap();
    assert_eq!(count_lines(&text, "*NSET, NSET=COMMON"), 2);
    assert_eq!(stats.total_files_inlined, 4);
    assert!(!text.lines().any(|l| l.trim().to_uppercase().starts_with("*INCLUDE")));

    // inspect also accepts the diamond and counts the file once.
    let summary = inspect_inp(&root).unwrap();
    assert_eq!(summary.total_files_count, 4);
}

#[test]
fn real_cycle_is_an_error() {
    let dir = tmp_dir("cycle");
    write(&dir, "a.inc", "*INCLUDE, INPUT=b.inc\n");
    write(&dir, "b.inc", "*INCLUDE, INPUT=a.inc\n");
    let root = write(&dir, "root.inp", "*INCLUDE, INPUT=a.inc\n");

    let err = consolidate_inp(&root).unwrap_err();
    assert!(err.to_string().contains("Circular"), "{err}");
    let err = inspect_inp(&root).unwrap_err();
    assert!(err.to_string().contains("Circular"), "{err}");
}

#[test]
fn self_include_is_a_cycle() {
    let dir = tmp_dir("selfinc");
    let root = write(&dir, "root.inp", "*INCLUDE, INPUT=root.inp\n");
    assert!(consolidate_inp(&root).is_err());
    assert!(inspect_inp(&root).is_err());
}

#[test]
fn depth_twelve_works() {
    let dir = tmp_dir("deep");
    for i in 1..=12 {
        let body = if i == 12 {
            "*NSET, NSET=DEEP\n7\n".to_string()
        } else {
            format!("*INCLUDE, INPUT=l{}.inc\n", i + 1)
        };
        write(&dir, &format!("l{i}.inc"), &body);
    }
    let root = write(&dir, "root.inp", "*INCLUDE, INPUT=l1.inc\n");

    let (text, stats) = consolidate_inp(&root).unwrap();
    assert_eq!(stats.total_files_inlined, 12);
    assert!(text.contains("NSET=DEEP"));
    let summary = inspect_inp(&root).unwrap();
    assert_eq!(summary.total_files_count, 13);
    assert_eq!(summary.node_sets.get("DEEP"), Some(&1));
}

#[test]
fn too_deep_is_an_error() {
    let dir = tmp_dir("toodeep");
    let n = ScanLimits::default().max_depth + 2;
    for i in 1..=n {
        let body = if i == n {
            "*NODE\n1, 0, 0, 0\n".to_string()
        } else {
            format!("*INCLUDE, INPUT=l{}.inc\n", i + 1)
        };
        write(&dir, &format!("l{i}.inc"), &body);
    }
    let root = write(&dir, "root.inp", "*INCLUDE, INPUT=l1.inc\n");
    assert!(consolidate_inp(&root).is_err());
    assert!(inspect_inp(&root).is_err());
}

#[test]
fn input_param_on_node_card_is_inlined() {
    let dir = tmp_dir("inputparam");
    write(&dir, "nodes.inc", "1, 0., 0., 0.\n2, 1., 0., 0.\n");
    write(&dir, "init.inc", "1, 20.\n");
    let root = write(
        &dir,
        "root.inp",
        "*NODE, INPUT=nodes.inc\n*INITIAL CONDITIONS,\n  TYPE=TEMPERATURE, input=\"init.inc\"\n*STEP\n*STATIC\n*END STEP\n",
    );

    let (text, stats) = consolidate_inp(&root).unwrap();
    assert_eq!(stats.total_files_inlined, 2);
    let lines: Vec<&str> = text.lines().collect();
    let node_idx = lines.iter().position(|l| *l == "*NODE").expect("bare *NODE header");
    assert_eq!(lines[node_idx + 1], "1, 0., 0., 0.");
    assert_eq!(lines[node_idx + 2], "2, 1., 0., 0.");
    let ic_idx = lines
        .iter()
        .position(|l| *l == "*INITIAL CONDITIONS, TYPE=TEMPERATURE")
        .expect("header without INPUT");
    assert_eq!(lines[ic_idx + 1], "1, 20.");
    assert!(!text.to_uppercase().contains("INPUT="));

    // The consolidated deck has the node data as ordinary data lines.
    let out = write(&dir, "out.inp", &text);
    let scan = scan_cards(&out, &ScanLimits::default()).unwrap();
    let node = scan.cards.iter().find(|c| c.keyword == "NODE").unwrap();
    assert_eq!(node.data_lines, 2);
}

#[test]
fn missing_input_file_is_an_error() {
    let dir = tmp_dir("missinginput");
    let root = write(&dir, "root.inp", "*NODE, INPUT=nope.inc\n");
    assert!(consolidate_inp(&root).is_err());
}

fn census(scan: &InpScan) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for c in &scan.cards {
        *m.entry(c.keyword.clone()).or_insert(0) += 1;
    }
    m
}

#[test]
fn lavffcyc_consolidation_matches_card_census() {
    let root = Path::new(
        "/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor/lavffcyc.inp",
    );
    if !root.exists() {
        eprintln!("skipping: {} not found", root.display());
        return;
    }
    let (text, _) = consolidate_inp(root).unwrap();
    assert_eq!(
        text.lines()
            .filter(|l| l.trim().to_uppercase().starts_with("*INCLUDE"))
            .count(),
        0
    );

    let original = scan_cards(root, &ScanLimits::default()).unwrap();
    let dir = tmp_dir("lavffcyc");
    let out = write(&dir, "consolidated.inp", &text);
    let consolidated = scan_cards(&out, &ScanLimits::default()).unwrap();

    let mut expected = census(&original);
    expected.remove("INCLUDE");
    assert_eq!(census(&consolidated), expected);
}
