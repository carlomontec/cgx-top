use cgx_core::*;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn load(deck: &str) -> (InpMesh, InpScan) {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir: PathBuf = std::env::temp_dir().join(format!("cgx_mesh_contact_{}_{}", std::process::id(), n));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join("deck.inp");
    fs::write(&p, deck).unwrap();
    let mesh = read_inp_mesh(&p).unwrap();
    let scan = scan_cards(&p, &ScanLimits::default()).unwrap();
    let _ = fs::remove_dir_all(&dir);
    (mesh, scan)
}

/// Block of `n[0] x n[1] x n[2]` C3D8 of edge `h` at `origin`, ids from `first`
/// (nodes and elements), ELSET `name`. `jitter` moves each node by a deterministic
/// pseudo-random offset of at most `jitter` per axis.
fn block(n: [usize; 3], h: f64, origin: [f64; 3], first: usize, name: &str, jitter: Option<(f64, &mut u64)>) -> String {
    let id = |i: usize, j: usize, k: usize| first + i + (n[0] + 1) * (j + (n[1] + 1) * k);
    let mut s = String::from("*NODE\n");
    let mut jit = jitter;
    for k in 0..=n[2] {
        for j in 0..=n[1] {
            for i in 0..=n[0] {
                let mut p = [origin[0] + i as f64 * h, origin[1] + j as f64 * h, origin[2] + k as f64 * h];
                if let Some((amp, seed)) = jit.as_mut() {
                    for c in &mut p {
                        *c += *amp * (2.0 * rand01(seed) - 1.0);
                    }
                }
                s += &format!("{}, {:.12}, {:.12}, {:.12}\n", id(i, j, k), p[0], p[1], p[2]);
            }
        }
    }
    s += &format!("*ELEMENT, TYPE=C3D8, ELSET={name}\n");
    let mut e = first;
    for k in 0..n[2] {
        for j in 0..n[1] {
            for i in 0..n[0] {
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

fn rand01(seed: &mut u64) -> f64 {
    *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    ((*seed >> 11) as f64) / ((1u64 << 53) as f64)
}

fn opts(gap_max: f64) -> ContactSearch {
    ContactSearch { gap_max, max_angle_deg: 30.0, groups: Vec::new() }
}

fn run(deck: &str, partition: Partition, o: &ContactSearch) -> (InpMesh, Vec<ContactGroup>, ContactReport) {
    let (mesh, scan) = load(deck);
    let groups = partition_groups(&mesh, &scan, partition);
    let rep = find_contact_pairs(&mesh, &groups, o);
    (mesh, groups, rep)
}

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

#[test]
fn gap_of_0_05_between_two_blocks() {
    // Lower block 2x2x1 (z in [0,1]), upper block 2x2x1 at z = 1.05.
    let deck = format!("{}{}", block([2, 2, 1], 1.0, [0.0; 3], 1, "LOW", None), block([2, 2, 1], 1.0, [0.0, 0.0, 1.05], 1000, "UP", None));
    let (_, groups, rep) = run(&deck, Partition::Component, &opts(0.1));
    assert_eq!(groups.len(), 2);
    assert_eq!((groups[0].name.as_str(), groups[1].name.as_str()), ("PART_1", "PART_2"));
    assert_eq!(rep.pairs.len(), 1);
    let p = &rep.pairs[0];
    assert_eq!((p.faces_a.len(), p.faces_b.len(), p.pairs.len()), (4, 4, 4));
    assert!(close(p.gap_min, 0.05, 1e-9) && close(p.gap_max, 0.05, 1e-9) && close(p.gap_mean, 0.05, 1e-9));
    assert!(close(p.area_a, 4.0, 1e-9) && close(p.area_b, 4.0, 1e-9) && close(p.overlap_area, 4.0, 1e-9));
    // The faces are the top of the lower block (S2, +z) and the bottom of the upper (S1, -z).
    let side = |g: usize| if groups[g].elements.contains(&1) { "LOW" } else { "UP" };
    for (list, g) in [(&p.faces_a, p.side_a), (&p.faces_b, p.side_b)] {
        let want = if side(g) == "LOW" { 2 } else { 1 };
        assert!(list.iter().all(|&i| rep.faces[i].face == want), "{:?}", list.iter().map(|&i| &rep.faces[i]).collect::<Vec<_>>());
    }
    // Too small a gap finds nothing; the nearest-gap search reports 0.05.
    let o = opts(0.01);
    let (mesh, scan) = load(&deck);
    let groups = partition_groups(&mesh, &scan, Partition::Component);
    assert!(find_contact_pairs(&mesh, &groups, &o).pairs.is_empty());
    let (a, b, g) = nearest_opposing_gap(&mesh, &groups, &o).unwrap();
    assert_eq!((a, b), (0, 1));
    assert!(close(g, 0.05, 1e-9));
}

#[test]
fn overlapping_blocks_give_negative_gap() {
    let deck = format!("{}{}", block([2, 2, 1], 1.0, [0.0; 3], 1, "LOW", None), block([2, 2, 1], 1.0, [0.0, 0.0, 0.97], 1000, "UP", None));
    let (_, _, rep) = run(&deck, Partition::Component, &opts(0.1));
    assert_eq!(rep.pairs.len(), 1);
    let p = &rep.pairs[0];
    assert!(close(p.gap_min, -0.03, 1e-9) && close(p.gap_max, -0.03, 1e-9));
    assert_eq!(p.pairs.len(), 4);
}

#[test]
fn shared_nodes_make_one_component_and_no_pair() {
    // The second block starts on the first block's top nodes: reuse ids by offsetting a block
    // of 2 layers and splitting it into two element sets.
    let deck = block([2, 2, 2], 1.0, [0.0; 3], 1, "ALL", None) + "*ELSET, ELSET=LOWER\n1, 2, 3, 4\n*ELSET, ELSET=UPPER\n5, 6, 7, 8\n";
    let (_, groups, rep) = run(&deck, Partition::Component, &opts(0.1));
    assert_eq!(groups.len(), 1);
    assert!(rep.pairs.is_empty());
    assert_eq!((rep.near_checks, rep.faces.len()), (0, 0), "a single group takes no part");
    // By element set the interface is interior (shared nodes), so there is still no pair; the
    // set ALL of every element is ignored.
    let (_, groups, rep) = run(&deck, Partition::Elset, &opts(0.1));
    assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["LOWER", "UPPER"]);
    assert!(rep.pairs.is_empty());
}

#[test]
fn perpendicular_faces_are_excluded() {
    // A: [0,2]x[0,2]x[0,1]. B: [2.05,3.05]x[0,2]x[0.95,2.95] stands beside A. A's top
    // faces (+z, centroids at x = 1.5) lie 0.55 in front of B's -x side (x = 2.05) and
    // project inside it, but at 90 degrees. The parallel pairs (A +x / B -x, A top / B
    // bottom) do not overlap, so their projections fall outside.
    let deck = format!("{}{}", block([2, 2, 1], 1.0, [0.0; 3], 1, "A", None), block([1, 2, 2], 1.0, [2.05, 0.0, 0.95], 1000, "B", None));
    let (_, _, rep) = run(&deck, Partition::Component, &opts(0.6));
    assert!(rep.pairs.is_empty(), "{:?}", rep.pairs);
    assert!(rep.near_checks > rep.opposing_checks, "near {} opposing {}", rep.near_checks, rep.opposing_checks);
    // Widening the angle past 90 degrees admits the perpendicular pairs: the normal test is
    // what excluded them.
    let o = ContactSearch { max_angle_deg: 95.0, ..opts(0.6) };
    let (_, _, rep) = run(&deck, Partition::Component, &o);
    assert_eq!(rep.pairs.len(), 1);
    assert!(rep.pairs[0].pairs.iter().all(|p| (rep.faces[p.a].normal[2] - rep.faces[p.b].normal[2]).abs() > 0.5 || (rep.faces[p.a].normal[0] - rep.faces[p.b].normal[0]).abs() > 0.5));
}

#[test]
fn partial_overlap_pairs_only_the_overlap() {
    // Lower 4x4x1, upper 4x4x1 shifted by 2 in x and lifted by a 0.02 gap: overlap 2x4 = 8.
    let deck = format!("{}{}", block([4, 4, 1], 1.0, [0.0; 3], 1, "LOW", None), block([4, 4, 1], 1.0, [2.0, 0.0, 1.02], 1000, "UP", None));
    let (_, groups, rep) = run(&deck, Partition::Component, &opts(0.05));
    assert_eq!(rep.pairs.len(), 1);
    let p = &rep.pairs[0];
    assert_eq!((p.faces_a.len(), p.faces_b.len()), (8, 8));
    assert!(close(p.overlap_area, 8.0, 1e-9), "hand value 2 x 4 = 8, got {}", p.overlap_area);
    for &i in p.faces_a.iter().chain(&p.faces_b) {
        let c = rep.faces[i].centroid;
        assert!(c[0] > 2.0 && c[0] < 4.0, "face outside the overlap: {c:?}");
    }
    let _ = groups;
}

#[test]
fn finer_side_is_suggested_as_slave() {
    // Coarse lower block (edge 1), fine upper block (edge 0.5) over the same 2x2 footprint.
    let deck = format!("{}{}", block([2, 2, 1], 1.0, [0.0; 3], 1, "COARSE", None), block([4, 4, 1], 0.5, [0.0, 0.0, 1.01], 1000, "FINE", None));
    let (mesh, scan) = load(&deck);
    let groups = partition_groups(&mesh, &scan, Partition::Elset);
    assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["COARSE", "FINE"]);
    let rep = find_contact_pairs(&mesh, &groups, &opts(0.05));
    let p = &rep.pairs[0];
    assert_eq!((p.side_a, p.side_b), (0, 1));
    assert!(!p.slave_is_a(), "the fine side B is the slave");
    assert_eq!((p.faces_a.len(), p.faces_b.len()), (4, 16));
    assert!(close(p.overlap_area, 4.0, 1e-9) && close(p.median_area_b, 0.25, 1e-12));
    // Every fine face projects into a coarse face; coarse centroids land on fine corners.
    assert!(p.pairs.len() >= 16);
}

#[test]
fn groups_filter_and_material_partition() {
    let deck = format!(
        "{}{}{}*MATERIAL, NAME=M1\n*MATERIAL, NAME=M2\n*SOLID SECTION, ELSET=A, MATERIAL=M1\n*SOLID SECTION, ELSET=B, MATERIAL=M2\n*SOLID SECTION, ELSET=C, MATERIAL=M1\n",
        block([1, 1, 1], 1.0, [0.0; 3], 1, "A", None),
        block([1, 1, 1], 1.0, [0.0, 0.0, 1.05], 100, "B", None),
        block([1, 1, 1], 1.0, [0.0, 0.0, 2.10], 200, "C", None)
    );
    let (mesh, scan) = load(&deck);
    let groups = partition_groups(&mesh, &scan, Partition::Material);
    assert_eq!(groups.iter().map(|g| (g.name.as_str(), g.elements.len())).collect::<Vec<_>>(), [("M1", 2), ("M2", 1)]);
    let rep = find_contact_pairs(&mesh, &groups, &opts(0.1));
    assert_eq!(rep.pairs.len(), 1);
    assert_eq!(rep.pairs[0].pairs.len(), 2, "M2 touches both M1 blocks");

    let groups = partition_groups(&mesh, &scan, Partition::Component);
    assert_eq!(groups.len(), 3);
    let all = find_contact_pairs(&mesh, &groups, &opts(0.1));
    assert_eq!(all.pairs.len(), 2);
    let a = groups.iter().position(|g| g.elements == [1]).unwrap();
    let b = groups.iter().position(|g| g.elements == [100]).unwrap();
    let c = groups.iter().position(|g| g.elements == [200]).unwrap();
    let only_a = find_contact_pairs(&mesh, &groups, &ContactSearch { groups: vec![a], ..opts(0.1) });
    assert_eq!(only_a.pairs.len(), 1);
    assert_eq!((only_a.pairs[0].side_a.min(only_a.pairs[0].side_b), only_a.pairs[0].side_a.max(only_a.pairs[0].side_b)), (a.min(b), a.max(b)));
    let a_c = find_contact_pairs(&mesh, &groups, &ContactSearch { groups: vec![a, c], ..opts(0.1) });
    assert!(a_c.pairs.is_empty(), "A and C do not face each other");
}

#[test]
fn grid_equals_brute_force_on_random_blocks() {
    let mut seed = 0x5eed_u64;
    for case in 0..6 {
        let mut deck = String::new();
        let mut first = 1;
        for b in 0..5 {
            let n = [1 + (rand01(&mut seed) * 3.0) as usize, 1 + (rand01(&mut seed) * 3.0) as usize, 1 + (rand01(&mut seed) * 2.0) as usize];
            let h = 0.4 + rand01(&mut seed) * 0.6;
            let o = [rand01(&mut seed) * 3.0, rand01(&mut seed) * 3.0, rand01(&mut seed) * 3.0];
            deck += &block(n, h, o, first, &format!("B{b}"), Some((0.05, &mut seed)));
            first += 1000;
        }
        let (mesh, scan) = load(&deck);
        let groups = partition_groups(&mesh, &scan, Partition::Component);
        for gap in [0.05, 0.3, 1.0] {
            let o = ContactSearch { gap_max: gap, max_angle_deg: 40.0, groups: Vec::new() };
            let rep = find_contact_pairs(&mesh, &groups, &o);
            let mut grid: Vec<FacePairRow> = rep
                .pairs
                .iter()
                .flat_map(|gp| gp.pairs.iter())
                .map(|p| {
                    let (a, b) = (&rep.faces[p.a], &rep.faces[p.b]);
                    ((a.element, a.face), (b.element, b.face), p.gap)
                })
                .collect();
            grid.sort_by_key(|x| (x.0, x.1));
            let brute = contact_face_pairs_brute_force(&mesh, &groups, &o);
            assert_eq!(grid.len(), brute.len(), "case {case} gap {gap}");
            for (g, b) in grid.iter().zip(&brute) {
                assert_eq!((g.0, g.1), (b.0, b.1), "case {case} gap {gap}");
                assert!(close(g.2, b.2, 1e-12));
            }
            assert_eq!(rep.face_pairs, brute.len());
            if gap == 1.0 {
                assert!(!brute.is_empty(), "case {case}: the random layout should produce some pairs at gap 1");
            }
        }
    }
}

// ---------- real decks ----------

const CCX_TEST: &str = "/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/test";

/// (slave, master) names of the first data line of each *CONTACT PAIR card.
fn deck_contact_pairs(scan: &InpScan) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for c in scan.cards.iter().filter(|c| c.keyword == "CONTACT PAIR") {
        for line in read_card_data(c, c.data_lines).unwrap_or_default() {
            let f: Vec<String> = line.split(',').map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty()).collect();
            if f.len() >= 2 {
                out.push((f[0].clone(), f[1].clone()));
            }
        }
    }
    out
}

