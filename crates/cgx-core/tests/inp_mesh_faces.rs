use cgx_core::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn tmp_dir(name: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("cgx_inp_mesh_{}_{}_{}", std::process::id(), n, name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, content).unwrap();
    p
}

fn read(name: &str, deck: &str) -> InpMesh {
    let dir = tmp_dir(name);
    let p = write(&dir, "deck.inp", deck);
    read_inp_mesh(&p).unwrap()
}

const CUBE: &str = "\
*NODE
1, 0, 0, 0
2, 1, 0, 0
3, 1, 1, 0
4, 0, 1, 0
5, 0, 0, 1
6, 1, 0, 1
7, 1, 1, 1
8, 0, 1, 1
*ELEMENT, TYPE=C3D8, ELSET=EALL
1, 1, 2, 3, 4, 5, 6, 7, 8
";

/// Nx x Ny x Nz block of C3D8 elements, node id = 1 + i + (nx+1)*(j + (ny+1)*k).
fn block_deck(nx: usize, ny: usize, nz: usize) -> String {
    let id = |i: usize, j: usize, k: usize| 1 + i + (nx + 1) * (j + (ny + 1) * k);
    let mut s = String::from("*NODE\n");
    for k in 0..=nz {
        for j in 0..=ny {
            for i in 0..=nx {
                s.push_str(&format!("{}, {}, {}, {}\n", id(i, j, k), i, j, k));
            }
        }
    }
    s.push_str("*ELEMENT, TYPE=C3D8, ELSET=EALL\n");
    let mut e = 1;
    for k in 0..nz {
        for j in 0..ny {
            for i in 0..nx {
                let n = [
                    id(i, j, k), id(i + 1, j, k), id(i + 1, j + 1, k), id(i, j + 1, k),
                    id(i, j, k + 1), id(i + 1, j, k + 1), id(i + 1, j + 1, k + 1), id(i, j + 1, k + 1),
                ];
                let n: Vec<String> = n.iter().map(|v| v.to_string()).collect();
                s.push_str(&format!("{}, {}\n", e, n.join(", ")));
                e += 1;
            }
        }
    }
    s
}

fn close(a: [f64; 3], b: [f64; 3]) -> bool {
    (0..3).all(|k| (a[k] - b[k]).abs() < 1e-12)
}

#[test]
fn unit_cube() {
    let m = read("cube", CUBE);
    assert_eq!(m.nodes.len(), 8);
    assert_eq!(m.elements.len(), 1);
    assert_eq!(m.element_types, vec!["C3D8"]);
    let f = exterior_faces(&m);
    assert_eq!(f.len(), 6);
    let expect = [
        [0.0, 0.0, -1.0],
        [0.0, 0.0, 1.0],
        [0.0, -1.0, 0.0],
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [-1.0, 0.0, 0.0],
    ];
    for (i, face) in f.iter().enumerate() {
        assert_eq!(face.face as usize, i + 1);
        assert_eq!(face.element, 1);
        assert!((face.area - 1.0).abs() < 1e-12);
        assert!(close(face.normal, expect[i]), "face {} normal {:?}", i + 1, face.normal);
    }
    assert!(close(f[0].centroid, [0.5, 0.5, 0.0]));
    assert!(close(f[1].centroid, [0.5, 0.5, 1.0]));
}

#[test]
fn stacked_hexes_share_a_face() {
    let m = read("stack", &block_deck(1, 1, 2));
    assert_eq!(m.elements.len(), 2);
    let f = exterior_faces(&m);
    assert_eq!(f.len(), 10);
    assert!(!f.iter().any(|x| x.element == 1 && x.face == 2));
    assert!(!f.iter().any(|x| x.element == 2 && x.face == 1));
}

#[test]
fn tetra() {
    let m = read(
        "tet",
        "*NODE\n1, 0, 0, 0\n2, 2, 0, 0\n3, 0, 3, 0\n4, 0, 0, 6\n*ELEMENT, TYPE=C3D4\n7, 1, 2, 3, 4\n",
    );
    let f = exterior_faces(&m);
    assert_eq!(f.len(), 4);
    // Faces: z=0 (3), x=0 (9), y=0 (6) and the slanted face with area vector (18, 12, 6)/2.
    let slanted = 0.5 * (18.0f64 * 18.0 + 12.0 * 12.0 + 6.0 * 6.0).sqrt();
    let total: f64 = f.iter().map(|x| x.area).sum();
    assert!((total - (3.0 + 9.0 + 6.0 + slanted)).abs() < 1e-9, "total {total}");
    let cen = [0.5, 0.75, 1.5];
    for face in &f {
        let d = [face.centroid[0] - cen[0], face.centroid[1] - cen[1], face.centroid[2] - cen[2]];
        assert!(d[0] * face.normal[0] + d[1] * face.normal[1] + d[2] * face.normal[2] > 0.0);
    }
}

