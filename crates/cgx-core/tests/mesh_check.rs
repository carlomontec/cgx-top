use cgx_core::*;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn load(deck: &str) -> (InpMesh, InpScan) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir: PathBuf = std::env::temp_dir().join(format!("cgx_mesh_check_{}_{}", std::process::id(), n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("deck.inp");
    fs::write(&p, deck).unwrap();
    let mesh = read_inp_mesh(&p).unwrap();
    let scan = scan_cards(&p, &ScanLimits::default()).unwrap();
    (mesh, scan)
}

/// `n`^3 block of unit C3D8 starting at `origin`, node ids from `first_node`, element ids
/// from `first_elem`, all in ELSET `name`.
fn block(n: usize, origin: [f64; 3], first_node: usize, first_elem: usize, name: &str) -> String {
    let id = |i: usize, j: usize, k: usize| first_node + i + (n + 1) * (j + (n + 1) * k);
    let mut s = String::from("*NODE\n");
    for k in 0..=n {
        for j in 0..=n {
            for i in 0..=n {
                s += &format!("{}, {}, {}, {}\n", id(i, j, k), origin[0] + i as f64, origin[1] + j as f64, origin[2] + k as f64);
            }
        }
    }
    s += &format!("*ELEMENT, TYPE=C3D8, ELSET={name}\n");
    let mut e = first_elem;
    for k in 0..n {
        for j in 0..n {
            for i in 0..n {
                let c = [
                    id(i, j, k), id(i + 1, j, k), id(i + 1, j + 1, k), id(i, j + 1, k),
                    id(i, j, k + 1), id(i + 1, j, k + 1), id(i + 1, j + 1, k + 1), id(i, j + 1, k + 1),
                ];
                let c: Vec<String> = c.iter().map(|v| v.to_string()).collect();
                s += &format!("{}, {}\n", e, c.join(", "));
                e += 1;
            }
        }
    }
    s
}

const STEEL: &str = "*MATERIAL, NAME=Steel\n*ELASTIC\n210000., 0.3\n*DENSITY\n7.85e-9\n*SOLID SECTION, ELSET=Blk, MATERIAL=STEEL\n";

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * (1.0 + b.abs())
}

#[test]
fn block_is_one_clean_part() {
    let (m, _) = load(&block(2, [0.0; 3], 1, 1, "Blk"));
    assert_eq!(m.elements.len(), 8);
    for i in 0..8 {
        assert!(close(element_volume(&m, i).unwrap(), 1.0));
    }
    let parts = components(&m);
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0].len(), 8);
    assert!(free_nodes(&m).is_empty());
    let q = quality(&m);
    assert!(q.non_positive_volume.is_empty());
    assert!(close(q.max_aspect, 1.0));
    assert_eq!(q.worst_aspect.len(), 8);
}

#[test]
fn two_blocks_are_two_parts_and_stretched_element_has_aspect() {
    let deck = format!("{}{}", block(2, [0.0; 3], 1, 1, "Blk"), block(1, [10.0, 0.0, 0.0], 100, 100, "Other"));
    let (m, scan) = load(&deck);
    let parts = components(&m);
    assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 1]);
    let r = mass_properties(&m, &scan);
    assert!(close(r.volume, 9.0));
    assert_eq!(r.bounding_box, ([0.0; 3], [11.0, 2.0, 2.0]));

    // One brick 4 x 1 x 1: aspect 4.
    let (m, _) = load("*NODE\n1,0,0,0\n2,4,0,0\n3,4,1,0\n4,0,1,0\n5,0,0,1\n6,4,0,1\n7,4,1,1\n8,0,1,1\n*ELEMENT, TYPE=C3D8\n1,1,2,3,4,5,6,7,8\n");
    assert!(close(quality(&m).max_aspect, 4.0));
}

#[test]
fn unused_node_is_free() {
    let deck = format!("{}*NODE\n999, 5, 5, 5\n", block(1, [0.0; 3], 1, 1, "Blk"));
    let (m, _) = load(&deck);
    assert_eq!(free_nodes(&m), vec![999]);
}

#[test]
fn inverted_element_has_negative_volume() {
    // Swap the top and bottom faces of a unit cube.
    let (m, _) = load("*NODE\n1,0,0,0\n2,1,0,0\n3,1,1,0\n4,0,1,0\n5,0,0,1\n6,1,0,1\n7,1,1,1\n8,0,1,1\n*ELEMENT, TYPE=C3D8\n1,5,6,7,8,1,2,3,4\n");
    assert!(close(element_volume(&m, 0).unwrap(), -1.0));
    assert_eq!(quality(&m).non_positive_volume, vec![1]);
}

