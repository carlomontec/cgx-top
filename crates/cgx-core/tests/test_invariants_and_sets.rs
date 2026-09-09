use cgx_core::{
    consolidate_inp, inspect_inp, parse_frd_file,
    parse_sets_from_str,
};
use std::path::PathBuf;

#[test]
fn test_parse_sets_from_str() {
    let inp_text = r#"
*NSET, NSET=FIX_BASE
1, 2, 3, 4,
5, 6
*NSET, NSET=RANGE_NODES, GENERATE
10, 20, 2
*ELSET, ELSET=BEAM_ELEMS, GENERATE
1, 5
"#;

    let sets = parse_sets_from_str(inp_text);
    assert_eq!(sets.len(), 3);

    let fix = sets.get("FIX_BASE").expect("FIX_BASE must exist");
    assert_eq!(fix.node_ids, vec![1, 2, 3, 4, 5, 6]);

    let range = sets.get("RANGE_NODES").expect("RANGE_NODES must exist");
    assert_eq!(range.node_ids, vec![10, 12, 14, 16, 18, 20]);

    let elems = sets.get("BEAM_ELEMS").expect("BEAM_ELEMS must exist");
    assert_eq!(elems.element_ids, vec![1, 2, 3, 4, 5]);
}

#[test]
fn test_consolidate_beam_tet() {
    let inp_path = PathBuf::from("/Users/carlo/code/CalculiX/cgx/CalculiX-CGX-New3D/test_newtetgen/05_end_to_end_ccx_simulation/beam_tet.inp");
    if !inp_path.exists() {
        eprintln!("Skipping test: beam_tet.inp not found");
        return;
    }

    let (consolidated, stats) = consolidate_inp(&inp_path).expect("Consolidation must succeed");

    assert!(stats.total_files_inlined >= 2, "Expected at least 2 inlined files, got {}", stats.total_files_inlined);
    assert!(consolidated.contains("BEGIN INCLUDE: all.msh"), "Should contain all.msh include header");
    assert!(consolidated.contains("BEGIN INCLUDE: tip.nam"), "Should contain tip.nam include header");
    assert!(!consolidated.contains("*INCLUDE, INPUT=all.msh"), "Original *INCLUDE directive should be inlined");
}

#[test]
fn test_inspect_beam20p_dense() {
    let inp_path = PathBuf::from("/Users/carlo/code/CalculiX/cgx/CalculiX-CGX-New3D/test/beam20p_dense.inp");
    if !inp_path.exists() {
        eprintln!("Skipping test: beam20p_dense.inp not found");
        return;
    }

    let summary = inspect_inp(&inp_path).expect("Inspection must succeed");
    assert!(summary.includes.len() >= 3, "Expected at least 3 includes (all.msh, NFIX.nam, NTIP.nam)");
    for inc in &summary.includes {
        assert!(inc.exists, "Include {} should exist on disk", inc.raw_target);
    }

    // Must have parsed sets from the included .nam files
    assert!(summary.node_sets.contains_key("NNFIX") || summary.node_sets.contains_key("NFIX"));
}

#[test]
fn test_result_frd_stress_invariants() {
    let frd_path = PathBuf::from("/Users/carlo/code/CalculiX/cgx/CalculiX-CGX-New3D/cgx_2.23/examples/result.frd");
    if !frd_path.exists() {
        eprintln!("Skipping test: result.frd not found");
        return;
    }

    let model = parse_frd_file(&frd_path).expect("result.frd parse failed");
    assert_eq!(model.nodes.len(), 3150);

    // Verify implicit sets
    assert!(model.sets.contains_key("+all"));
    assert!(model.sets.contains_key("+C3D20"));

    // Verify stress invariants across all modes
    let stress_history = model.compute_stress_history(None);
    assert_eq!(stress_history.len(), 10, "Expected 10 modal stress steps");

    for (idx, entry) in stress_history.iter().enumerate() {
        assert!(entry.max_von_mises.0 > 0.0, "Mode {} von Mises must be positive", idx + 1);
        assert!(entry.max_principal_s1.0 > entry.min_principal_s3.0, "S1 must be >= S3");
        assert!(entry.max_tresca.0 > 0.0, "Tresca shear must be positive");
    }

    // Mode 1 peak stress check
    let mode_1 = &stress_history[0];
    println!("Mode 1 peak von Mises: {:.3e} at Node {}", mode_1.max_von_mises.0, mode_1.max_von_mises.1);
    println!("Mode 1 peak S1: {:.3e} at Node {}", mode_1.max_principal_s1.0, mode_1.max_principal_s1.1);
    assert!(mode_1.max_von_mises.0 > 1.0e6);
}

#[test]
fn test_beam20p_companion_sets() {
    let frd_path = PathBuf::from("/Users/carlo/code/CalculiX/cgx/CalculiX-CGX-New3D/test/beam20p.frd");
    if !frd_path.exists() {
        eprintln!("Skipping test: beam20p.frd not found");
        return;
    }

    let model = parse_frd_file(&frd_path).expect("beam20p.frd parse failed");

    // Auto-discovery of beam20p.inp should have loaded sets: CN7, NALL, EALL, LAST
    assert!(model.sets.contains_key("CN7"), "Set CN7 must be discovered from beam20p.inp");
    assert!(model.sets.contains_key("LAST"), "Set LAST must be discovered from beam20p.inp");
    assert!(model.sets.contains_key("EALL"), "Set EALL must be discovered from beam20p.inp");

    // Test element set expansion: EALL has 32 elements, each C3D20 (20 nodes)
    let eall_nodes = model.resolve_set_node_ids("EALL").expect("EALL node expansion failed");
    assert_eq!(eall_nodes.len(), 261, "EALL should expand to all 261 mesh nodes");

    // Filtered stress history on set CN7 (fixed boundary nodes with stress)
    let cn7_nodes = model.resolve_set_node_ids("CN7").expect("CN7 node resolution failed");
    let stress_filtered = model.compute_stress_history(Some(&cn7_nodes));
    assert!(!stress_filtered.is_empty(), "Expected stress results on CN7 nodes");
    assert_eq!(stress_filtered[0].node_count, 21);

    // Filtered displacement history on set NALL
    let nall_nodes = model.resolve_set_node_ids("NALL").expect("NALL node resolution failed");
    let disp_filtered = model.compute_disp_history(Some(&nall_nodes));
    assert!(!disp_filtered.is_empty(), "Expected displacement results on NALL nodes");
    assert_eq!(disp_filtered[0].node_count, 261);
}
