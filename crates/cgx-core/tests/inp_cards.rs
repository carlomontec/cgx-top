use cgx_core::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Create a unique empty temp directory for one test.
fn tmp_dir(name: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "cgx_inp_cards_{}_{}_{}",
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
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&p, content).unwrap();
    p
}

fn scan(p: &Path) -> InpScan {
    scan_cards(p, &ScanLimits::default()).unwrap()
}

const BASIC: &str = "\
** a comment
*NODE
1, 0., 0., 0.
** inner comment
2, 1., 0., 0.
*ELEMENT, TYPE=C3D8, ELSET=EALL
1, 1, 2, 3, 4, 5, 6, 7, 8
*MATERIAL, NAME=steel
*ELASTIC
210000., 0.3
*STEP, NLGEOM
*STATIC
*BOUNDARY
1, 1, 3
*END STEP
*STEP, PERTURBATION
*FREQUENCY
10
*END STEP
";

#[test]
fn default_limits() {
    let l = ScanLimits::default();
    assert_eq!(l.max_files, 1000);
    assert_eq!(l.max_depth, 16);
    assert_eq!(l.max_total_bytes, 2 * 1024 * 1024 * 1024);
}

#[test]
fn comments_and_data_counts() {
    let d = tmp_dir("basic");
    let p = write(&d, "a.inp", BASIC);
    let s = scan(&p);
    let node = &s.find("NODE", None)[0];
    assert_eq!(node.data_lines, 2);
    assert_eq!(node.line, 2);
    assert_eq!(
        read_card_data(node, 100).unwrap(),
        vec!["1, 0., 0., 0.", "2, 1., 0., 0."]
    );
    assert_eq!(s.total_lines, BASIC.lines().count());
}

#[test]
fn header_continuation() {
    let d = tmp_dir("cont");
    let p = write(
        &d,
        "a.inp",
        "*ELEMENT, TYPE=C3D8,\n** comment inside header\n  ELSET=EALL\n1, 1, 2\n*NODE\n1,0,0,0\n",
    );
    let s = scan(&p);
    assert_eq!(s.cards.len(), 2);
    let e = &s.cards[0];
    assert_eq!(e.keyword, "ELEMENT");
    assert_eq!(e.line, 1);
    assert_eq!(
        e.params,
        vec![
            ("TYPE".to_string(), Some("C3D8".to_string())),
            ("ELSET".to_string(), Some("EALL".to_string()))
        ]
    );
    assert_eq!(e.data_lines, 1);
    assert_eq!(read_card_data(e, 10).unwrap(), vec!["1, 1, 2"]);
    assert_eq!(s.cards[1].line, 5);
}

#[test]
fn parameter_parsing() {
    let d = tmp_dir("params");
    let p = write(
        &d,
        "a.inp",
        "*step , nlgeom , Inc = 100 ,unsymm=yes\n*Cyclic   Symmetry  Model, n=12 , TIE = tie1\n",
    );
    let s = scan(&p);
    assert_eq!(s.cards[0].keyword, "STEP");
    assert_eq!(
        s.cards[0].params,
        vec![
            ("NLGEOM".to_string(), None),
            ("INC".to_string(), Some("100".to_string())),
            ("UNSYMM".to_string(), Some("yes".to_string())),
        ]
    );
    assert_eq!(s.cards[1].keyword, "CYCLIC SYMMETRY MODEL");
    assert_eq!(
        s.cards[1].params,
        vec![
            ("N".to_string(), Some("12".to_string())),
            ("TIE".to_string(), Some("tie1".to_string())),
        ]
    );
}

#[test]
fn crlf_offsets() {
    let d = tmp_dir("crlf");
    let content = BASIC.replace('\n', "\r\n");
    let p = write(&d, "a.inp", &content);
    let s = scan(&p);
    let node = &s.find("NODE", None)[0];
    assert_eq!(node.data_lines, 2);
    assert_eq!(
        read_card_data(node, 100).unwrap(),
        vec!["1, 0., 0., 0.", "2, 1., 0., 0."]
    );
    let el = &s.find("*element", None)[0];
    assert_eq!(read_card_data(el, 100).unwrap(), vec!["1, 1, 2, 3, 4, 5, 6, 7, 8"]);
    // Offset must point exactly at the first data line.
    let bytes = fs::read(&p).unwrap();
    assert!(bytes[node.data_offset as usize..].starts_with(b"1, 0., 0., 0.\r\n"));
    // Mixed endings in one file.
    let p2 = write(&d, "m.inp", "*NODE\r\n1,0,0,0\n2,1,0,0\r\n*ELEMENT\n1,1,2\r\n");
    let s2 = scan(&p2);
    assert_eq!(read_card_data(&s2.cards[0], 10).unwrap(), vec!["1,0,0,0", "2,1,0,0"]);
    assert_eq!(read_card_data(&s2.cards[1], 10).unwrap(), vec!["1,1,2"]);
}

