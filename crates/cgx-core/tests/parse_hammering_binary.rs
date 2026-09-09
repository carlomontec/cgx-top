use cgx_core::{parse_frd_file, ElementType, FieldType};
use std::path::PathBuf;

#[test]
fn test_parse_hammering_binary() {
    let test_path = PathBuf::from("/Users/carlo/Downloads/CCXMDL_20211201_hammering_SC2000_contactmpc_origK_glob.frd");
    if !test_path.exists() {
        eprintln!("Skipping test: hammering file not found at {:?}", test_path);
        return;
    }

    let start = std::time::Instant::now();
    let model = parse_frd_file(&test_path).expect("Failed to parse binary hammering FRD");
    let elapsed = start.elapsed();

    println!("\nParsed 88.8 MB Binary FRD in {:.2?}!", elapsed);
    println!("  Nodes: {}", model.nodes.len());
    println!("  Elements: {}", model.elements.len());
    println!("  Steps: {}", model.steps.len());

    // Assertions
    assert_eq!(model.nodes.len(), 366_987, "Expected 366,987 nodes");
    assert_eq!(model.elements.len(), 156_443, "Expected 156,443 elements");

    // Verify first node coordinates: Node 1: ID=1, x=-231.9099, y=45.5600, z=-9.0598
    let node_1 = model.get_node(1).expect("Node 1 must exist");
    assert_eq!(node_1.id, 1);
    assert!((node_1.x - (-231.9099)).abs() < 1e-3);
    assert!((node_1.y - 45.5600).abs() < 1e-3);
    assert!((node_1.z - (-9.0598)).abs() < 1e-3);

    // Verify element #1: Type 4 (C3D20)
    let elem_1 = &model.elements[0];
    assert_eq!(elem_1.id, 1);
    assert_eq!(elem_1.elem_type, ElementType::C3D20);
    assert_eq!(elem_1.nodes.len(), 20);

    // Verify Mode 1
    assert!(!model.steps.is_empty(), "Expected modal steps");
    let mode_1 = &model.steps[0];
    assert_eq!(mode_1.step_number, 1);
    assert!((mode_1.value - 252.13417).abs() < 1e-3, "Mode 1 frequency mismatch: got {}", mode_1.value);

    // Verify dataset
    assert_eq!(mode_1.datasets.len(), 1);
    let ds = &mode_1.datasets[0];
    assert_eq!(ds.name, "DISP");
    assert_eq!(ds.field_type, FieldType::Displacement);
    assert_eq!(ds.components.len(), 4); // D1, D2, D3, ALL
}