#[test]
fn wedge() {
    let m = read(
        "wedge",
        "*NODE\n1,0,0,0\n2,1,0,0\n3,0,1,0\n4,0,0,2\n5,1,0,2\n6,0,1,2\n*ELEMENT, TYPE=C3D6\n1,1,2,3,4,5,6\n",
    );
    let f = exterior_faces(&m);
    assert_eq!(f.len(), 5);
    assert_eq!(f.iter().filter(|x| x.corners.len() == 3).count(), 2);
    assert_eq!(f.iter().filter(|x| x.corners.len() == 4).count(), 3);
    assert!(close(f[0].normal, [0.0, 0.0, -1.0]));
    assert!(close(f[1].normal, [0.0, 0.0, 1.0]));
}

#[test]
fn c3d20_continuation_lines() {
    let mut s = String::from("*NODE\n");
    let corners = [
        [0., 0., 0.], [1., 0., 0.], [1., 1., 0.], [0., 1., 0.],
        [0., 0., 1.], [1., 0., 1.], [1., 1., 1.], [0., 1., 1.],
    ];
    for (i, c) in corners.iter().enumerate() {
        s.push_str(&format!("{}, {}, {}, {}\n", i + 1, c[0], c[1], c[2]));
    }
    // Mid-side nodes 9..20: short lines (z defaults to 0), position irrelevant here.
    for id in 9..=20 {
        s.push_str(&format!("{id}, 0.5, 0.5\n"));
    }
    s.push_str("*ELEMENT, TYPE=C3D20R, ELSET=E20\n1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10,\n11, 12, 13, 14, 15, 16,\n17, 18, 19, 20\n");
    let m = read("c3d20", &s);
    assert_eq!(m.nodes.len(), 20);
    assert_eq!(m.elements.len(), 1);
    assert_eq!(m.elements[0].nodes.len(), 20);
    assert_eq!(m.elements[0].elem_type, ElementType::C3D20R);
    assert_eq!(m.element_types[0], "C3D20R");
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);
    assert_eq!(m.nodes[8].z, 0.0);
    let f = exterior_faces(&m);
    assert_eq!(f.len(), 6);
    assert!(f.iter().all(|x| x.corners.len() == 4 && (x.area - 1.0).abs() < 1e-12));
}

#[test]
fn sets_and_surfaces() {
    let mut deck = block_deck(3, 1, 1);
    deck.push_str(
        "\
*NSET, NSET=Base, GENERATE
1, 4
*NSET, nset=other
8, 7,
*NSET, NSET=both
base, OTHER, 99, missing
*ELSET, ELSET=Left
1
*ELSET, ELSET=right
3,
*ELSET, ELSET=ends
left, RIGHT
*SURFACE, NAME=Top, TYPE=ELEMENT
ends, S2
2, S2
*SURFACE, NAME=nodesurf, TYPE=NODE
base
*NODE
100, 1.5D+2, -2.5d0
",
    );
    let m = read("sets", &deck);
    assert_eq!(m.sets["BASE"].node_ids, vec![1, 2, 3, 4]);
    assert_eq!(m.sets["OTHER"].node_ids, vec![8, 7]);
    assert_eq!(m.sets["BOTH"].node_ids, vec![1, 2, 3, 4, 8, 7, 99]);
    assert_eq!(m.sets["ENDS"].element_ids, vec![1, 3]);
    assert_eq!(m.sets["EALL"].element_ids, vec![1, 2, 3]);
    assert!(m.warnings.iter().any(|w| w.contains("missing")));
    assert!(m.warnings.iter().any(|w| w.contains("TYPE=NODE")));
    let n = &m.nodes[m.node_index[&100]];
    assert_eq!((n.x, n.y, n.z), (150.0, -2.5, 0.0));
    assert!(m.warnings.iter().all(|w| w.contains("deck.inp:")));

    let top = faces_of_surface(&m, "top").unwrap();
    let pairs: Vec<(u32, u8)> = top.iter().map(|f| (f.element, f.face)).collect();
    assert_eq!(pairs, vec![(1, 2), (3, 2), (2, 2)]);
    assert!(top.iter().all(|f| close(f.normal, [0.0, 0.0, 1.0]) && (f.area - 1.0).abs() < 1e-12));
    assert!(faces_of_surface(&m, "NODESURF").is_none());
}