#[test]
fn mass_and_centre_of_gravity() {
    let deck = format!("{}{}", block(2, [0.0; 3], 1, 1, "Blk"), STEEL);
    let (m, scan) = load(&deck);
    let r = mass_properties(&m, &scan);
    assert!(close(r.volume, 8.0));
    assert!(close(r.mass.unwrap(), 8.0 * 7.85e-9));
    let cg = r.centre_of_gravity.unwrap();
    assert!(cg.iter().all(|c| close(*c, 1.0)), "{cg:?}");
    assert_eq!(r.unassigned_elements, 0);
    assert_eq!(r.by_material.len(), 1);
    let mm = &r.by_material[0];
    assert_eq!((mm.material.as_str(), mm.elset.as_str(), mm.elements), ("STEEL", "BLK", 8));
    assert_eq!(mm.density, Some(7.85e-9));
    assert!(close(mm.mass.unwrap(), 8.0 * 7.85e-9));
}

#[test]
fn missing_density_gives_no_mass_and_unassigned_is_counted() {
    let deck = format!(
        "{}*MATERIAL, NAME=NODENS\n*ELASTIC\n1.,0.3\n*SOLID SECTION, ELSET=Blk, MATERIAL=NODENS\n",
        block(1, [0.0; 3], 1, 1, "Blk")
    );
    let (m, scan) = load(&deck);
    let r = mass_properties(&m, &scan);
    assert!(r.mass.is_none() && r.centre_of_gravity.is_none());
    assert_eq!(r.by_material[0].density, None);

    let (m, scan) = load(&block(1, [0.0; 3], 1, 1, "Blk"));
    let r = mass_properties(&m, &scan);
    assert_eq!((r.unassigned_elements, r.mass), (1, None));
}

#[test]
fn reference_problems_report_undefined_things() {
    let deck = format!(
        "{}*NSET, NSET=EMPTYN\n*MATERIAL, NAME=STEEL\n*DENSITY\n1.\n\
*SOLID SECTION, ELSET=Blk, MATERIAL=GHOST\n*SOLID SECTION, ELSET=NOELS, MATERIAL=STEEL\n\
*SURFACE, NAME=SURFX, TYPE=ELEMENT\n1, S1\n\
*STEP\n*STATIC\n*BOUNDARY\nNOPE, 1, 3\n1, 1, 3\nEMPTYN, 1\n*CLOAD\nBlk, 1, 1.\n\
*DSLOAD\nSURFX, P, 1.\nSURFY, P, 1.\n*DLOAD\nBlk, GRAV, 1., 0., 0., -1.\n*END STEP\n",
        block(1, [0.0; 3], 1, 1, "Blk")
    );
    let (m, scan) = load(&deck);
    let p = reference_problems(&m, &scan);
    let has = |needle: &str| p.iter().any(|s| s.contains(needle));
    assert!(has("material 'GHOST' is not defined"), "{p:#?}");
    assert!(has("element set 'NOELS' is not defined"), "{p:#?}");
    assert!(has("node set 'NOPE' is not defined"), "{p:#?}");
    assert!(has("node set 'EMPTYN' is empty"), "{p:#?}");
    // BLK is an element set, not a node set: *CLOAD must flag it.
    assert!(has("*CLOAD: node set 'BLK'"), "{p:#?}");
    assert!(has("surface") || has("SURFY"), "{p:#?}");
    // Defined or numeric references are not reported.
    assert!(!has("SURFX") && !has("'STEEL'") && !p.iter().any(|s| s.contains("*DLOAD")), "{p:#?}");
    assert!(p.iter().all(|s| s.contains("deck.inp:")), "{p:#?}");
}