/// Median over slave faces of the distance from the slave centroid to the plane of the master
/// face with the nearest centroid: the clearance the deck builds in.
fn deck_clearance(slave: &[Face], master: &[Face]) -> f64 {
    let mut d: Vec<f64> = slave
        .iter()
        .filter_map(|s| {
            master
                .iter()
                .min_by(|a, b| dist2(a.centroid, s.centroid).total_cmp(&dist2(b.centroid, s.centroid)))
                .map(|m| (0..3).map(|k| (s.centroid[k] - m.centroid[k]) * m.normal[k]).sum::<f64>().abs())
        })
        .collect();
    d.sort_by(f64::total_cmp);
    d.get(d.len() / 2).copied().unwrap_or(0.0)
}

fn dist2(a: [f64; 3], b: [f64; 3]) -> f64 {
    (0..3).map(|k| (a[k] - b[k]).powi(2)).sum()
}

#[test]
#[allow(clippy::print_stdout)]
fn real_decks_cover_their_own_contact_surfaces() {
    // gap_max comes from the geometry: 1.25 x the clearance between the deck's own slave and
    // master surfaces (`deck_clearance`) plus 0.05 x the median contact face size, so touching
    // surfaces (clearance 0) still get a small positive search band.
    //
    // contact2: two C3D20 cantilevers 0.5 apart (thermal expansion closes the gap).
    // punch1: curved C3D20 punch on a cube; the master is the whole cube top (9 faces), the
    //   punch covers 2 of them, so master coverage is low by design of the deck.
    // potied: two halves of a beam touching (tied contact).
    // beamcontact: three parts, two contact pairs.
    // Skipped: cycliccontact (the partner surface is the rotated neighbour sector of
    // *CYCLIC SYMMETRY MODEL, CONTACT, which does not exist in the deck geometry); load1
    // (the slave is an S8 shell face; only solid faces are supported); node-surface decks
    // (TYPE=NODE slaves cannot be compared face by face).
    //
    // Assertion, decided from the data: every deck reaches 100 % slave coverage, so require
    // at least 90 % of the slave faces; extra faces (paired faces outside the deck's
    // surfaces) stay at most 10 % of the deck faces (2 of 30 on contact2: faces 34 S4 and
    // 36 S4, on the same side as the slave faces 38 S4, 40 S4, ..., that face the master
    // within the gap but were left out of the deck's slave surface). Master coverage is reported, not asserted, because masters
    // are often defined larger than the contact footprint (punch1).
    let decks: Vec<(&str, Partition)> =
        ["contact2", "punch1", "potied", "beamcontact"].into_iter().map(|n| (n, Partition::Component)).collect();
    let mut failed: Vec<String> = Vec::new();
    let base = Path::new(CCX_TEST);
    if !base.exists() {
        eprintln!("ccx test decks not present, skipping");
        return;
    }
    println!("| deck | partition | clearance | gap_max | groups | slave covered | master covered | deck faces covered | extra faces | group pairs | ms |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|");
    for (name, partition) in decks {
        let path = base.join(format!("{name}.inp"));
        if !path.exists() {
            continue;
        }
        let mesh = read_inp_mesh(&path).unwrap();
        let scan = scan_cards(&path, &ScanLimits::default()).unwrap();
        let mut deck_faces: BTreeSet<FaceKey> = BTreeSet::new();
        let mut slave: BTreeSet<FaceKey> = BTreeSet::new();
        let mut master: BTreeSet<FaceKey> = BTreeSet::new();
        let mut sizes = Vec::new();
        let mut clearance: f64 = 0.0;
        for (s, m) in deck_contact_pairs(&scan) {
            let get = |n: &str| faces_of_surface(&mesh, n).unwrap_or_else(|| panic!("{name}: surface {n} is not an element-face surface"));
            let (fs, fm) = (get(&s), get(&m));
            clearance = clearance.max(deck_clearance(&fs, &fm));
            slave.extend(fs.iter().map(|x| (x.element, x.face)));
            master.extend(fm.iter().map(|x| (x.element, x.face)));
            for x in fs.iter().chain(&fm) {
                sizes.push(x.area.sqrt());
                deck_faces.insert((x.element, x.face));
            }
        }
        sizes.sort_by(f64::total_cmp);
        let gap = 1.25 * clearance + 0.05 * sizes[sizes.len() / 2];
        let groups = partition_groups(&mesh, &scan, partition);
        let o = ContactSearch { gap_max: gap, max_angle_deg: 30.0, groups: Vec::new() };
        let t = std::time::Instant::now();
        let rep = find_contact_pairs(&mesh, &groups, &o);
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let found: BTreeSet<(u32, u8)> = rep
            .pairs
            .iter()
            .flat_map(|p| p.faces_a.iter().chain(&p.faces_b))
            .map(|&i| (rep.faces[i].element, rep.faces[i].face))
            .collect();
        let frac = |set: &BTreeSet<(u32, u8)>| format!("{}/{}", set.intersection(&found).count(), set.len());
        let covered = deck_faces.intersection(&found).count();
        let extra = found.difference(&deck_faces).count();
        let coverage = covered as f64 / deck_faces.len() as f64;
        println!(
            "| {name} | {partition:?} | {clearance:.4} | {gap:.4} | {} | {} | {} | {covered}/{} ({:.0} %) | {extra} | {} | {ms:.1} |",
            groups.len(),
            frac(&slave),
            frac(&master),
            deck_faces.len(),
            coverage * 100.0,
            rep.pairs.len()
        );
        let missed: Vec<_> = deck_faces.difference(&found).collect();
        if !missed.is_empty() {
            println!("  {name}: deck faces not paired {missed:?}");
        }
        let extras: Vec<_> = found.difference(&deck_faces).collect();
        if !extras.is_empty() {
            println!("  {name}: extra faces {extras:?}");
        }
        if rep.pairs.is_empty() {
            println!("  {name}: nearest opposing gap {:?}", nearest_opposing_gap(&mesh, &groups, &o));
        }
        let slave_cov = slave.intersection(&found).count() as f64 / slave.len() as f64;
        if slave_cov < 0.9 || extra as f64 > 0.1 * deck_faces.len() as f64 {
            failed.push(format!("{name}: slave coverage {slave_cov}, extra {extra}"));
        }
    }
    assert!(failed.is_empty(), "{failed:?}");
}