#[test]
fn duplicate_node_warns() {
    let m = read("dup", "*NODE\n1,0,0,0\n1,1,1,1\n2,x,0,0\n");
    assert_eq!(m.nodes.len(), 1);
    assert_eq!(m.warnings.len(), 2);
}

#[test]
fn selection_helpers() {
    let m = read("sel", &block_deck(3, 3, 1));
    let f = exterior_faces(&m);
    assert_eq!(f.len(), 9 + 9 + 12);
    let top = by_normal(&f, [0.0, 0.0, 1.0], 1.0);
    assert_eq!(top.len(), 9);
    assert_eq!(on_plane(&m, &f, [0.0, 0.0, 0.0], [0.0, 0.0, 1.0], 1e-9).len(), 9);
    assert_eq!(on_plane(&m, &f, [0.0, 0.0, 1.0], [0.0, 0.0, 2.0], 1e-9).len(), 9);
    // Centroid box x <= 1: top and bottom faces of column 0 (2), the x=0 face column (3)
    // and the y=0 / y=3 side faces of column 0 (2).
    let b = in_box(&f, [0.0, 0.0, 0.0], [1.0, 3.0, 1.0]);
    let expect = f.iter().filter(|x| x.centroid[0] <= 1.0).count();
    assert_eq!(b.len(), expect);
    assert_eq!(b.len(), 2 * 3 + 3 + 2);
    let patch = connected_patch(&f, top[0], 10.0);
    assert_eq!(patch.len(), 9);
    assert!(patch.iter().all(|i| top.contains(i)));
    assert_eq!(connected_patch(&f, top[0], 100.0).len(), f.len());
    assert!(connected_patch(&f, 999, 10.0).is_empty());
}

#[test]
fn writers_round_trip() {
    let base = block_deck(3, 3, 1);
    let m = read("rt1", &base);
    let f = exterior_faces(&m);
    let top: Vec<&Face> = by_normal(&f, [0.0, 0.0, 1.0], 1.0).into_iter().map(|i| &f[i]).collect();
    let card = write_surface_card("topskin", &top);
    assert!(card.starts_with("*SURFACE, NAME=topskin, TYPE=ELEMENT\n"));
    let nset = write_nset_card("N", &[5, 3, 3, 1, 2, 4, 6, 7, 8, 9, 10, 11, 12]);
    assert_eq!(nset, "*NSET, NSET=N\n1, 2, 3, 4, 5, 6, 7, 8, 9, 10\n11, 12\n");
    let m2 = read("rt2", &format!("{base}{card}{nset}"));
    let back = faces_of_surface(&m2, "TOPSKIN").unwrap();
    let mut a: Vec<(u32, u8)> = top.iter().map(|f| (f.element, f.face)).collect();
    let mut b: Vec<(u32, u8)> = back.iter().map(|f| (f.element, f.face)).collect();
    a.sort();
    b.sort();
    assert_eq!(a, b);
    assert_eq!(m2.sets["N"].node_ids.len(), 12);
}

#[test]
fn real_rotor_mesh() {
    let p = Path::new(
        "/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor/lavffcyc.inp",
    );
    if !p.exists() {
        eprintln!("skipped: {} not found", p.display());
        return;
    }
    let t = std::time::Instant::now();
    let m = read_inp_mesh(p).unwrap();
    let t_read = t.elapsed();
    let f = exterior_faces(&m);
    let t_all = t.elapsed();
    let mut hist = std::collections::BTreeMap::new();
    for l in &m.element_types {
        *hist.entry(l.clone()).or_insert(0usize) += 1;
    }
    let (mut lo, mut hi) = ([f64::MAX; 3], [f64::MIN; 3]);
    for n in &m.nodes {
        for (k, v) in [n.x, n.y, n.z].into_iter().enumerate() {
            lo[k] = lo[k].min(v);
            hi[k] = hi[k].max(v);
        }
    }
    let area: f64 = f.iter().map(|x| x.area).sum();
    println!("nodes {} elements {} warnings {}", m.nodes.len(), m.elements.len(), m.warnings.len());
    println!("element types {hist:?}");
    println!("exterior faces {} total area {area:.3}", f.len());
    println!("bbox min {lo:?} max {hi:?}");
    println!("sets {} surfaces {}", m.sets.len(), m.surfaces.len());
    println!("read {t_read:?}, read+faces {t_all:?}");
    // The deck (all.msh) defines 48877 nodes with ids 1..=48877; the .frd of the run lists
    // 48909 because ccx adds 32 nodes of its own.
    assert_eq!(m.nodes.len(), 48877);
    assert_eq!(m.elements.len(), 10816);
    assert!(!f.is_empty() && f.iter().all(|x| x.area > 0.0));
}