#[test]
fn read_card_data_max_lines() {
    let d = tmp_dir("maxlines");
    let p = write(&d, "a.inp", "*NODE\n1,0,0,0\n** c\n2,0,0,0\n3,0,0,0\n*END STEP\n");
    let s = scan(&p);
    let n = &s.cards[0];
    assert_eq!(n.data_lines, 3);
    assert_eq!(read_card_data(n, 2).unwrap(), vec!["1,0,0,0", "2,0,0,0"]);
    assert_eq!(read_card_data(n, 0).unwrap().len(), 0);
    assert_eq!(read_card_data(n, 99).unwrap().len(), 3);
    // A card without data returns nothing.
    assert!(read_card_data(&s.cards[1], 5).unwrap().is_empty());
}

#[test]
fn nested_includes_three_levels() {
    let d = tmp_dir("nested");
    write(&d, "sub/l3.inp", "*NSET, NSET=N3\n1,2\n");
    write(&d, "sub/l2.inp", "*INCLUDE, INPUT=l3.inp\n*NSET, NSET=N2\n3\n");
    write(&d, "l1.inp", "*INCLUDE, INPUT=sub/l2.inp\n*NSET, NSET=N1\n4\n");
    let p = write(&d, "root.inp", "*NODE\n1,0,0,0\n*INCLUDE, input=\"l1.inp\"\n*ELEMENT\n1,1\n");
    let s = scan(&p);
    assert_eq!(s.includes.len(), 3);
    assert!(s.includes.iter().all(|i| i.exists));
    assert_eq!(s.includes[0].raw_target, "l1.inp");
    assert_eq!(s.includes[0].line_number, 3);
    assert_eq!(s.includes[0].included_from, p);
    assert_eq!(s.includes[2].resolved_path, d.join("sub").join("l3.inp"));
    let order: Vec<&str> = s.cards.iter().map(|c| c.keyword.as_str()).collect();
    assert_eq!(
        order,
        vec!["NODE", "INCLUDE", "INCLUDE", "INCLUDE", "NSET", "NSET", "NSET", "ELEMENT"]
    );
    let n3 = s.cards.iter().find(|c| c.params.first().and_then(|p| p.1.as_deref()) == Some("N3")).unwrap();
    assert_eq!(n3.file, d.join("sub").join("l3.inp"));
    assert_eq!(read_card_data(n3, 5).unwrap(), vec!["1,2"]);
    assert!(!s.flags().contains(&"MISSING_INCLUDES"));
}

#[test]
fn include_cycle_is_error() {
    let d = tmp_dir("cycle");
    write(&d, "b.inp", "*INCLUDE, INPUT=a.inp\n");
    let p = write(&d, "a.inp", "*INCLUDE, INPUT=b.inp\n");
    match scan_cards(&p, &ScanLimits::default()) {
        Err(FrdError::InvalidFormat(m)) => assert!(m.contains("Circular"), "{m}"),
        other => panic!("expected cycle error, got {other:?}"),
    }
    // Self include.
    let p = write(&d, "self.inp", "*INCLUDE, INPUT=self.inp\n");
    assert!(matches!(
        scan_cards(&p, &ScanLimits::default()),
        Err(FrdError::InvalidFormat(_))
    ));
}

#[test]
fn diamond_include_is_not_a_cycle() {
    let d = tmp_dir("diamond");
    write(&d, "c.inp", "*NSET, NSET=C\n1\n");
    write(&d, "a.inp", "*INCLUDE, INPUT=c.inp\n");
    write(&d, "b.inp", "*INCLUDE, INPUT=c.inp\n");
    let p = write(&d, "root.inp", "*INCLUDE, INPUT=a.inp\n*INCLUDE, INPUT=b.inp\n");
    let s = scan(&p);
    assert_eq!(s.find("NSET", None).len(), 2);
}

#[test]
fn missing_include_is_recorded() {
    let d = tmp_dir("missing");
    let p = write(&d, "root.inp", "*INCLUDE, INPUT=nope.inp\n*NODE\n1,0,0,0\n");
    let s = scan(&p);
    assert_eq!(s.includes.len(), 1);
    assert!(!s.includes[0].exists);
    assert_eq!(s.includes[0].raw_target, "nope.inp");
    assert_eq!(s.find("NODE", None).len(), 1);
    assert!(s.flags().contains(&"MISSING_INCLUDES"));
}

