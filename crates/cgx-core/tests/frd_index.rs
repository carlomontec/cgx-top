//! `FrdIndex` (lazy) must agree with `FrdParserEngine::parse_all` (full materialisation).

use cgx_core::{FrdIndex, FrdModel, FrdParserEngine, StressTensor};
use std::collections::HashSet;
use std::path::Path;

// ---------- synthetic FRD builders ----------

fn header(node_count: usize, step: u32, value: f32, format: u8) -> String {
    format!(
        "{:>5}C{:<6}{:>12.5E}{:>12}{:<20}{:>2}{:>5}{:<10}{:>2}\n",
        100, "L", value, node_count, "", 0, step, "", format
    )
}

fn meta(name: &str, comps: &[(&str, i32)]) -> String {
    let mut s = format!(" -4  {:<8}{:>5}{:>5}\n", name, comps.len(), 1);
    for (k, (comp, iexist)) in comps.iter().enumerate() {
        s += &format!(" -5  {:<8}{:>5}{:>5}{:>5}{:>5}{:>5}\n", comp, 1, 2, k + 1, 0, iexist);
    }
    s
}

/// Parameter records ccx writes before a `100C` block (layout of `frdheader.c`): `1PSTEP` always,
/// the others for frequency analyses. `nd = -1` means no nodal diameter.
fn params(counter: u32, inc: u32, step: u32, modal: Option<(u32, i32)>) -> Vec<u8> {
    let mut s = format!("    1PSTEP{:>14}{:>12}{:>12}{:>12}          \n", "", counter, inc, step);
    if let Some((mode, nd)) = modal {
        s += &format!("    1PGM{:>16}{:>12.6E}{:34}\n", "", 1.0f32, "");
        s += &format!("    1PGK{:>16}{:>12.6E}{:34}\n", "", 2.7e9f32, "");
        s += &format!("    1PHID{:>15}{:>12}{:34}\n", "", nd, "");
        if nd >= 0 {
            s += "    1PAX                -0.00000E+00-0.00000E+00-0.00000E+00-1.00000E+00-0.00000E+00-0.00000E+00\n";
        }
        s += &format!("    1PSUBC{:>14}{:>12}{:34}\n", "", 0, "");
        s += &format!("    1PMODE{:>14}{:>12}{:34}\n", "", mode, "");
    }
    s.into_bytes()
}

fn with_params(mut p: Vec<u8>, block: Vec<u8>) -> Vec<u8> {
    p.extend(block);
    p
}

const DISP: [(&str, i32); 4] = [("D1", 0), ("D2", 0), ("D3", 0), ("ALL", 1)];
const STRESS: [(&str, i32); 6] =
    [("SXX", 0), ("SYY", 0), ("SZZ", 0), ("SXY", 0), ("SYZ", 0), ("SZX", 0)];
const PDISP: [(&str, i32); 6] =
    [("MAG1", 0), ("MAG2", 0), ("MAG3", 0), ("PHA1", 0), ("PHA2", 0), ("PHA3", 0)];

/// Deterministic pseudo-data: value of component `c` at node `nid` in step `step`.
fn sample(nid: u32, c: usize, step: u32) -> f32 {
    let x = (nid as f32 * 0.37 + c as f32 * 1.3 + step as f32 * 0.11).sin();
    x * 100.0 * (1.0 + c as f32)
}

fn ascii_block(name: &str, comps: &[(&str, i32)], nodes: &[u32], step: u32, long: bool) -> Vec<u8> {
    let nvals = comps.iter().filter(|c| c.1 != 1).count();
    let mut s = header(nodes.len(), step, step as f32 * 0.5, long as u8);
    s += &meta(name, comps);
    for &nid in nodes {
        s += &if long { format!(" -1{:>10}", nid) } else { format!(" -1{:>5}", nid) };
        for c in 0..nvals {
            s += &format!("{:>12.5E}", sample(nid, c, step));
        }
        s += "\n";
    }
    s += " -3\n";
    s.into_bytes()
}

