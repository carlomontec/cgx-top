use cgx_core::{parse_frd_file, ElementType, FieldType};
use std::path::PathBuf;

#[test]
fn test_parse_beam_modal() {
    let test_path = PathBuf::from("/Users/carlo/code/CalculiX/cgx/CalculiX-CGX-New3D/test/beam_modal.frd");
    if !test_path.exists() {
        eprintln!("Skipping test: beam_modal.frd not found at {:?}", test_path);
        return;
    }

    let model = parse_frd_file(&test_path).expect("Failed to parse beam_modal.frd");

    // Check node count
    assert_eq!(model.nodes.len(), 77, "Expected exactly 77 nodes");
    assert_eq!(model.node_index.len(), 77);

    // Verify first and last node coordinates
    let first_node = model.get_node(1).expect("Node 1 should exist");
    assert_eq!(first_node.id, 1);
    assert!((first_node.x - 0.0).abs() < 1e-4);
    assert!((first_node.y - 0.0).abs() < 1e-4);
    assert!((first_node.z - 0.0).abs() < 1e-4);

    let last_node = model.get_node(77).expect("Node 77 should exist");
    assert_eq!(last_node.id, 77);
    assert!((last_node.x - 45.0).abs() < 1e-4);
    assert!((last_node.y - 25.0).abs() < 1e-4);

    // Check element count and topology
    assert_eq!(model.elements.len(), 120, "Expected exactly 120 elements");
    for elem in &model.elements {
        assert_eq!(elem.elem_type, ElementType::S3, "Expected TRI3 (S3) shell elements");
        assert_eq!(elem.nodes.len(), 3, "TRI3 elements must have 3 connectivity nodes");
    }

    // Verify element #1 connectivity: nodes [1, 2, 13]
    let elem_1 = &model.elements[0];
    assert_eq!(elem_1.id, 1);
    assert_eq!(elem_1.nodes, vec![1, 2, 13]);

    // Check modal steps / eigenvalues
    assert_eq!(model.steps.len(), 10, "Expected 10 vibration modes in beam_modal.frd");

    // Check Mode 1 frequency (~0.397161 Hz)
    let mode_1 = &model.steps[0];
    assert_eq!(mode_1.step_number, 1);
    assert!((mode_1.value - 0.397161).abs() < 1e-5, "Mode 1 frequency mismatch: got {}", mode_1.value);

    // Check Mode 1 dataset
    assert_eq!(mode_1.datasets.len(), 1);
    let ds = &mode_1.datasets[0];
    assert_eq!(ds.name, "DISP");
    assert_eq!(ds.field_type, FieldType::Displacement);
    assert_eq!(ds.components.len(), 7); // D1, D2, D3, D4, D5, D6, ALL

    let comp_names: Vec<&str> = ds.components.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(comp_names, vec!["D1", "D2", "D3", "D4", "D5", "D6", "ALL"]);

    // Check nodal values are populated
    let d3 = &ds.components[2];
    assert_eq!(d3.name, "D3");
    assert_eq!(d3.values.len(), 77, "Expected displacement values for all 77 nodes");

    println!("SUCCESS: Successfully parsed beam_modal.frd!");
    println!("  Nodes: {}", model.nodes.len());
    println!("  Elements: {}", model.elements.len());
    println!("  Modes: {}", model.steps.len());
    println!("  Mode 1 freq: {:.6} Hz", mode_1.value);
}