/// Unit cube as C3D20 (corner + mid-side nodes) and as C3D8; both of size `s` x `s` x `s`.
fn quad_cube(s: f64) -> String {
    let c: [[f64; 3]; 8] = [
        [0., 0., 0.], [1., 0., 0.], [1., 1., 0.], [0., 1., 0.],
        [0., 0., 1.], [1., 0., 1.], [1., 1., 1.], [0., 1., 1.],
    ];
    let edges = [(0, 1), (1, 2), (2, 3), (3, 0), (4, 5), (5, 6), (6, 7), (7, 4), (0, 4), (1, 5), (2, 6), (3, 7)];
    let mut pts: Vec<[f64; 3]> = c.to_vec();
    for (a, b) in edges {
        pts.push([(c[a][0] + c[b][0]) / 2.0, (c[a][1] + c[b][1]) / 2.0, (c[a][2] + c[b][2]) / 2.0]);
    }
    let mut d = String::from("*NODE\n");
    for (i, p) in pts.iter().enumerate() {
        d += &format!("{}, {}, {}, {}\n", i + 1, p[0] * s, p[1] * s, p[2] * s);
    }
    d += "*ELEMENT, TYPE=C3D20\n1, 1, 2, 3, 4, 5, 6, 7, 8,\n9, 10, 11, 12, 13, 14, 15, 16,\n17, 18, 19, 20\n";
    d
}

#[test]
fn c3d20_volume_matches_c3d8_and_face_nodes_are_planar() {
    let (q, _) = load(&quad_cube(2.0));
    let (l, _) = load("*NODE\n1,0,0,0\n2,2,0,0\n3,2,2,0\n4,0,2,0\n5,0,0,2\n6,2,0,2\n7,2,2,2\n8,0,2,2\n*ELEMENT, TYPE=C3D8\n1,1,2,3,4,5,6,7,8\n");
    assert_eq!(q.elements[0].nodes.len(), 20);
    assert!(close(element_volume(&q, 0).unwrap(), element_volume(&l, 0).unwrap()));
    assert!(close(element_volume(&q, 0).unwrap(), 8.0));

    let faces = exterior_faces(&q);
    assert_eq!(faces.len(), 6);
    for f in &faces {
        let nodes = face_nodes(&q, f);
        assert_eq!(nodes.len(), 8, "face S{}", f.face);
        let mut sorted = nodes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 8);
        // Every node lies on the plane of the face (through the centroid, along the normal).
        for id in nodes {
            let n = &q.nodes[q.node_index[&id]];
            let d = (n.x - f.centroid[0]) * f.normal[0] + (n.y - f.centroid[1]) * f.normal[1] + (n.z - f.centroid[2]) * f.normal[2];
            assert!(d.abs() < 1e-12, "face S{} node {id} off plane by {d}", f.face);
        }
    }
    // Linear elements return their corners.
    let lf = exterior_faces(&l);
    assert_eq!(face_nodes(&l, &lf[0]).len(), 4);
}

#[test]
fn quadratic_tet_and_wedge_face_node_counts() {
    // Corners only matter for the counts; coordinates are arbitrary but distinct.
    let mut tet = String::from("*NODE\n");
    for i in 1..=10 {
        tet += &format!("{i}, {}, {}, {}\n", i as f64, (i * i) as f64 % 7.0, (i * 3) as f64 % 5.0);
    }
    tet += "*ELEMENT, TYPE=C3D10\n1, 1,2,3,4,5,6,7,8,9,10\n";
    let (m, _) = load(&tet);
    let f = Face { element: 1, face: 2, corners: vec![], centroid: [0.0; 3], normal: [0.0; 3], area: 0.0 };
    // S2 = 1-2-4 with mid-side nodes 5, 9, 8 (ccx table ifacet).
    assert_eq!(face_nodes(&m, &f), vec![1, 2, 4, 5, 9, 8]);

    let mut w = String::from("*NODE\n");
    for i in 1..=15 {
        w += &format!("{i}, {}, {}, {}\n", i as f64, (i * i) as f64 % 11.0, (i * 3) as f64 % 5.0);
    }
    w += "*ELEMENT, TYPE=C3D15\n1, 1,2,3,4,5,6,7,8,9,10,11,12,13,14,15\n";
    let (m, _) = load(&w);
    let tri = Face { element: 1, face: 1, ..f.clone() };
    let quad = Face { element: 1, face: 3, ..f };
    assert_eq!(face_nodes(&m, &tri).len(), 6);
    assert_eq!(face_nodes(&m, &quad), vec![1, 2, 5, 4, 7, 14, 10, 13]);
}