fn binary_block(name: &str, comps: &[(&str, i32)], nodes: &[u32], step: u32, double: bool) -> Vec<u8> {
    let nvals = comps.iter().filter(|c| c.1 != 1).count();
    let mut out = header(nodes.len(), step, step as f32 * 0.5, if double { 3 } else { 2 }).into_bytes();
    out.extend(meta(name, comps).into_bytes());
    for &nid in nodes {
        out.extend(nid.to_le_bytes());
        for c in 0..nvals {
            let v = sample(nid, c, step);
            if double {
                out.extend((v as f64).to_le_bytes());
            } else {
                out.extend(v.to_le_bytes());
            }
        }
    }
    out.extend(b" -3\n");
    out
}

fn file(blocks: Vec<Vec<u8>>) -> Vec<u8> {
    let mut out = b"    1Ctest\n".to_vec();
    for b in blocks {
        out.extend(b);
    }
    out.extend(b" 9999\n");
    out
}

// ---------- equivalence check ----------

fn same(a: f32, b: f32) -> bool {
    a == b || (a.is_nan() && b.is_nan())
}

/// Compare every header, every nodal value and both histories against the full parser.
fn assert_equivalent(index: &FrdIndex, model: &FrdModel, filter: Option<&HashSet<u32>>) {
    assert_eq!(index.mesh().title, model.title);
    assert_eq!(index.mesh().nodes, model.nodes);
    assert_eq!(index.mesh().elements, model.elements);
    assert_eq!(index.steps().len(), model.steps.len(), "step count");

    for (sref, step) in index.steps().iter().zip(&model.steps) {
        assert_eq!(sref.step_number, step.step_number);
        assert!(same(sref.value, step.value));
        assert_eq!(sref.entries.len(), step.datasets.len(), "datasets in step {}", step.step_number);

        for (&idx, ds) in sref.entries.iter().zip(&step.datasets) {
            let entry = &index.entries()[idx];
            assert_eq!(entry.name, ds.name);
            assert_eq!(entry.field_type, ds.field_type);
            assert_eq!(entry.components.len(), ds.components.len());

            let dense = index.load(idx).expect("load");
            let unique: HashSet<u32> = dense.node_ids.iter().copied().collect();
            for (c, comp) in ds.components.iter().enumerate() {
                assert_eq!(dense.names[c], comp.name);
                let col = &dense.columns[c];
                if col.is_empty() {
                    assert!(comp.values.is_empty(), "{}:{} should be empty", ds.name, comp.name);
                    continue;
                }
                assert_eq!(col.len(), dense.node_ids.len());
                assert_eq!(comp.values.len(), unique.len(), "{}:{} node count", ds.name, comp.name);
                for (i, nid) in dense.node_ids.iter().enumerate() {
                    let expected = comp.values[nid];
                    assert!(same(col[i], expected), "{}:{} node {}: {} vs {}", ds.name, comp.name, nid, col[i], expected);
                }
            }
        }
    }

    // `compute_stress_history` matches "STRESS" by substring and so also counts the imaginary
    // part `STRESSI` of complex results; `FrdIndex::stress_history` deliberately does not.
    // Compare against the old function on a model without those datasets.
    let mut real_only = model.clone();
    for step in &mut real_only.steps {
        step.datasets.retain(|d| d.name.eq_ignore_ascii_case("STRESS") || !d.name.to_uppercase().contains("STRESS"));
    }
    let (new_s, old_s) = (index.stress_history(filter), real_only.compute_stress_history(filter));
    assert_eq!(new_s.len(), old_s.len(), "stress history length");
    for (a, b) in new_s.iter().zip(&old_s) {
        assert_eq!((a.step_number, a.increment_number, a.node_count), (b.step_number, b.increment_number, b.node_count));
        assert!(same(a.value, b.value));
        // Values must match exactly. Node ids may differ only on exact ties.
        assert!(same(a.max_von_mises.0, b.max_von_mises.0));
        assert!(same(a.min_von_mises.0, b.min_von_mises.0));
        assert!(same(a.max_principal_s1.0, b.max_principal_s1.0));
        assert!(same(a.min_principal_s3.0, b.min_principal_s3.0));
        assert!(same(a.max_tresca.0, b.max_tresca.0));
        assert!(same(a.max_hydrostatic.0, b.max_hydrostatic.0));
    }
    // The reported node really carries the reported von Mises value.
    for (a, step) in new_s.iter().zip(model.steps.iter().filter(|s| s.datasets.iter().any(|d| d.name == "STRESS"))) {
        let ds = step.datasets.iter().find(|d| d.name == "STRESS").unwrap();
        let v = |c: usize| ds.components[c].values[&a.max_von_mises.1];
        let vm = StressTensor::new(v(0), v(1), v(2), v(3), v(4), v(5)).von_mises();
        assert!(same(vm, a.max_von_mises.0), "node {} vm {} vs {}", a.max_von_mises.1, vm, a.max_von_mises.0);
    }

    let (new_d, old_d) = (index.disp_history(filter), model.compute_disp_history(filter));
    assert_eq!(new_d.len(), old_d.len(), "disp history length");
    for (a, b) in new_d.iter().zip(&old_d) {
        assert_eq!((a.step_number, a.node_count), (b.step_number, b.node_count));
        assert!(same(a.max_disp.0, b.max_disp.0));
        assert!(same(a.min_disp.0, b.min_disp.0));
    }
}

