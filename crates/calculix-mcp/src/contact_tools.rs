//! Preprocessing tool `mesh_contact_pairs`: contact candidates between parts of a mesh.

use crate::error::{nearest, ToolError};
use crate::frd_tools::Table;
use crate::paths::Roots;
use cgx_core::{
    exterior_faces, find_contact_pairs, nearest_opposing_gap, partition_groups, read_inp_mesh, scan_cards, write_surface_card,
    ContactGroup, ContactReport, ContactSearch, Face, InpMesh, InpScan, Partition, ScanLimits,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Most group pairs listed in the response (all are counted in `pairs_total`).
const MAX_PAIRS_LISTED: usize = 20;
/// Most groups listed in the response.
const MAX_GROUPS_LISTED: usize = 50;
/// Most group pairs a single save may write (two files each).
const MAX_PAIRS_SAVED: usize = 20;

#[derive(Debug, Clone, Copy, Default, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PartitionKind {
    /// Connected parts (elements sharing nodes), named PART_1, PART_2, ... largest first
    #[default]
    Component,
    /// Each element in the first element set (deck order) that contains it; sets of every element are ignored
    Elset,
    /// By *SOLID SECTION material
    Material,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ContactSaveSpec {
    /// Letters, digits, `_` and `-`; starts with a letter. Files are <prefix>_<A>_<B>_slave.sur and _master.sur in the deck directory
    pub prefix: String,
    /// Replace existing files (default false)
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MeshContactIn {
    /// Path to the root .inp deck
    pub path: String,
    /// How faces are grouped into parts (default component)
    pub partition: Option<PartitionKind>,
    /// Group names to consider (case-insensitive). One name: pairs involving it. Several: pairs between them. Default: all
    pub groups: Option<Vec<String>>,
    /// Largest signed gap to report, deck length units (negative gaps down to -gap_max are penetrations). Default: 0.1 x the median exterior face size
    pub gap_max: Option<f64>,
    /// Largest angle between one face normal and the reversed other normal (default 30)
    pub max_angle_deg: Option<f64>,
    /// Write the two surfaces of every pair found as include files
    pub save: Option<ContactSaveSpec>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BBox {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct GroupOut {
    pub name: String,
    pub elements: usize,
    pub exterior_faces: usize,
    /// Box of the group's nodes
    pub bounding_box: BBox,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ContactPairOut {
    pub side_a: String,
    pub side_b: String,
    /// Paired faces on each side
    pub faces_a: usize,
    pub faces_b: usize,
    /// Area of the paired faces on each side
    pub area_a: f64,
    pub area_b: f64,
    /// Signed gaps over the face pairs (negative = penetration)
    pub gap_min: f64,
    pub gap_mean: f64,
    pub gap_max: f64,
    /// The smaller of area_a and area_b (each face counted once)
    pub overlap_area_estimate: f64,
    /// The finer side (smaller median face area). A suggestion: CalculiX wants the slave at least as fine as the master
    pub suggested_slave: String,
    pub suggested_master: String,
    /// Columns: element_a, face_a, element_b, face_b, gap, area_a, area_b. Up to 10 face pairs, smallest gap first
    pub sample: Table,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ContactSavedOut {
    /// Files written, relative to the root
    pub files: Vec<String>,
    /// Lines to add to the deck (model definition, before the first *STEP)
    pub includes: Vec<String>,
    /// TEMPLATE TO REVIEW, not written anywhere: surface interaction and contact pair cards using the saved surfaces (slave first, then master)
    pub contact_card_template: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MeshContactOut {
    pub partition: String,
    /// Gap limit used, deck units
    pub gap_max: f64,
    pub max_angle_deg: f64,
    pub groups_total: usize,
    /// Up to 50 groups in partition order
    pub groups: Vec<GroupOut>,
    /// Columns: stage, count. Exterior faces considered, face tests proposed by the spatial grid, tests with opposing normals, face pairs within the gap, group pairs
    pub stages: Table,
    pub pairs_total: usize,
    /// Up to 20 group pairs, largest overlap first
    pub pairs: Vec<ContactPairOut>,
    pub saved: Option<ContactSavedOut>,
    /// Set when nothing was found or nothing could be saved
    pub hint: Option<String>,
    pub notes: Vec<String>,
}

fn read_deck(roots: &Roots, raw: &str) -> Result<(PathBuf, InpMesh, InpScan), ToolError> {
    let path = roots.existing(raw, &["inp"])?;
    let deck_err = |e: cgx_core::FrdError| ToolError::new("DECK_INVALID", format!("{}: {}", roots.display(&path), e));
    let mesh = read_inp_mesh(&path).map_err(deck_err)?;
    let scan = scan_cards(&path, &ScanLimits::default()).map_err(deck_err)?;
    if mesh.nodes.is_empty() || mesh.elements.is_empty() {
        return Err(ToolError::new("DECK_INVALID", "the deck has no nodes or no elements")
            .hint("Call inp_inspect to see which files the deck includes."));
    }
    Ok((path, mesh, scan))
}

/// `^[A-Za-z][A-Za-z0-9_-]{0,79}$`, as in mesh_select.
fn valid_name(name: &str, what: &str) -> Result<(), ToolError> {
    let ok = !name.is_empty()
        && name.len() <= 80
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(ToolError::invalid(format!("{what} '{name}' is not a valid name"))
            .hint("Names start with a letter and have at most 80 letters, digits, '_' or '-'. Use a shorter save.prefix, or the groups filter to skip groups with unusual names."))
    }
}

fn median_face_size(faces: &[Face]) -> f64 {
    let mut s: Vec<f64> = faces.iter().filter(|f| f.area > 0.0).map(|f| f.area.sqrt()).collect();
    s.sort_by(f64::total_cmp);
    s.get(s.len() / 2).copied().unwrap_or(0.0)
}

fn partition_name(p: PartitionKind) -> (&'static str, Partition) {
    match p {
        PartitionKind::Component => ("component", Partition::Component),
        PartitionKind::Elset => ("elset", Partition::Elset),
        PartitionKind::Material => ("material", Partition::Material),
    }
}

pub fn contact_pairs(roots: &Roots, p: MeshContactIn) -> Result<MeshContactOut, ToolError> {
    if let Some(g) = p.gap_max {
        if !(g.is_finite() && g > 0.0) {
            return Err(ToolError::invalid("gap_max must be a positive number"));
        }
    }
    let max_angle = p.max_angle_deg.unwrap_or(30.0);
    if !(max_angle.is_finite() && (0.0..=180.0).contains(&max_angle)) {
        return Err(ToolError::invalid("max_angle_deg must be between 0 and 180"));
    }
    if let Some(s) = &p.save {
        valid_name(&s.prefix, "save.prefix")?;
        valid_name(&format!("{}_SI", s.prefix), "interaction name")?;
    }
    let (deck, mesh, scan) = read_deck(roots, &p.path)?;
    let (pname, partition) = partition_name(p.partition.unwrap_or_default());
    let groups = partition_groups(&mesh, &scan, partition);

    let mut sel: Vec<usize> = Vec::new();
    for name in p.groups.iter().flatten() {
        let i = groups.iter().position(|g| g.name.eq_ignore_ascii_case(name.trim())).ok_or_else(|| {
            ToolError::new("UNKNOWN_GROUP", format!("no group named '{name}' with partition={pname}"))
                .valid(nearest(name, groups.iter().map(|g| g.name.clone())))
                .hint("Call without `groups` to list the group names, or change `partition`.")
        })?;
        if !sel.contains(&i) {
            sel.push(i);
        }
    }

    let gap_max = match p.gap_max {
        Some(g) => g,
        None => {
            let m = median_face_size(&exterior_faces(&mesh));
            if m > 0.0 { 0.1 * m } else { return Err(ToolError::new("DECK_INVALID", "the deck has no solid exterior faces")) }
        }
    };
    let search = ContactSearch { gap_max, max_angle_deg: max_angle, groups: sel.clone() };
    let rep = find_contact_pairs(&mesh, &groups, &search);

    let group_out: Vec<GroupOut> = groups
        .iter()
        .zip(&rep.faces_per_group)
        .take(MAX_GROUPS_LISTED)
        .map(|(g, &nf)| GroupOut { name: g.name.clone(), elements: g.elements.len(), exterior_faces: nf, bounding_box: BBox { min: g.bbox.0, max: g.bbox.1 } })
        .collect();
    let stages = Table {
        columns: ["stage", "count"].map(String::from).to_vec(),
        rows: [
            ("faces_considered", rep.faces.len()),
            ("near_checks", rep.near_checks),
            ("opposing_normals", rep.opposing_checks),
            ("face_pairs_within_gap", rep.face_pairs),
            ("group_pairs", rep.pairs.len()),
        ]
        .iter()
        .map(|(n, c)| vec![json!(n), json!(c)])
        .collect(),
    };
    let pairs: Vec<ContactPairOut> = rep.pairs.iter().take(MAX_PAIRS_LISTED).map(|gp| pair_out(&groups, &rep, gp)).collect();

    let mut hint = empty_hint(&mesh, &groups, &rep, &search, pname);
    let saved = match &p.save {
        Some(_) if rep.pairs.is_empty() => {
            hint = Some(format!("{} Nothing was saved.", hint.unwrap_or_default()).trim().to_string());
            None
        }
        Some(spec) => Some(save_pairs(roots, &deck, &scan, spec, &groups, &rep)?),
        None => None,
    };

    Ok(MeshContactOut {
        partition: pname.into(),
        gap_max,
        max_angle_deg: max_angle,
        groups_total: groups.len(),
        groups: group_out,
        stages,
        pairs_total: rep.pairs.len(),
        pairs,
        saved,
        hint,
        notes: vec![
            "Gaps are signed along the partner face normal: positive = clearance, negative = penetration. Face pairs: one centroid within gap_max of the other face's plane, projecting inside that face (10 % edge tolerance), with opposing normals.".into(),
            "suggested_slave is the finer side (smaller median face area). CalculiX manual: the mesh on the dependent (slave) side should be at least as fine as on the independent (master) side, preferably finer. It is a suggestion; review it.".into(),
            "Faces between groups that share nodes are interior and never pair; use partition=elset or material to split a single connected part only where the groups touch through a gap.".into(),
        ],
    })
}

fn pair_out(groups: &[ContactGroup], rep: &ContactReport, gp: &cgx_core::GroupPair) -> ContactPairOut {
    let (a, b) = (groups[gp.side_a].name.clone(), groups[gp.side_b].name.clone());
    let (slave, master) = if gp.slave_is_a() { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
    let sample = Table {
        columns: ["element_a", "face_a", "element_b", "face_b", "gap", "area_a", "area_b"].map(String::from).to_vec(),
        rows: gp
            .pairs
            .iter()
            .take(10)
            .map(|fp| {
                let (fa, fb) = (&rep.faces[fp.a], &rep.faces[fp.b]);
                vec![json!(fa.element), json!(fa.face), json!(fb.element), json!(fb.face), json!(fp.gap), json!(fa.area), json!(fb.area)]
            })
            .collect(),
    };
    ContactPairOut {
        side_a: a,
        side_b: b,
        faces_a: gp.faces_a.len(),
        faces_b: gp.faces_b.len(),
        area_a: gp.area_a,
        area_b: gp.area_b,
        gap_min: gp.gap_min,
        gap_mean: gp.gap_mean,
        gap_max: gp.gap_max,
        overlap_area_estimate: gp.overlap_area,
        suggested_slave: slave,
        suggested_master: master,
        sample,
    }
}

fn empty_hint(mesh: &InpMesh, groups: &[ContactGroup], rep: &ContactReport, search: &ContactSearch, pname: &str) -> Option<String> {
    if !rep.pairs.is_empty() {
        return None;
    }
    if groups.len() < 2 {
        return Some(format!(
            "partition={pname} gives a single group, so there is nothing to pair. Try partition=elset or partition=material if the parts are distinguished by sets or materials."
        ));
    }
    if rep.faces.is_empty() {
        return Some("The selected groups have no solid exterior faces that could pair (one group selected, or only shells/beams). Check `groups`.".into());
    }
    Some(match nearest_opposing_gap(mesh, groups, search) {
        Some((a, b, g)) => format!(
            "No face pairs within gap_max={}. The closest opposing faces are between {} and {} at a gap of {:.6}; retry with gap_max={:.6} if they should be in contact.",
            search.gap_max,
            groups[a].name,
            groups[b].name,
            g,
            1.2 * g.abs()
        ),
        None => format!("No opposing faces between the groups up to the model size (max_angle_deg={}). The parts do not face each other.", search.max_angle_deg),
    })
}

fn save_pairs(
    roots: &Roots,
    deck: &Path,
    scan: &InpScan,
    spec: &ContactSaveSpec,
    groups: &[ContactGroup],
    rep: &ContactReport,
) -> Result<ContactSavedOut, ToolError> {
    if rep.pairs.len() > MAX_PAIRS_SAVED {
        return Err(ToolError::invalid(format!("{} group pairs found; a save writes at most {MAX_PAIRS_SAVED}", rep.pairs.len()))
            .hint("Pass `groups` to choose the parts to save."));
    }
    let deck_dir = deck.parent().unwrap_or(Path::new("."));
    let mut deck_files: HashSet<PathBuf> = HashSet::new();
    deck_files.insert(deck.to_path_buf());
    // Files with cards, plus included files without any card (only comments or data).
    for f in scan.cards.iter().map(|c| &c.file).chain(scan.includes.iter().map(|i| &i.resolved_path)) {
        deck_files.insert(f.canonicalize().unwrap_or_else(|_| f.clone()));
    }

    // Plan every file first; write only when all of them are allowed.
    let mut plan: Vec<(PathBuf, String)> = Vec::new();
    let mut lines: Vec<(String, String)> = Vec::new(); // (slave surface, master surface)
    for gp in &rep.pairs {
        let stem = format!("{}_{}_{}", spec.prefix, groups[gp.side_a].name, groups[gp.side_b].name);
        let (slave, master) = if gp.slave_is_a() { (&gp.faces_a, &gp.faces_b) } else { (&gp.faces_b, &gp.faces_a) };
        let mut names = Vec::new();
        for (role, idx) in [("slave", slave), ("master", master)] {
            let surf = format!("{stem}_{role}").to_uppercase();
            valid_name(&surf, "surface name")?;
            let out = roots.creatable(&deck_dir.join(format!("{stem}_{role}.sur")).to_string_lossy(), &["sur"])?;
            if deck_files.contains(&out) {
                return Err(ToolError::invalid(format!("{} is a file of the deck; the deck itself is never modified", roots.display(&out)))
                    .hint("Choose another save.prefix."));
            }
            if out.exists() && !spec.overwrite {
                return Err(ToolError::new("RESULTS_EXIST", format!("{} already exists", roots.display(&out)))
                    .hint("Pass save.overwrite=true or choose another save.prefix."));
            }
            let faces: Vec<&Face> = idx.iter().map(|&i| &rep.faces[i]).collect();
            plan.push((out, write_surface_card(&surf, &faces)));
            names.push(surf);
        }
        lines.push((names[0].clone(), names[1].clone()));
    }
    let mut files = Vec::new();
    let mut includes = Vec::new();
    for (out, text) in &plan {
        std::fs::write(out, text).map_err(ToolError::internal)?;
        files.push(roots.display(out));
        let target = out.strip_prefix(deck_dir).map(|r| r.display().to_string()).unwrap_or_else(|_| out.display().to_string());
        includes.push(format!("*INCLUDE, INPUT={target}"));
    }
    let si = format!("{}_SI", spec.prefix).to_uppercase();
    let mut t = String::from(
        "** TEMPLATE TO REVIEW - not written into the deck. Place after the *INCLUDE lines, before the first *STEP.\n\
         ** Slave (first) = suggested finer side; swap the names on a data line to change it.\n\
         ** PRESSURE-OVERCLOSURE=HARD is reduced internally to LINEAR with default constants for penalty contact;\n\
         ** set LINEAR with a slope of 5 to 50 x Young's modulus, or use TYPE=MORTAR. All *CONTACT PAIR cards in a deck must have the same TYPE.\n",
    );
    t += &format!("*SURFACE INTERACTION, NAME={si}\n*SURFACE BEHAVIOR, PRESSURE-OVERCLOSURE=HARD\n*CONTACT PAIR, INTERACTION={si}, TYPE=SURFACE TO SURFACE\n");
    for (s, m) in &lines {
        t += &format!("{s}, {m}\n");
    }
    Ok(ContactSavedOut { files, includes, contact_card_template: t })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cgx_core::faces_of_surface;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn sandbox() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("calculix-mcp-contact-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("root")).unwrap();
        dir
    }

    /// n[0] x n[1] x n[2] block of C3D8 with edge h at origin, ids from `first`, ELSET `name`.
    fn block(n: [usize; 3], h: f64, origin: [f64; 3], first: usize, name: &str) -> String {
        let id = |i: usize, j: usize, k: usize| first + i + (n[0] + 1) * (j + (n[1] + 1) * k);
        let mut s = String::from("*NODE\n");
        for k in 0..=n[2] {
            for j in 0..=n[1] {
                for i in 0..=n[0] {
                    s += &format!("{}, {}, {}, {}\n", id(i, j, k), origin[0] + i as f64 * h, origin[1] + j as f64 * h, origin[2] + k as f64 * h);
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

    /// Coarse 2x2x1 block (edge 1) under a fine 4x4x1 block (edge 0.5) with a 0.02 gap, and a
    /// third block far away.
    fn three_parts() -> String {
        format!(
            "{}{}{}",
            block([2, 2, 1], 1.0, [0.0; 3], 1, "BASE"),
            block([4, 4, 1], 0.5, [0.0, 0.0, 1.02], 1000, "LID"),
            block([1, 1, 1], 1.0, [10.0, 0.0, 0.0], 5000, "FAR")
        )
    }

    fn setup(deck: &str) -> (PathBuf, Roots) {
        let dir = sandbox();
        std::fs::write(dir.join("root/deck.inp"), deck).unwrap();
        let roots = Roots::new(vec![dir.join("root")]).unwrap();
        (dir, roots)
    }

    fn req(path: &str) -> MeshContactIn {
        MeshContactIn { path: path.into(), partition: None, groups: None, gap_max: Some(0.05), max_angle_deg: None, save: None }
    }

    #[test]
    fn response_shape() {
        let (dir, roots) = setup(&three_parts());
        let out = contact_pairs(&roots, req("deck.inp")).unwrap();
        assert_eq!(out.partition, "component");
        assert_eq!(out.groups_total, 3);
        // Components by size: LID (16), BASE (4), FAR (1).
        assert_eq!(out.groups.iter().map(|g| (g.name.as_str(), g.elements)).collect::<Vec<_>>(), [("PART_1", 16), ("PART_2", 4), ("PART_3", 1)]);
        assert_eq!(out.groups[2].bounding_box.min, [10.0, 0.0, 0.0]);
        assert_eq!(out.pairs_total, 1);
        let p = &out.pairs[0];
        assert_eq!((p.side_a.as_str(), p.side_b.as_str()), ("PART_1", "PART_2"));
        assert_eq!((p.faces_a, p.faces_b), (16, 4));
        assert!((p.gap_min - 0.02).abs() < 1e-9 && (p.gap_max - 0.02).abs() < 1e-9);
        assert!((p.overlap_area_estimate - 4.0).abs() < 1e-9);
        assert_eq!((p.suggested_slave.as_str(), p.suggested_master.as_str()), ("PART_1", "PART_2"), "the fine lid is the slave");
        assert_eq!(p.sample.columns.len(), 7);
        assert_eq!(p.sample.rows.len(), 10);
        assert_eq!(out.stages.rows.len(), 5);
        assert_eq!(out.stages.rows[0], vec![json!("faces_considered"), json!(16 + 48 + 6)]);
        assert!(out.hint.is_none() && out.saved.is_none());
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("calculix-mcp-contact"), "no absolute paths: {text}");

        // Default gap_max = 0.1 x the median face size (sqrt(0.25) for most faces here).
        let out = contact_pairs(&roots, MeshContactIn { gap_max: None, ..req("deck.inp") }).unwrap();
        assert!((out.gap_max - 0.05).abs() < 1e-12, "{}", out.gap_max);
        assert_eq!(out.pairs_total, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn groups_filter_and_partitions() {
        let (dir, roots) = setup(&three_parts());
        let out = contact_pairs(&roots, MeshContactIn { groups: Some(vec!["part_3".into()]), ..req("deck.inp") }).unwrap();
        assert_eq!(out.pairs_total, 0);
        assert!(out.hint.as_deref().unwrap().contains("No face pairs") || out.hint.as_deref().unwrap().contains("do not face"), "{:?}", out.hint);
        let out = contact_pairs(&roots, MeshContactIn { groups: Some(vec!["PART_1".into(), "PART_2".into()]), ..req("deck.inp") }).unwrap();
        assert_eq!(out.pairs_total, 1);
        let err = contact_pairs(&roots, MeshContactIn { groups: Some(vec!["PART_9".into()]), ..req("deck.inp") }).unwrap_err();
        assert_eq!(err.code, "UNKNOWN_GROUP");
        assert!(err.valid.contains(&"PART_1".to_string()));

        let out = contact_pairs(&roots, MeshContactIn { partition: Some(PartitionKind::Elset), ..req("deck.inp") }).unwrap();
        assert_eq!(out.groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["BASE", "LID", "FAR"]);
        assert_eq!((out.pairs[0].side_a.as_str(), out.pairs[0].suggested_slave.as_str()), ("BASE", "LID"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_result_has_a_hint() {
        let (dir, roots) = setup(&three_parts());
        let out = contact_pairs(&roots, MeshContactIn { gap_max: Some(0.001), ..req("deck.inp") }).unwrap();
        assert_eq!(out.pairs_total, 0);
        let hint = out.hint.unwrap();
        assert!(hint.contains("PART_1") && hint.contains("PART_2") && hint.contains("0.020000") && hint.contains("gap_max=0.024"), "{hint}");

        // A single part: nothing to pair, suggest another partition.
        std::fs::write(dir.join("root/one.inp"), block([2, 2, 2], 1.0, [0.0; 3], 1, "ALL")).unwrap();
        let out = contact_pairs(&roots, req("one.inp")).unwrap();
        assert!(out.hint.unwrap().contains("single group"));
        assert_eq!(contact_pairs(&roots, MeshContactIn { gap_max: Some(-1.0), ..req("one.inp") }).unwrap_err().code, "INVALID_ARGUMENT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_round_trip_and_overwrite_refusal() {
        let (dir, roots) = setup(&three_parts());
        let spec = |overwrite| Some(ContactSaveSpec { prefix: "lid".into(), overwrite });
        let out = contact_pairs(&roots, MeshContactIn { save: spec(false), ..req("deck.inp") }).unwrap();
        let saved = out.saved.unwrap();
        assert_eq!(saved.files, ["lid_PART_1_PART_2_slave.sur", "lid_PART_1_PART_2_master.sur"]);
        assert_eq!(saved.includes, ["*INCLUDE, INPUT=lid_PART_1_PART_2_slave.sur", "*INCLUDE, INPUT=lid_PART_1_PART_2_master.sur"]);
        let t = &saved.contact_card_template;
        assert!(t.starts_with("** TEMPLATE TO REVIEW"));
        assert!(t.contains("*SURFACE INTERACTION, NAME=LID_SI\n*SURFACE BEHAVIOR, PRESSURE-OVERCLOSURE=HARD\n*CONTACT PAIR, INTERACTION=LID_SI, TYPE=SURFACE TO SURFACE\nLID_PART_1_PART_2_SLAVE, LID_PART_1_PART_2_MASTER\n"), "{t}");

        // The deck is untouched; a copy with the includes appended reads back the same faces.
        let original = std::fs::read_to_string(dir.join("root/deck.inp")).unwrap();
        assert_eq!(original, three_parts());
        std::fs::write(dir.join("root/deck2.inp"), format!("{original}{}\n", saved.includes.join("\n"))).unwrap();
        let mesh = read_inp_mesh(&dir.join("root/deck2.inp")).unwrap();
        let mut slave: Vec<(u32, u8)> = faces_of_surface(&mesh, "LID_PART_1_PART_2_SLAVE").unwrap().iter().map(|f| (f.element, f.face)).collect();
        let mut master: Vec<(u32, u8)> = faces_of_surface(&mesh, "lid_part_1_part_2_master").unwrap().iter().map(|f| (f.element, f.face)).collect();
        slave.sort_unstable();
        master.sort_unstable();
        assert_eq!(slave.len(), 16);
        assert!(slave.iter().all(|&(e, f)| (1000..5000).contains(&e) && f == 1), "lid bottoms: {slave:?}");
        assert_eq!(master, [(1, 2), (2, 2), (3, 2), (4, 2)], "base tops");
        // Same faces as the detection reports.
        let mesh0 = read_inp_mesh(&dir.join("root/deck.inp")).unwrap();
        let scan0 = scan_cards(&dir.join("root/deck.inp"), &ScanLimits::default()).unwrap();
        let groups = partition_groups(&mesh0, &scan0, Partition::Component);
        let rep = find_contact_pairs(&mesh0, &groups, &ContactSearch { gap_max: 0.05, max_angle_deg: 30.0, groups: Vec::new() });
        let mut want: Vec<(u32, u8)> = rep.pairs[0].faces_a.iter().map(|&i| (rep.faces[i].element, rep.faces[i].face)).collect();
        want.sort_unstable();
        assert_eq!(slave, want);

        let err = contact_pairs(&roots, MeshContactIn { save: spec(false), ..req("deck.inp") }).unwrap_err();
        assert_eq!(err.code, "RESULTS_EXIST");
        assert!(contact_pairs(&roots, MeshContactIn { save: spec(true), ..req("deck.inp") }).is_ok());

        // Nothing found: nothing saved.
        let out = contact_pairs(&roots, MeshContactIn { gap_max: Some(0.001), save: Some(ContactSaveSpec { prefix: "none".into(), overwrite: false }), ..req("deck.inp") }).unwrap();
        assert!(out.saved.is_none() && out.hint.unwrap().ends_with("Nothing was saved."));
        assert!(!dir.join("root/none_PART_1_PART_2_slave.sur").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bad_names_and_paths_outside_roots() {
        let (dir, roots) = setup(&three_parts());
        std::fs::write(dir.join("outside.inp"), three_parts()).unwrap();
        let err = contact_pairs(&roots, req("../outside.inp")).unwrap_err();
        assert_eq!(err.code, "PATH_OUTSIDE_ROOTS");
        let err = contact_pairs(&roots, req(dir.join("outside.inp").to_str().unwrap())).unwrap_err();
        assert_eq!(err.code, "PATH_OUTSIDE_ROOTS");
        for bad in ["", "1x", "a b", "a/b", "../x"] {
            let err = contact_pairs(&roots, MeshContactIn { save: Some(ContactSaveSpec { prefix: bad.into(), overwrite: true }), ..req("deck.inp") }).unwrap_err();
            assert_eq!(err.code, "INVALID_ARGUMENT", "prefix {bad:?}");
        }
        // A prefix that makes the surface name longer than 80 characters.
        let long = "p".repeat(70);
        let err = contact_pairs(&roots, MeshContactIn { save: Some(ContactSaveSpec { prefix: long, overwrite: true }), ..req("deck.inp") }).unwrap_err();
        assert_eq!(err.code, "INVALID_ARGUMENT");
        assert!(std::fs::read_dir(dir.join("root")).unwrap().count() == 1, "nothing written");

        // A target that the deck already includes is refused even with overwrite=true.
        std::fs::write(dir.join("root/inc_PART_1_PART_2_master.sur"), "** kept\n").unwrap();
        std::fs::write(dir.join("root/deck.inp"), format!("{}*INCLUDE, INPUT=inc_PART_1_PART_2_master.sur\n", three_parts())).unwrap();
        let err = contact_pairs(&roots, MeshContactIn { save: Some(ContactSaveSpec { prefix: "inc".into(), overwrite: true }), ..req("deck.inp") }).unwrap_err();
        assert_eq!(err.code, "INVALID_ARGUMENT");
        assert!(err.message.contains("file of the deck"), "{}", err.message);
        assert_eq!(std::fs::read_to_string(dir.join("root/inc_PART_1_PART_2_master.sur")).unwrap(), "** kept\n");
        assert!(!dir.join("root/inc_PART_1_PART_2_slave.sur").exists(), "no partial write");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[allow(clippy::print_stdout)]
    fn rotor_deck_runs_quickly() {
        let deck = Path::new("/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor/lavffcyc.inp");
        if !deck.exists() {
            eprintln!("rotor deck not present, skipping");
            return;
        }
        let roots = Roots::new(vec![deck.parent().unwrap().to_path_buf()]).unwrap();
        let t = std::time::Instant::now();
        let out = contact_pairs(&roots, MeshContactIn { gap_max: None, ..req("lavffcyc.inp") }).unwrap();
        println!("rotor mesh_contact_pairs: {:.0} ms (deck read included), groups {}, hint {:?}", t.elapsed().as_secs_f64() * 1e3, out.groups_total, out.hint);
        assert_eq!(out.pairs_total, 0);
        assert!(out.hint.unwrap().contains("single group"));
    }
}