#[test]
fn missing_root_is_error() {
    let d = tmp_dir("noroot");
    assert!(scan_cards(&d.join("none.inp"), &ScanLimits::default()).is_err());
}

#[test]
fn step_numbering() {
    let d = tmp_dir("steps");
    let p = write(&d, "a.inp", BASIC);
    let s = scan(&p);
    let step_of = |kw: &str| s.find(kw, None).iter().map(|c| c.step).collect::<Vec<_>>();
    assert_eq!(step_of("NODE"), vec![0]);
    assert_eq!(step_of("STEP"), vec![1, 2]);
    assert_eq!(step_of("STATIC"), vec![1]);
    assert_eq!(step_of("FREQUENCY"), vec![2]);
    assert_eq!(step_of("END STEP"), vec![1, 2]);
    // Model-level card after the steps returns to step 0.
    let p = write(&d, "b.inp", "*STEP\n*STATIC\n*END STEP\n*NSET, NSET=A\n1\n*STEP\n*END STEP\n");
    let s = scan(&p);
    assert_eq!(s.find("NSET", None)[0].step, 0);
    assert_eq!(s.find("STEP", None)[1].step, 2);
}

#[test]
fn step_numbering_across_includes() {
    let d = tmp_dir("stepinc");
    write(&d, "s.inp", "*STEP\n*STATIC\n*END STEP\n");
    let p = write(&d, "root.inp", "*INCLUDE, INPUT=s.inp\n*STEP\n*FREQUENCY\n*END STEP\n");
    let s = scan(&p);
    assert_eq!(s.steps().len(), 2);
    assert_eq!(s.steps()[1].number, 2);
    assert_eq!(s.steps()[1].procedure.as_deref(), Some("FREQUENCY"));
}

#[test]
fn census_and_steps() {
    let d = tmp_dir("census");
    let p = write(&d, "a.inp", BASIC);
    let s = scan(&p);
    let c = s.census();
    assert_eq!(c.get("NODE"), Some(&1));
    assert_eq!(c.get("ELEMENT"), Some(&1));
    assert_eq!(c.get("MATERIAL"), Some(&1));
    assert_eq!(c.get("ELASTIC"), Some(&1));
    assert_eq!(c.get("STEP"), None);
    assert_eq!(c.get("STATIC"), None);

    let steps = s.steps();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].number, 1);
    assert_eq!(steps[0].params, "NLGEOM");
    assert_eq!(steps[0].procedure.as_deref(), Some("STATIC"));
    assert_eq!(steps[0].cards.get("BOUNDARY"), Some(&1));
    assert_eq!(steps[0].line, 11);
    assert_eq!(steps[0].file, p);
    assert_eq!(steps[1].procedure.as_deref(), Some("FREQUENCY"));
    assert_eq!(steps[1].params, "PERTURBATION");
}

#[test]
fn step_without_procedure() {
    let d = tmp_dir("noproc");
    let p = write(&d, "a.inp", "*STEP, INC=5\n*BOUNDARY\n1,1\n*END STEP\n");
    let s = scan(&p);
    let st = s.steps();
    assert_eq!(st[0].procedure, None);
    assert_eq!(st[0].params, "INC=5");
}

#[test]
fn flags() {
    let d = tmp_dir("flags");
    let p = write(&d, "a.inp", BASIC);
    let s = scan(&p);
    assert_eq!(s.flags(), vec!["NLGEOM", "PERTURBATION"]);

    let p = write(
        &d,
        "b.inp",
        "*CYCLIC SYMMETRY MODEL, N=4\n1,2\n*CONTACT PAIR, INTERACTION=I\na,b\n*INCLUDE, INPUT=gone.inp\n",
    );
    let s = scan(&p);
    assert_eq!(
        s.flags(),
        vec!["CYCLIC_SYMMETRY", "CONTACT", "MISSING_INCLUDES", "NO_STEPS", "NO_MESH"]
    );

    // Only nodes: still NO_MESH.
    let p = write(&d, "c.inp", "*NODE\n1,0,0,0\n*STEP\n*END STEP\n");
    assert_eq!(scan(&p).flags(), vec!["NO_MESH"]);
}

#[test]
fn external_refs() {
    let d = tmp_dir("extrefs");
    write(&d, "table.dat", "1,2\n");
    let p = write(
        &d,
        "a.inp",
        "*INCLUDE, INPUT=other.inp\n\
         *AMPLITUDE, NAME=A1, INPUT=table.dat\n\
         *NODE, INPUT=missing.nod\n\
         *RESTART, READ, FILE=\"restart.rin\"\n\
         *NODE FILE\nU\n",
    );
    let s = scan(&p);
    let refs = s.external_refs();
    assert_eq!(refs.len(), 3);
    assert_eq!(refs[0].card, "AMPLITUDE");
    assert_eq!(refs[0].param, "INPUT");
    assert_eq!(refs[0].target, "table.dat");
    assert_eq!(refs[0].resolved_path, d.join("table.dat"));
    assert_eq!(refs[0].line, 2);
    assert_eq!(refs[0].file, p);
    assert!(refs[0].exists);
    assert_eq!(refs[1].card, "NODE");
    assert!(!refs[1].exists);
    assert_eq!(refs[2].param, "FILE");
    assert_eq!(refs[2].target, "restart.rin");
    assert!(!refs[2].exists);
}