fn check_bytes(bytes: Vec<u8>, filter: Option<&HashSet<u32>>) -> FrdIndex {
    let model = FrdParserEngine::from_bytes(&bytes).parse_all().expect("full parse");
    let index = FrdIndex::from_bytes(bytes).expect("index");
    assert_equivalent(&index, &model, filter);
    index
}

fn nodes() -> Vec<u32> {
    (1..=40).map(|i| i * 3).collect()
}

// ---------- tests ----------

#[test]
fn ascii_short_and_long_match_full_parser() {
    for long in [false, true] {
        let n = nodes();
        let mut blocks = Vec::new();
        for step in 1..=3 {
            blocks.push(ascii_block("DISP", &DISP, &n, step, long));
            blocks.push(ascii_block("STRESS", &STRESS, &n, step, long));
        }
        let index = check_bytes(file(blocks), None);
        assert_eq!(index.entries().len(), 6);
        assert_eq!(index.steps().len(), 3);
        assert_eq!(index.stress_history(None).len(), 3);
        assert_eq!(index.disp_history(None).len(), 3);
    }
}

#[test]
fn binary_f32_and_f64_match_full_parser() {
    for double in [false, true] {
        let n = nodes();
        let mut blocks = Vec::new();
        for step in 1..=3 {
            blocks.push(binary_block("DISP", &DISP, &n, step, double));
            blocks.push(binary_block("STRESS", &STRESS, &n, step, double));
        }
        let index = check_bytes(file(blocks), None);
        assert_eq!(index.entries().len(), 6);
        assert_eq!(index.entries()[1].format, if double { 3 } else { 2 });
        assert_eq!(index.stress_history(None).len(), 3);
    }
}

#[test]
fn binary_payload_containing_newlines_and_markers_is_skipped_by_length() {
    // Node ids whose little-endian bytes contain '\n' (10) and spell " -3 " / " -4 ".
    let tricky = [10u32, 0x0A0A_0A0A, u32::from_le_bytes(*b" -3 "), u32::from_le_bytes(*b" -4 "), 7];
    let blocks = vec![
        binary_block("STRESS", &STRESS, &tricky, 1, false),
        binary_block("DISP", &DISP, &tricky, 1, false),
    ];
    let index = FrdIndex::from_bytes(file(blocks)).expect("index");
    assert_eq!(index.entries().len(), 2);
    assert_eq!(index.entries()[1].name, "DISP");
    let ds = index.load(0).unwrap();
    assert_eq!(ds.node_ids, tricky);
    assert_eq!(ds.columns[5][4], sample(7, 5, 1));
}