#[test]
fn on_cylinder_selects_only_the_bore() {
    // Annulus r in [1, 2], 16 segments, one layer 0 <= z <= 1.
    let seg = 16;
    let node = |ring: usize, i: usize, k: usize| 1 + (i % seg) + seg * (ring + 2 * k);
    let mut d = String::from("*NODE\n");
    for k in 0..2 {
        for ring in 0..2 {
            for i in 0..seg {
                let (r, th) = (1.0 + ring as f64, 2.0 * std::f64::consts::PI * i as f64 / seg as f64);
                d += &format!("{}, {:.15}, {:.15}, {}\n", node(ring, i, k), r * th.cos(), r * th.sin(), k);
            }
        }
    }
    d += "*ELEMENT, TYPE=C3D8\n";
    for i in 0..seg {
        let c = [node(0, i, 0), node(1, i, 0), node(1, i + 1, 0), node(0, i + 1, 0), node(0, i, 1), node(1, i, 1), node(1, i + 1, 1), node(0, i + 1, 1)];
        let c: Vec<String> = c.iter().map(|v| v.to_string()).collect();
        d += &format!("{}, {}\n", i + 1, c.join(", "));
    }
    let (m, _) = load(&d);
    assert!((0..seg).all(|i| element_volume(&m, i).unwrap() > 0.0));
    let faces = exterior_faces(&m);
    assert_eq!(faces.len(), 64);
    let bore = on_cylinder(&m, &faces, [0.0; 3], [0.0, 0.0, 5.0], 1.0, 1e-6);
    assert_eq!(bore.len(), seg);
    assert!(bore.iter().all(|&i| faces[i].face == 6), "bore faces are S6");
    let outer = on_cylinder(&m, &faces, [0.0; 3], [0.0, 0.0, 1.0], 2.0, 1e-6);
    assert_eq!(outer.len(), seg);
    assert!(on_cylinder(&m, &faces, [0.0; 3], [0.0, 0.0, 1.0], 1.5, 1e-6).is_empty());

    // near_point / nearest_face
    let p = [1.0, 0.0, 0.5];
    let nf = nearest_face(&faces, p).unwrap();
    assert!(bore.contains(&nf));
    assert!(near_point(&faces, p, 0.5).contains(&nf));
    assert!(near_point(&faces, [100.0, 0.0, 0.0], 0.1).is_empty());
    assert!(nearest_face(&[], p).is_none());
}