#[test]
fn find_variants() {
    let d = tmp_dir("find");
    let p = write(&d, "a.inp", BASIC);
    let s = scan(&p);
    assert_eq!(s.find("*Boundary", None).len(), 1);
    assert_eq!(s.find("  end   step ", None).len(), 2);
    assert_eq!(s.find("STEP", Some(0)).len(), 0);
    assert_eq!(s.find("STEP", Some(1)).len(), 1);
    assert_eq!(s.find("BOUNDARY", Some(0)).len(), 0);
    assert_eq!(s.find("BOUNDARY", Some(1)).len(), 1);
    assert_eq!(s.find("BOUNDARY", Some(2)).len(), 0);
    assert_eq!(s.find("NODE", Some(0)).len(), 1);
    assert_eq!(s.find("NOPE", None).len(), 0);
}

#[test]
fn limit_max_files() {
    let d = tmp_dir("lim_files");
    write(&d, "x.inp", "*NODE\n1,0,0,0\n");
    write(&d, "y.inp", "*NODE\n1,0,0,0\n");
    let p = write(&d, "root.inp", "*INCLUDE, INPUT=x.inp\n*INCLUDE, INPUT=y.inp\n");
    let ok = ScanLimits { max_files: 3, ..Default::default() };
    assert!(scan_cards(&p, &ok).is_ok());
    let bad = ScanLimits { max_files: 2, ..Default::default() };
    match scan_cards(&p, &bad) {
        Err(FrdError::InvalidFormat(m)) => assert!(m.contains("number of files"), "{m}"),
        other => panic!("expected limit error, got {other:?}"),
    }
}

#[test]
fn limit_max_depth() {
    let d = tmp_dir("lim_depth");
    write(&d, "l2.inp", "*NODE\n1,0,0,0\n");
    write(&d, "l1.inp", "*INCLUDE, INPUT=l2.inp\n");
    let p = write(&d, "root.inp", "*INCLUDE, INPUT=l1.inp\n");
    let ok = ScanLimits { max_depth: 2, ..Default::default() };
    assert!(scan_cards(&p, &ok).is_ok());
    let bad = ScanLimits { max_depth: 1, ..Default::default() };
    match scan_cards(&p, &bad) {
        Err(FrdError::InvalidFormat(m)) => assert!(m.contains("depth"), "{m}"),
        other => panic!("expected limit error, got {other:?}"),
    }
    let zero = ScanLimits { max_depth: 0, ..Default::default() };
    assert!(scan_cards(&p, &zero).is_err());
}

#[test]
fn limit_max_total_bytes() {
    let d = tmp_dir("lim_bytes");
    write(&d, "x.inp", "*NODE\n1,0,0,0\n");
    let p = write(&d, "root.inp", "*INCLUDE, INPUT=x.inp\n");
    let total = fs::metadata(&p).unwrap().len() + fs::metadata(d.join("x.inp")).unwrap().len();
    let ok = ScanLimits { max_total_bytes: total, ..Default::default() };
    assert!(scan_cards(&p, &ok).is_ok());
    let bad = ScanLimits { max_total_bytes: total - 1, ..Default::default() };
    match scan_cards(&p, &bad) {
        Err(FrdError::InvalidFormat(m)) => assert!(m.contains("total bytes"), "{m}"),
        other => panic!("expected limit error, got {other:?}"),
    }
}

#[test]
fn real_deck_lavffcyc() {
    let p = Path::new(
        "/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor/lavffcyc.inp",
    );
    if !p.exists() {
        eprintln!("skipping: {} not found", p.display());
        return;
    }
    let s = scan_cards(p, &ScanLimits::default()).unwrap();
    println!("total_lines: {}  cards: {}  includes: {}", s.total_lines, s.cards.len(), s.includes.len());
    println!("census: {:#?}", s.census());
    println!("steps: {:#?}", s.steps());
    println!("external_refs: {:#?}", s.external_refs());
    println!("flags: {:?}", s.flags());
    assert!(s.flags().contains(&"CYCLIC_SYMMETRY"));
    assert!(s.steps().len() >= 2);
}