#[test]
fn set_filter_matches_full_parser() {
    let n = nodes();
    let blocks = vec![
        ascii_block("DISP", &DISP, &n, 1, false),
        binary_block("STRESS", &STRESS, &n, 1, false),
        ascii_block("STRESS", &STRESS, &n, 2, true),
    ];
    let filter: HashSet<u32> = [3, 9, 30, 60, 99999].into_iter().collect();
    let index = check_bytes(file(blocks), Some(&filter));
    assert_eq!(index.stress_history(Some(&filter))[0].node_count, 4);

    let nothing: HashSet<u32> = [1, 2].into_iter().collect();
    assert!(index.stress_history(Some(&nothing)).is_empty());
    assert!(index.disp_history(Some(&nothing)).is_empty());
}

#[test]
fn derived_all_component_is_vector_magnitude() {
    let index = FrdIndex::from_bytes(file(vec![ascii_block("DISP", &DISP, &[5, 6], 1, false)])).unwrap();
    let ds = index.load(0).unwrap();
    let all = ds.column(&["ALL"]).unwrap();
    for i in 0..2 {
        let (x, y, z) = (ds.columns[0][i], ds.columns[1][i], ds.columns[2][i]);
        assert_eq!(all[i], (x * x + y * y + z * z).sqrt());
    }
}

#[test]
fn truncated_files_do_not_panic() {
    let n = nodes();
    let good = file(vec![
        ascii_block("DISP", &DISP, &n, 1, false),
        binary_block("STRESS", &STRESS, &n, 1, false),
    ]);
    // Cut inside the binary payload: a clean error.
    let cut = good.len() - 200;
    assert!(FrdIndex::from_bytes(good[..cut].to_vec()).is_err());

    // Every prefix of an ASCII-only file indexes and loads without panicking.
    let ascii = file(vec![ascii_block("DISP", &DISP, &n, 1, false), ascii_block("STRESS", &STRESS, &n, 1, true)]);
    for len in (0..ascii.len()).step_by(37) {
        if let Ok(index) = FrdIndex::from_bytes(ascii[..len].to_vec()) {
            for i in 0..index.entries().len() {
                let _ = index.load(i);
            }
            let _ = index.stress_history(None);
            let _ = index.disp_history(None);
        }
    }
    assert!(FrdIndex::from_bytes(Vec::new()).unwrap().entries().is_empty());
    assert!(FrdIndex::from_bytes(good).unwrap().load(99).is_err());
}