#[test]
#[allow(clippy::print_stdout)]
fn rotor_single_part_is_fast_and_finds_nothing() {
    let deck = Path::new("/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor/lavffcyc.inp");
    if !deck.exists() {
        eprintln!("rotor deck not present, skipping");
        return;
    }
    let mesh = read_inp_mesh(deck).unwrap();
    let scan = scan_cards(deck, &ScanLimits::default()).unwrap();
    for partition in [Partition::Component, Partition::Elset, Partition::Material] {
        let t = std::time::Instant::now();
        let groups = partition_groups(&mesh, &scan, partition);
        let rep = find_contact_pairs(&mesh, &groups, &ContactSearch { gap_max: 0.1, max_angle_deg: 30.0, groups: Vec::new() });
        let ms = t.elapsed().as_secs_f64() * 1e3;
        println!(
            "rotor {partition:?}: {} elements, {} groups, {} faces considered, {} near checks, {} group pairs, {ms:.0} ms",
            mesh.elements.len(),
            groups.len(),
            rep.faces.len(),
            rep.near_checks,
            rep.pairs.len()
        );
        if partition == Partition::Component {
            assert_eq!(groups.len(), 1);
            assert!(rep.pairs.is_empty());
        }
    }
    // A single group takes no part, so the above never reaches the grid. To time the grid on
    // every exterior face, split the elements into 8 synthetic groups by id order.
    let mut ids: Vec<u32> = mesh.elements.iter().map(|e| e.id).collect();
    ids.sort_unstable();
    let chunk = ids.len().div_ceil(8);
    let groups: Vec<ContactGroup> = ids
        .chunks(chunk)
        .enumerate()
        .map(|(i, c)| ContactGroup { name: format!("G{i}"), elements: c.to_vec(), bbox: ([0.0; 3], [0.0; 3]) })
        .collect();
    let t = std::time::Instant::now();
    let rep = find_contact_pairs(&mesh, &groups, &ContactSearch { gap_max: 0.1, max_angle_deg: 30.0, groups: Vec::new() });
    let ms = t.elapsed().as_secs_f64() * 1e3;
    println!(
        "rotor split into 8 id-order groups: {} faces through the grid (cell {:.3}), {} near checks, {} opposing, {} face pairs, {} group pairs, {ms:.0} ms",
        rep.faces.len(),
        rep.cell_size,
        rep.near_checks,
        rep.opposing_checks,
        rep.face_pairs,
        rep.pairs.len()
    );
    assert!(rep.faces.len() > 1000);
    assert!(rep.near_checks < 100 * rep.faces.len(), "near-linear: {} checks for {} faces", rep.near_checks, rep.faces.len());
}