fn surface_closure_and_volume(mesh: &InpMesh) -> (f64, f64, f64) {
    let faces = exterior_faces(mesh);
    let mut total_area = 0.0;
    let mut normal_sum = [0.0; 3];
    let mut tri_normal_sum = [0.0; 3];
    let mut vol_sum = 0.0;
    let pt = |id: u32| {
        let n = &mesh.nodes[mesh.node_index[&id]];
        [n.x, n.y, n.z]
    };
    let sub = |a: [f64; 3], b: [f64; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let cross = |a: [f64; 3], b: [f64; 3]| [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
    let dot = |a: [f64; 3], b: [f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];

    for f in &faces {
        total_area += f.area;
        for k in 0..3 {
            normal_sum[k] += f.area * f.normal[k];
        }
        // Triangulate the face (fan from corner 0) so warped quads are integrated cleanly.
        let p0 = pt(f.corners[0]);
        for i in 1..f.corners.len() - 1 {
            let p1 = pt(f.corners[i]);
            let p2 = pt(f.corners[i + 1]);
            let area_vec_x2 = cross(sub(p1, p0), sub(p2, p0));
            for k in 0..3 {
                tri_normal_sum[k] += 0.5 * area_vec_x2[k];
            }
            let c = [(p0[0] + p1[0] + p2[0]) / 3.0, (p0[1] + p1[1] + p2[1]) / 3.0, (p0[2] + p1[2] + p2[2]) / 3.0];
            vol_sum += dot(c, area_vec_x2) / 6.0;
        }
    }
    let norm = |v: [f64; 3]| (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    (norm(normal_sum) / total_area, norm(tri_normal_sum) / total_area, vol_sum)
}

#[test]
fn closed_surface_identity_and_surface_integral_volume() {
    for (label, deck) in [("block2", block(2, [0.0; 3], 1, 1, "Blk")), ("quad_cube", quad_cube(2.0))] {
        let (m, _) = load(&deck);
        let elem_vol: f64 = (0..m.elements.len()).map(|i| element_volume(&m, i).unwrap()).sum();
        let (closure_res, tri_closure_res, surf_vol) = surface_closure_and_volume(&m);
        let rel_vol_err = (surf_vol - elem_vol).abs() / elem_vol;
        println!("{label}: closure_res={closure_res:.3e}, tri_closure_res={tri_closure_res:.3e}, elem_vol={elem_vol:.9}, surf_vol={surf_vol:.9}, rel_err={rel_vol_err:.3e}");
        assert!(closure_res < 1e-12, "{label} closure residual {closure_res}");
        assert!(tri_closure_res < 1e-12, "{label} tri closure residual {tri_closure_res}");
        assert!(rel_vol_err < 1e-9, "{label} volume mismatch {rel_vol_err}");
    }

    let rotor = PathBuf::from("/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor/lavffcyc.inp");
    if !rotor.exists() {
        return;
    }
    let m = read_inp_mesh(&rotor).unwrap();
    let elem_vol: f64 = (0..m.elements.len()).map(|i| element_volume(&m, i).unwrap()).sum();
    let (closure_res, tri_closure_res, surf_vol) = surface_closure_and_volume(&m);
    let rel_vol_err = (surf_vol - elem_vol).abs() / elem_vol;
    println!("lavffcyc: closure_res={closure_res:.3e}, tri_closure_res={tri_closure_res:.3e}, elem_vol={elem_vol:.6}, surf_vol={surf_vol:.6}, rel_err={rel_vol_err:.3e}");
    assert!(closure_res < 1e-3, "rotor face-normal closure residual {closure_res}");
    assert!(tri_closure_res < 1e-12, "rotor triangulated closure residual {tri_closure_res}");
    assert!(rel_vol_err < 0.01, "rotor surface vs element volume mismatch {rel_vol_err}");
}

#[test]
fn curved_c3d20_annulus_corner_node_approximation_error() {
    // Annulus r in [1, 2], 16 segments, 0 <= z <= 1 with curved C3D20 mid-side nodes on the circles.
    let seg = 16usize;
    let mut nodes: Vec<(usize, f64, f64, f64)> = Vec::new();
    let mut next_id = 1usize;
    let mut pt_map = std::collections::HashMap::<(usize, usize, usize), usize>::new();
    let mut add_pt = |ring2: usize, i2: usize, k2: usize| -> usize {
        let key = (ring2, i2 % (2 * seg), k2);
        if let Some(&id) = pt_map.get(&key) {
            return id;
        }
        let r = 1.0 + 0.5 * ring2 as f64;
        let th = 2.0 * std::f64::consts::PI * (key.1 as f64) / ((2 * seg) as f64);
        let z = 0.5 * k2 as f64;
        let id = next_id;
        next_id += 1;
        nodes.push((id, r * th.cos(), r * th.sin(), z));
        pt_map.insert(key, id);
        id
    };

    let mut elems = Vec::new();
    for i in 0..seg {
        let i0 = 2 * i;
        let im = 2 * i + 1;
        let i1 = 2 * i + 2;
        let n = [
            // corners: bottom (k=0) then top (k=2)
            add_pt(0, i0, 0), add_pt(2, i0, 0), add_pt(2, i1, 0), add_pt(0, i1, 0),
            add_pt(0, i0, 2), add_pt(2, i0, 2), add_pt(2, i1, 2), add_pt(0, i1, 2),
            // mid-sides bottom: 8..11
            add_pt(1, i0, 0), add_pt(2, im, 0), add_pt(1, i1, 0), add_pt(0, im, 0),
            // mid-sides top: 12..15
            add_pt(1, i0, 2), add_pt(2, im, 2), add_pt(1, i1, 2), add_pt(0, im, 2),
            // vertical mid-sides: 16..19
            add_pt(0, i0, 1), add_pt(2, i0, 1), add_pt(2, i1, 1), add_pt(0, i1, 1),
        ];
        elems.push((i + 1, n));
    }

    let mut d = String::from("*NODE\n");
    for (id, x, y, z) in nodes {
        d += &format!("{id}, {x:.15}, {y:.15}, {z:.15}\n");
    }
    d += "*ELEMENT, TYPE=C3D20\n";
    for (eid, ns) in elems {
        let s: Vec<String> = ns.iter().map(|v| v.to_string()).collect();
        d += &format!("{eid}, {}\n", s.join(", "));
    }

    let (m, _) = load(&d);
    let corner_vol: f64 = (0..m.elements.len()).map(|i| element_volume(&m, i).unwrap()).sum();
    let analytic_vol = std::f64::consts::PI * (4.0 - 1.0) * 1.0;
    let rel_err = (analytic_vol - corner_vol).abs() / analytic_vol;
    println!("C3D20 16-seg annulus: analytic={analytic_vol:.6}, corner_vol={corner_vol:.6}, rel_err={:.3}%", rel_err * 100.0);
    // Exact 16-gon prism shortfall is 1 - sin(pi/8)/(pi/8) ≈ 2.55%.
    assert!((rel_err - 0.0255).abs() < 0.001, "unexpected corner-node error {rel_err}");
}