#[test]
fn step_parameters_and_complex_results() {
    let n = nodes();
    let modal = Some((3, 5));
    let blocks = vec![
        // Static step: 1PSTEP only.
        with_params(params(1, 1, 1, None), ascii_block("DISP", &DISP, &n, 1, false)),
        with_params(params(2, 1, 1, None), ascii_block("STRESS", &STRESS, &n, 1, false)),
        // Cyclic modal step: every block has its own parameter records.
        with_params(params(3, 1, 2, modal), ascii_block("STRESS", &STRESS, &n, 2, false)),
        with_params(params(4, 1, 2, modal), ascii_block("STRESSI", &STRESS, &n, 2, false)),
        with_params(params(5, 1, 2, modal), ascii_block("PDISP", &PDISP, &n, 2, false)),
        // No parameter records: nothing may leak from the previous block.
        ascii_block("STRESS", &STRESS, &n, 3, true),
        // Plain frequency analysis: ccx writes nodal diameter -1.
        with_params(params(7, 1, 3, Some((1, -1))), binary_block("STRESS", &STRESS, &n, 4, false)),
    ];
    let index = check_bytes(file(blocks), None);

    let meta: Vec<_> = index.entries().iter().map(|e| (e.pstep, e.mode, e.nodal_diameter)).collect();
    assert_eq!(
        meta,
        [
            (Some((1, 1, 1)), None, None),
            (Some((2, 1, 1)), None, None),
            (Some((3, 1, 2)), Some(3), Some(5)),
            (Some((4, 1, 2)), Some(3), Some(5)),
            (Some((5, 1, 2)), Some(3), Some(5)),
            (None, None, None),
            (Some((7, 1, 3)), Some(1), None),
        ]
    );
    let steps: Vec<_> = index.steps().iter().map(|s| (s.step_number, s.pstep, s.mode, s.nodal_diameter)).collect();
    assert_eq!(
        steps,
        [
            (1, Some((1, 1, 1)), None, None),
            (2, Some((3, 1, 2)), Some(3), Some(5)),
            (3, None, None, None),
            (4, Some((7, 1, 3)), Some(1), None),
        ]
    );

    // STRESSI is not real stress, PDISP is not DISP.
    let stress = index.stress_history(None);
    assert_eq!(stress.iter().map(|s| s.step_number).collect::<Vec<_>>(), [1, 2, 3, 4]);
    assert_eq!(index.disp_history(None).len(), 1);

    // PDISP amplitude bound = peak of sqrt(MAG1^2 + MAG2^2 + MAG3^2).
    let pdisp = index.load(4).unwrap();
    let summ = pdisp.pdisp_extrema(None).unwrap();
    let amp = |nid: u32| (0..3).map(|c| sample(nid, c, 2).powi(2)).sum::<f32>().sqrt();
    let (best, node) = n.iter().map(|&nid| (amp(nid), nid)).fold((f32::MIN, 0), |a, b| if b.0 > a.0 { b } else { a });
    assert_eq!(summ.max_disp, (best, node));
    assert_eq!(summ.node_count, n.len());
    let some: HashSet<u32> = [3, 6].into_iter().collect();
    assert_eq!(pdisp.pdisp_extrema(Some(&some)).unwrap().node_count, 2);
    // Other datasets lack MAG1..3.
    assert!(index.load(0).unwrap().pdisp_extrema(None).is_none());
}

// ---------- real files (skipped when absent) ----------

const REAL_FILES: [&str; 4] = [
    "/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/test_NewLib/beam_benchmark.frd",
    "/Users/carlo/code/CalculiX/cgx/CalculiX-CGX-New3D/test/beam_modal.frd",
    "/Users/carlo/code/CalculiX/cgx/CalculiX-CGX-New3D/cgx_2.23/examples/result.frd",
    "/Users/carlo/code/CalculiX/cgx/CalculiX-CGX-New3D/test/beam20p.frd",
];

#[test]
fn real_files_match_full_parser() {
    for path in REAL_FILES {
        if !Path::new(path).exists() {
            eprintln!("Skipping: {} not found", path);
            continue;
        }
        let model = cgx_core::parse_frd_file(path).expect("full parse");
        let index = FrdIndex::open(path).expect("index");
        assert_equivalent(&index, &model, None);
        assert_eq!(index.mesh().sets.len(), model.sets.len(), "sets of {}", path);

        // Filter on every third node.
        let filter: HashSet<u32> = model.nodes.iter().step_by(3).map(|n| n.id).collect();
        assert_equivalent(&index, &model, Some(&filter));
        println!("OK {} ({} datasets, {} steps)", path, index.entries().len(), index.steps().len());
    }
}

const CYCLIC_FILE: &str =
    "/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor/lavffcyc.frd";

/// Cyclic symmetry run: one static step, then three modes of nodal diameter 5 stored as complex
/// results (STRESS + STRESSI, ERROR + ERRORI, PDISP).
#[test]
fn cyclic_modal_file_metadata() {
    if !Path::new(CYCLIC_FILE).exists() {
        eprintln!("Skipping: {} not found", CYCLIC_FILE);
        return;
    }
    let index = FrdIndex::open(CYCLIC_FILE).expect("index");
    let steps = index.steps();
    assert_eq!(steps.len(), 4);
    assert_eq!((steps[0].mode, steps[0].nodal_diameter, steps[0].pstep), (None, None, Some((1, 1, 1))));
    for s in &steps[1..] {
        assert!(s.mode.is_some(), "step {} has no mode", s.step_number);
        assert_eq!(s.nodal_diameter, Some(5));
        assert_eq!(s.pstep.map(|p| p.2), Some(2), "analysis step of the deck");
    }
    assert_eq!(steps.iter().map(|s| s.mode).collect::<Vec<_>>(), [None, Some(1), Some(3), Some(5)]);
    // Every dataset of a step carries the same parameters.
    for s in steps {
        for &e in &s.entries {
            let entry = &index.entries()[e];
            assert_eq!((entry.mode, entry.nodal_diameter), (s.mode, s.nodal_diameter), "{}", entry.name);
        }
    }

    let stress = index.stress_history(None);
    assert_eq!(stress.iter().map(|s| s.step_number).collect::<Vec<_>>(), steps.iter().map(|s| s.step_number).collect::<Vec<_>>());
    // DISP only in the static step; the modal steps have PDISP.
    assert_eq!(index.disp_history(None).len(), 1);
    let pdisp = steps[1].entries.iter().copied().find(|&e| index.entries()[e].name == "PDISP").expect("PDISP");
    let summ = index.load(pdisp).unwrap().pdisp_extrema(None).expect("PDISP amplitude");
    assert!(summ.max_disp.0 > 0.0 && summ.max_disp.0.is_finite());
}

/// Timing comparison. Run one at a time under `/usr/bin/time -l` for peak memory:
/// `FRD=<file> cargo test --release -p cgx-core --test frd_index bench_lazy -- --ignored --nocapture`
fn bench_file() -> Option<String> {
    let path = std::env::var("FRD").unwrap_or_else(|_| REAL_FILES[0].to_string());
    Path::new(&path).exists().then_some(path)
}

#[test]
#[ignore]
fn bench_lazy() {
    let Some(path) = bench_file() else { return };
    let t = std::time::Instant::now();
    let index = FrdIndex::open(&path).unwrap();
    let t_open = t.elapsed();
    let (s, d) = (index.stress_history(None), index.disp_history(None));
    println!(
        "LAZY  {}: open {:.2?}, open + histories {:.2?} ({} datasets, {} stress, {} disp rows)",
        path, t_open, t.elapsed(), index.entries().len(), s.len(), d.len()
    );
    let payload: usize = index.entries().iter().map(|e| e.payload.len()).sum();
    println!(
        "      {} nodes, {} elements, result payload {:.1} MB",
        index.mesh().nodes.len(), index.mesh().elements.len(), payload as f64 / 1e6
    );
    for e in index.entries() {
        println!(
            "      step {:>4} value {:<12} {:<8} fmt {} nodes {:>8} comps {:?}",
            e.step_number, e.value, e.name, e.format, e.node_count,
            e.components.iter().map(|c| c.0.as_str()).collect::<Vec<_>>()
        );
    }
}

#[test]
#[ignore]
fn bench_full() {
    let Some(path) = bench_file() else { return };
    let t = std::time::Instant::now();
    let model = cgx_core::parse_frd_file(&path).unwrap();
    let t_parse = t.elapsed();
    let (s, d) = (model.compute_stress_history(None), model.compute_disp_history(None));
    println!(
        "FULL  {}: parse {:.2?}, parse + histories {:.2?} ({} stress, {} disp rows)",
        path, t_parse, t.elapsed(), s.len(), d.len()
    );
}
