//! Preprocessing tools: `mesh_check`, `mesh_select`.

use crate::error::{nearest, ToolError};
use crate::frd_tools::Table;
use crate::paths::Roots;
use cgx_core::{
    by_normal, components, connected_patch, exterior_faces, face_nodes, faces_of_surface, free_nodes, in_box, mass_properties,
    near_point, nearest_face, on_cylinder, on_plane, quality, read_inp_mesh, reference_problems, scan_cards, write_nset_card,
    write_surface_card, Face, InpMesh, InpScan, ScanLimits,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

type V3 = [f64; 3];

#[derive(Debug, Serialize, JsonSchema)]
pub struct BBox {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

fn read_deck(roots: &Roots, raw: &str) -> Result<(std::path::PathBuf, InpMesh, InpScan), ToolError> {
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

/// Remove the absolute root prefix from paths embedded in a message.
fn relative(roots: &Roots, msg: &str) -> String {
    let prefix = format!("{}{}", roots.first().display(), std::path::MAIN_SEPARATOR);
    msg.replace(&prefix, "")
}

fn node_pos(mesh: &InpMesh, id: u32) -> Option<V3> {
    mesh.node_index.get(&id).map(|&k| [mesh.nodes[k].x, mesh.nodes[k].y, mesh.nodes[k].z])
}

fn bbox_of(points: impl Iterator<Item = V3>) -> BBox {
    let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    let mut any = false;
    for p in points {
        any = true;
        for (k, v) in p.into_iter().enumerate() {
            lo[k] = lo[k].min(v);
            hi[k] = hi[k].max(v);
        }
    }
    if any { BBox { min: lo, max: hi } } else { BBox { min: [0.0; 3], max: [0.0; 3] } }
}

// ---------- mesh_check ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MeshCheckIn {
    /// Path to the root .inp deck
    pub path: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PartsOut {
    /// Groups of elements connected through shared nodes
    pub count: usize,
    /// Element counts of the 10 largest parts, largest first
    pub largest_sizes: Vec<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IdList {
    pub count: usize,
    /// At most the first 10 ids, ascending
    pub first: Vec<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct QualityOut {
    /// Solid elements with volume <= 0 (inverted or collapsed)
    pub non_positive_volume: IdList,
    /// Largest ratio of longest to shortest corner-node edge; null when there are no solids or an edge has zero length
    pub max_aspect: Option<f64>,
    /// Columns: element, aspect. The 10 worst solid elements
    pub worst_aspect: Table,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MassOut {
    /// Sum of signed solid element volumes, deck units cubed
    pub volume: f64,
    /// Null when any solid element has no density
    pub mass: Option<f64>,
    /// Mass-weighted centre of gravity; null when the mass is null
    pub centre_of_gravity: Option<[f64; 3]>,
    /// Columns: elset, material, density, elements, volume, mass. One row per *SOLID SECTION
    pub by_section: Table,
    /// Solid elements that no *SOLID SECTION covers
    pub unassigned_elements: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ProblemsOut {
    pub total: usize,
    /// At most 20: undefined or empty sets, undefined materials, then reader warnings
    pub items: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MeshCheckOut {
    /// OK, or REVIEW when there is more than one part, a free node, a non-positive volume, an unassigned element or a problem
    pub verdict: String,
    pub nodes: usize,
    pub elements: usize,
    /// Element TYPE= label -> count
    pub element_types: BTreeMap<String, usize>,
    pub bounding_box: BBox,
    pub parts: PartsOut,
    /// Nodes that no element uses
    pub free_nodes: IdList,
    pub quality: QualityOut,
    pub mass: MassOut,
    pub problems: ProblemsOut,
    pub notes: Vec<String>,
}

fn id_list(mut ids: Vec<u32>) -> IdList {
    ids.sort_unstable();
    let count = ids.len();
    ids.truncate(10);
    IdList { count, first: ids }
}

pub fn check(roots: &Roots, p: MeshCheckIn) -> Result<MeshCheckOut, ToolError> {
    let (_, mesh, scan) = read_deck(roots, &p.path)?;

    let mut element_types: BTreeMap<String, usize> = BTreeMap::new();
    for t in &mesh.element_types {
        *element_types.entry(t.clone()).or_default() += 1;
    }
    let parts = components(&mesh);
    let free = free_nodes(&mesh);
    let q = quality(&mesh);
    let mass = mass_properties(&mesh, &scan);

    let mut problems: Vec<String> = reference_problems(&mesh, &scan);
    // Node surfaces are not read by the mesh reader; that is expected, not a deck problem.
    let (skipped, warnings): (Vec<&String>, Vec<&String>) =
        mesh.warnings.iter().partition(|w| w.contains("skipped (only TYPE=ELEMENT is read)"));
    problems.extend(warnings.into_iter().cloned());
    let problems_total = problems.len();
    let items: Vec<String> = problems.iter().take(20).map(|m| relative(roots, m)).collect();

    let verdict = if parts.len() > 1
        || !free.is_empty()
        || !q.non_positive_volume.is_empty()
        || mass.unassigned_elements > 0
        || problems_total > 0
    {
        "REVIEW"
    } else {
        "OK"
    };

    let by_section = Table {
        columns: ["elset", "material", "density", "elements", "volume", "mass"].map(String::from).to_vec(),
        rows: mass
            .by_material
            .iter()
            .map(|m| vec![json!(m.elset), json!(m.material), json!(m.density), json!(m.elements), json!(m.volume), json!(m.mass)])
            .collect(),
    };
    let worst_aspect = Table {
        columns: ["element", "aspect"].map(String::from).to_vec(),
        rows: q.worst_aspect.iter().map(|(e, a)| vec![json!(e), json!(a)]).collect(),
    };
    let bb = mass.bounding_box;
    Ok(MeshCheckOut {
        verdict: verdict.into(),
        nodes: mesh.nodes.len(),
        elements: mesh.elements.len(),
        element_types,
        bounding_box: BBox { min: bb.0, max: bb.1 },
        parts: PartsOut { count: parts.len(), largest_sizes: parts.iter().take(10).map(Vec::len).collect() },
        free_nodes: id_list(free),
        quality: QualityOut {
            non_positive_volume: id_list(q.non_positive_volume),
            max_aspect: (q.max_aspect.is_finite() && q.max_aspect > 0.0).then_some(q.max_aspect),
            worst_aspect,
        },
        mass: MassOut {
            volume: mass.volume,
            mass: mass.mass,
            centre_of_gravity: mass.centre_of_gravity,
            by_section,
            unassigned_elements: mass.unassigned_elements,
        },
        problems: ProblemsOut { total: problems_total, items },
        notes: [
            "Volume, mass and centre of gravity use corner nodes only (mid-side nodes are ignored), so curved quadratic edges are approximated.".into(),
            "Values are in the units of the deck; no unit conversion is applied.".into(),
            "Aspect is the longest over the shortest corner-node edge of an element. Only solid elements are measured.".into(),
        ]
        .into_iter()
        .chain((!skipped.is_empty()).then(|| format!("{} *SURFACE card(s) of TYPE=NODE are not read and not counted as problems.", skipped.len())))
        .collect(),
    })
}

// ---------- mesh_select ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NormalFilter {
    /// Direction the face normals must point to (need not be unit length)
    pub direction: [f64; 3],
    /// Largest angle between face normal and direction (default 5)
    pub max_angle_deg: Option<f64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PlaneFilter {
    pub point: [f64; 3],
    pub normal: [f64; 3],
    /// All corner nodes must lie within this distance of the plane
    pub tol: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BoxFilter {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CylinderFilter {
    pub axis_point: [f64; 3],
    pub axis_dir: [f64; 3],
    pub radius: f64,
    /// All corner nodes must lie at radius +- tol from the axis
    pub tol: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NearFilter {
    pub point: [f64; 3],
    /// Face centroids within this distance are kept
    pub radius: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PatchFilter {
    /// The face nearest to this point (among the faces that passed the other filters) starts the flood
    pub seed: [f64; 3],
    /// Largest normal change across an edge the flood may cross (default 30)
    pub max_angle_deg: Option<f64>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SaveKind {
    /// *SURFACE, TYPE=ELEMENT file with `element, S<n>` lines
    Surface,
    /// *NSET file with the distinct node ids of the selection (mid-side nodes included)
    Nset,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SaveSpec {
    /// Letters, digits, `_` and `-`; starts with a letter; at most 80 characters
    pub name: String,
    pub kind: SaveKind,
    /// Default: <deck dir>/<NAME>.sur or .nam. Extension must be sur, nam, inp or inc
    pub output_path: Option<String>,
    /// Replace an existing file (default false)
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MeshSelectIn {
    /// Path to the root .inp deck
    pub path: String,
    /// Name of an element-face *SURFACE in the deck to start from. Default: all exterior faces
    pub from: Option<String>,
    /// Faces whose normal points along a direction
    pub normal: Option<NormalFilter>,
    /// Faces lying in a plane
    pub plane: Option<PlaneFilter>,
    /// Faces whose centroid is inside a box
    #[serde(rename = "box")]
    pub box_: Option<BoxFilter>,
    /// Faces lying on a cylinder
    pub cylinder: Option<CylinderFilter>,
    /// Faces whose centroid is near a point
    pub near: Option<NearFilter>,
    /// Keep only the connected patch around a seed point; applied last
    pub patch: Option<PatchFilter>,
    /// Write the selection as an include file
    pub save: Option<SaveSpec>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SavedOut {
    pub file: String,
    /// The line to add to the deck to use the file
    pub card: String,
    /// Lines written
    pub lines: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MeshSelectOut {
    /// Where the faces came from
    pub source: String,
    pub faces: usize,
    pub area: f64,
    /// Distinct nodes including mid-side nodes
    pub nodes: usize,
    /// Bounding box of those nodes
    pub bounding_box: BBox,
    /// Area-weighted unit normal; zeros for an empty or closed selection
    pub mean_normal: [f64; 3],
    /// Largest distance of the corner nodes from the plane through their mean with the mean normal
    pub planarity: f64,
    /// Columns: filter, remaining. Face count at the start and after each filter, in application order
    pub stages: Table,
    /// Columns: element, face, cx, cy, cz, area. Up to 10 faces, largest area first
    pub sample: Table,
    pub saved: Option<SavedOut>,
    /// Set when the selection is empty or could not be saved
    pub hint: Option<String>,
}

const FILTER_NAMES: &str = "normal, plane, box, cylinder, near, patch";

fn finite3(v: &[f64; 3]) -> bool {
    v.iter().all(|x| x.is_finite())
}

fn nonzero3(v: &[f64; 3]) -> bool {
    finite3(v) && v.iter().any(|x| *x != 0.0)
}

fn validate(p: &MeshSelectIn) -> Result<(), ToolError> {
    let bad = |what: &str| Err(ToolError::invalid(what.to_string()));
    if let Some(f) = &p.normal {
        if !nonzero3(&f.direction) {
            return bad("normal.direction must be a finite, non-zero vector");
        }
        if !f.max_angle_deg.unwrap_or(5.0).within(0.0, 180.0) {
            return bad("normal.max_angle_deg must be between 0 and 180");
        }
    }
    if let Some(f) = &p.plane {
        if !finite3(&f.point) || !nonzero3(&f.normal) {
            return bad("plane.point must be finite and plane.normal a finite, non-zero vector");
        }
        if !(f.tol.is_finite() && f.tol >= 0.0) {
            return bad("plane.tol must be a non-negative number");
        }
    }
    if let Some(f) = &p.box_ {
        if !finite3(&f.min) || !finite3(&f.max) || (0..3).any(|k| f.min[k] > f.max[k]) {
            return bad("box.min must be finite and not larger than box.max in every axis");
        }
    }
    if let Some(f) = &p.cylinder {
        if !finite3(&f.axis_point) || !nonzero3(&f.axis_dir) {
            return bad("cylinder.axis_point must be finite and cylinder.axis_dir a finite, non-zero vector");
        }
        if !(f.radius.is_finite() && f.radius >= 0.0 && f.tol.is_finite() && f.tol >= 0.0) {
            return bad("cylinder.radius and cylinder.tol must be non-negative numbers");
        }
    }
    if let Some(f) = &p.near {
        if !finite3(&f.point) || !(f.radius.is_finite() && f.radius >= 0.0) {
            return bad("near.point must be finite and near.radius a non-negative number");
        }
    }
    if let Some(f) = &p.patch {
        if !finite3(&f.seed) || !f.max_angle_deg.unwrap_or(30.0).within(0.0, 180.0) {
            return bad("patch.seed must be finite and patch.max_angle_deg between 0 and 180");
        }
    }
    if let Some(s) = &p.save {
        valid_name(&s.name)?;
    }
    Ok(())
}

trait FiniteIn {
    fn within(self, lo: f64, hi: f64) -> bool;
}
impl FiniteIn for f64 {
    fn within(self, lo: f64, hi: f64) -> bool {
        self.is_finite() && self >= lo && self <= hi
    }
}

/// `^[A-Za-z][A-Za-z0-9_-]{0,79}$`
fn valid_name(name: &str) -> Result<(), ToolError> {
    let ok = !name.is_empty()
        && name.len() <= 80
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(ToolError::invalid(format!("'{name}' is not a valid name"))
            .hint("Use a name that starts with a letter and has at most 80 letters, digits, '_' or '-'."))
    }
}

/// Keep the faces at `idx` (indices into `faces`), preserving order.
fn pick(faces: &[Face], idx: Vec<usize>) -> Vec<Face> {
    idx.into_iter().map(|i| faces[i].clone()).collect()
}

fn mean_normal_of(faces: &[Face]) -> V3 {
    let mut n = [0.0; 3];
    for f in faces {
        for (k, c) in n.iter_mut().enumerate() {
            *c += f.normal[k] * f.area;
        }
    }
    let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if l > 1e-12 * faces.iter().map(|f| f.area).sum::<f64>().max(f64::MIN_POSITIVE) { [n[0] / l, n[1] / l, n[2] / l] } else { [0.0; 3] }
}

pub fn select(roots: &Roots, p: MeshSelectIn) -> Result<MeshSelectOut, ToolError> {
    let has_filter = p.normal.is_some()
        || p.plane.is_some()
        || p.box_.is_some()
        || p.cylinder.is_some()
        || p.near.is_some()
        || p.patch.is_some();
    if !has_filter && p.from.is_none() {
        return Err(ToolError::invalid("give at least one filter, or `from` to start from a surface")
            .hint(format!("Filters: {FILTER_NAMES}.")));
    }
    validate(&p)?;
    let (deck, mesh, scan) = read_deck(roots, &p.path)?;

    let (mut cur, source) = match &p.from {
        Some(name) => {
            let faces = faces_of_surface(&mesh, name).ok_or_else(|| {
                ToolError::new("UNKNOWN_SURFACE", format!("no element-face surface named '{name}' in the deck"))
                    .valid(nearest(name, mesh.surfaces.keys().cloned()))
                    .hint("Pick one of the valid names, or omit `from` to use all exterior faces.")
            })?;
            (faces, format!("surface {}", name.to_uppercase()))
        }
        None => (exterior_faces(&mesh), "all exterior faces".to_string()),
    };

    let mut stages: Vec<(String, usize)> = vec![("start".into(), cur.len())];
    if let Some(f) = &p.normal {
        let idx = by_normal(&cur, f.direction, f.max_angle_deg.unwrap_or(5.0));
        cur = pick(&cur, idx);
        stages.push(("normal".into(), cur.len()));
    }
    if let Some(f) = &p.plane {
        let idx = on_plane(&mesh, &cur, f.point, f.normal, f.tol);
        cur = pick(&cur, idx);
        stages.push(("plane".into(), cur.len()));
    }
    if let Some(f) = &p.box_ {
        let idx = in_box(&cur, f.min, f.max);
        cur = pick(&cur, idx);
        stages.push(("box".into(), cur.len()));
    }
    if let Some(f) = &p.cylinder {
        let idx = on_cylinder(&mesh, &cur, f.axis_point, f.axis_dir, f.radius, f.tol);
        cur = pick(&cur, idx);
        stages.push(("cylinder".into(), cur.len()));
    }
    if let Some(f) = &p.near {
        let idx = near_point(&cur, f.point, f.radius);
        cur = pick(&cur, idx);
        stages.push(("near".into(), cur.len()));
    }
    if let Some(f) = &p.patch {
        let idx = match nearest_face(&cur, f.seed) {
            Some(seed) => connected_patch(&cur, seed, f.max_angle_deg.unwrap_or(30.0)),
            None => Vec::new(),
        };
        cur = pick(&cur, idx);
        stages.push(("patch".into(), cur.len()));
    }

    // Distinct nodes of the selection, mid-side nodes included.
    let mut node_ids: BTreeSet<u32> = BTreeSet::new();
    let mut corner_ids: BTreeSet<u32> = BTreeSet::new();
    for f in &cur {
        node_ids.extend(face_nodes(&mesh, f));
        corner_ids.extend(f.corners.iter().copied());
    }
    let area: f64 = cur.iter().map(|f| f.area).sum::<f64>() + 0.0;
    let mean_normal = mean_normal_of(&cur);
    let corners: Vec<V3> = corner_ids.iter().filter_map(|&id| node_pos(&mesh, id)).collect();
    let planarity = if corners.is_empty() || mean_normal == [0.0; 3] {
        0.0
    } else {
        let n = corners.len() as f64;
        let c = [
            corners.iter().map(|p| p[0]).sum::<f64>() / n,
            corners.iter().map(|p| p[1]).sum::<f64>() / n,
            corners.iter().map(|p| p[2]).sum::<f64>() / n,
        ];
        corners
            .iter()
            .map(|p| ((p[0] - c[0]) * mean_normal[0] + (p[1] - c[1]) * mean_normal[1] + (p[2] - c[2]) * mean_normal[2]).abs())
            .fold(0.0, f64::max)
    };

    let mut by_area: Vec<&Face> = cur.iter().collect();
    by_area.sort_by(|a, b| b.area.total_cmp(&a.area).then(a.element.cmp(&b.element)).then(a.face.cmp(&b.face)));
    let sample = Table {
        columns: ["element", "face", "cx", "cy", "cz", "area"].map(String::from).to_vec(),
        rows: by_area
            .iter()
            .take(10)
            .map(|f| vec![json!(f.element), json!(f.face), json!(f.centroid[0]), json!(f.centroid[1]), json!(f.centroid[2]), json!(f.area)])
            .collect(),
    };

    let mut hint = None;
    if cur.is_empty() {
        let culprit = stages.iter().find(|s| s.1 == 0);
        hint = Some(match culprit {
            Some((name, _)) if name == "start" => format!("{source} contains no usable solid faces."),
            Some((name, _)) => {
                let before = stages.iter().position(|s| s.0 == *name).map(|i| stages[i - 1].1).unwrap_or(0);
                format!("The `{name}` filter removed all {before} remaining faces. Relax it or check its values against mesh_check's bounding box.")
            }
            None => "No faces selected.".to_string(),
        });
    }

    let saved = match (&p.save, cur.is_empty()) {
        (Some(_), true) => {
            hint = Some(format!("{} Nothing was saved.", hint.unwrap_or_default()).trim().to_string());
            None
        }
        (Some(spec), false) => {
            let ids: Vec<u32> = node_ids.iter().copied().collect();
            let refs: Vec<&Face> = cur.iter().collect();
            Some(save_selection(roots, &deck, &scan, spec, &refs, &ids)?)
        }
        (None, _) => None,
    };

    Ok(MeshSelectOut {
        source,
        faces: cur.len(),
        area,
        nodes: node_ids.len(),
        bounding_box: bbox_of(node_ids.iter().filter_map(|&id| node_pos(&mesh, id))),
        mean_normal,
        planarity,
        stages: Table {
            columns: ["filter", "remaining"].map(String::from).to_vec(),
            rows: stages.iter().map(|(n, c)| vec![json!(n), json!(c)]).collect(),
        },
        sample,
        saved,
        hint,
    })
}

fn save_selection(
    roots: &Roots,
    deck: &Path,
    scan: &InpScan,
    spec: &SaveSpec,
    faces: &[&Face],
    node_ids: &[u32],
) -> Result<SavedOut, ToolError> {
    let deck_dir = deck.parent().unwrap_or(Path::new("."));
    let default = deck_dir.join(format!("{}.{}", spec.name, if spec.kind == SaveKind::Surface { "sur" } else { "nam" }));
    let raw = spec.output_path.clone().unwrap_or_else(|| default.to_string_lossy().into_owned());
    let out = roots.creatable(&raw, &["sur", "nam", "inp", "inc"])?;

    // Never overwrite the deck or any file it includes.
    let mut deck_files: HashSet<std::path::PathBuf> = HashSet::new();
    deck_files.insert(deck.to_path_buf());
    for c in &scan.cards {
        deck_files.insert(c.file.canonicalize().unwrap_or_else(|_| c.file.clone()));
    }
    if deck_files.contains(&out) {
        return Err(ToolError::invalid("output_path is a file of the deck; the deck itself is never modified")
            .hint("Choose a new .sur or .nam file name."));
    }
    if out.exists() && !spec.overwrite {
        return Err(ToolError::new("RESULTS_EXIST", format!("{} already exists", roots.display(&out)))
            .hint("Pass save.overwrite=true or choose another output_path."));
    }
    let text = match spec.kind {
        SaveKind::Surface => write_surface_card(&spec.name, faces),
        SaveKind::Nset => write_nset_card(&spec.name, node_ids),
    };
    std::fs::write(&out, &text).map_err(ToolError::internal)?;
    let target = out.strip_prefix(deck_dir).map(|r| r.display().to_string()).unwrap_or_else(|_| out.display().to_string());
    Ok(SavedOut { file: roots.display(&out), card: format!("*INCLUDE, INPUT={target}"), lines: text.lines().count() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn sandbox() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("calculix-mcp-mesh-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("root")).unwrap();
        dir
    }

    /// nx x ny x nz block of unit C3D8, ELSET=EALL, with a steel section.
    fn block_deck(nx: usize, ny: usize, nz: usize) -> String {
        let id = |i: usize, j: usize, k: usize| 1 + i + (nx + 1) * (j + (ny + 1) * k);
        let mut s = String::from("*NODE\n");
        for k in 0..=nz {
            for j in 0..=ny {
                for i in 0..=nx {
                    s += &format!("{}, {}, {}, {}\n", id(i, j, k), i, j, k);
                }
            }
        }
        s += "*ELEMENT, TYPE=C3D8, ELSET=EALL\n";
        let mut e = 1;
        for k in 0..nz {
            for j in 0..ny {
                for i in 0..nx {
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
        s += "*MATERIAL, NAME=STEEL\n*DENSITY\n7.85e-9\n*SOLID SECTION, ELSET=EALL, MATERIAL=STEEL\n";
        s
    }

    fn setup(deck: &str) -> (PathBuf, Roots) {
        let dir = sandbox();
        std::fs::write(dir.join("root/deck.inp"), deck).unwrap();
        let roots = Roots::new(vec![dir.join("root")]).unwrap();
        (dir, roots)
    }

    fn sel(path: &str) -> MeshSelectIn {
        MeshSelectIn { path: path.into(), from: None, normal: None, plane: None, box_: None, cylinder: None, near: None, patch: None, save: None }
    }

    fn up() -> NormalFilter {
        NormalFilter { direction: [0.0, 0.0, 1.0], max_angle_deg: None }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn check_ok_and_review() {
        let (dir, roots) = setup(&block_deck(2, 2, 2));
        let out = check(&roots, MeshCheckIn { path: "deck.inp".into() }).unwrap();
        assert_eq!(out.verdict, "OK", "{:?}", out.problems.items);
        assert_eq!((out.nodes, out.elements, out.parts.count), (27, 8, 1));
        assert_eq!(out.element_types["C3D8"], 8);
        assert!(close(out.mass.volume, 8.0) && close(out.mass.mass.unwrap(), 8.0 * 7.85e-9));
        assert_eq!(out.mass.by_section.rows.len(), 1);
        assert_eq!(out.mass.by_section.rows[0][1], json!("STEEL"));
        assert_eq!(out.bounding_box.max, [2.0, 2.0, 2.0]);
        assert_eq!(out.quality.max_aspect, Some(1.0));
        assert_eq!(out.notes.len(), 3);
        let text = serde_json::to_string(&out).unwrap();
        assert!(!text.contains("calculix-mcp-mesh"), "paths are relative: {text}");

        // A free node, a second part and a bad reference make it REVIEW.
        let deck = format!(
            "{}*NODE\n900, 9, 9, 9\n*NODE\n100, 10,0,0\n101, 11,0,0\n102, 11,1,0\n103, 10,1,0\n104, 10,0,1\n105, 11,0,1\n106, 11,1,1\n107, 10,1,1\n\
*ELEMENT, TYPE=C3D8\n100, 100,101,102,103,104,105,106,107\n*BOUNDARY\nGHOST, 1, 3\n",
            block_deck(2, 2, 2)
        );
        std::fs::write(dir.join("root/deck.inp"), deck).unwrap();
        let out = check(&roots, MeshCheckIn { path: "deck.inp".into() }).unwrap();
        assert_eq!(out.verdict, "REVIEW");
        assert_eq!(out.parts.largest_sizes, vec![8, 1]);
        assert_eq!(out.free_nodes.first, vec![900]);
        assert_eq!(out.mass.unassigned_elements, 1);
        assert!(out.mass.mass.is_none());
        assert!(out.problems.items.iter().any(|m| m.starts_with("deck.inp:") && m.contains("GHOST")), "{:?}", out.problems.items);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn select_normal_plane_box_and_patch() {
        let (dir, roots) = setup(&block_deck(3, 3, 1));

        let out = select(&roots, MeshSelectIn { normal: Some(up()), ..sel("deck.inp") }).unwrap();
        assert_eq!(out.faces, 9);
        assert!(close(out.area, 9.0));
        assert_eq!(out.nodes, 16);
        assert_eq!(out.mean_normal, [0.0, 0.0, 1.0]);
        assert!(out.planarity < 1e-12);
        assert_eq!(out.bounding_box.min, [0.0, 0.0, 1.0]);
        assert_eq!(out.bounding_box.max, [3.0, 3.0, 1.0]);
        assert_eq!(out.sample.rows.len(), 9);
        assert_eq!(out.stages.rows[0], vec![json!("start"), json!(9 + 9 + 12)]);
        assert!(out.hint.is_none() && out.saved.is_none());

        let plane = PlaneFilter { point: [0.0, 0.0, 1.0], normal: [0.0, 0.0, 1.0], tol: 1e-9 };
        let bx = BoxFilter { min: [0.0, 0.0, 0.0], max: [1.0, 3.0, 2.0] };
        let out = select(&roots, MeshSelectIn { plane: Some(plane), box_: Some(bx), ..sel("deck.inp") }).unwrap();
        assert_eq!(out.faces, 3);
        assert!(close(out.area, 3.0));
        assert_eq!(out.stages.rows.iter().map(|r| r[0].as_str().unwrap()).collect::<Vec<_>>(), ["start", "plane", "box"]);

        // Patch: the flood stays on the top plane because the edges to the sides are 90 degrees.
        let patch = PatchFilter { seed: [0.4, 0.4, 1.0], max_angle_deg: None };
        let out = select(&roots, MeshSelectIn { patch: Some(patch), ..sel("deck.inp") }).unwrap();
        assert_eq!((out.faces, out.nodes), (9, 16));
        // A wide angle crosses the edges and takes the whole closed surface.
        let patch = PatchFilter { seed: [0.4, 0.4, 1.0], max_angle_deg: Some(100.0) };
        let out = select(&roots, MeshSelectIn { patch: Some(patch), ..sel("deck.inp") }).unwrap();
        assert_eq!(out.faces, 30);
        assert!(out.mean_normal == [0.0; 3] || out.mean_normal.iter().all(|c| c.abs() < 1e-9));

        // The patch only crosses faces that passed the other filters (one strip y < 1).
        let strip = BoxFilter { min: [0.0, 0.0, 0.0], max: [3.0, 0.9, 1.0] };
        let patch = PatchFilter { seed: [0.5, 0.5, 1.0], max_angle_deg: None };
        let out = select(&roots, MeshSelectIn { box_: Some(strip), patch: Some(patch), ..sel("deck.inp") }).unwrap();
        assert_eq!(out.faces, 3, "top strip only");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn select_cylinder_near_and_from() {
        let mut deck = block_deck(3, 3, 1);
        deck += "*SURFACE, NAME=Top, TYPE=ELEMENT\nEALL, S2\n";
        let (dir, roots) = setup(&deck);
        let near = NearFilter { point: [0.5, 0.5, 1.0], radius: 0.1 };
        let out = select(&roots, MeshSelectIn { near: Some(near), ..sel("deck.inp") }).unwrap();
        assert_eq!(out.faces, 1);
        let out = select(&roots, MeshSelectIn { from: Some("top".into()), ..sel("deck.inp") }).unwrap();
        assert_eq!((out.faces, out.source.as_str()), (9, "surface TOP"));
        let err = select(&roots, MeshSelectIn { from: Some("tpo".into()), ..sel("deck.inp") }).unwrap_err();
        assert_eq!((err.code, err.valid.as_slice()), ("UNKNOWN_SURFACE", &["TOP".to_string()][..]));
        let cyl = CylinderFilter { axis_point: [0.0; 3], axis_dir: [0.0, 0.0, 1.0], radius: 1.0, tol: 1e-9 };
        let out = select(&roots, MeshSelectIn { cylinder: Some(cyl), ..sel("deck.inp") }).unwrap();
        assert_eq!(out.faces, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_selection_explains_itself() {
        let (dir, roots) = setup(&block_deck(3, 3, 1));
        let plane = PlaneFilter { point: [0.0, 0.0, 5.0], normal: [0.0, 0.0, 1.0], tol: 1e-6 };
        let save = SaveSpec { name: "NOPE".into(), kind: SaveKind::Surface, output_path: None, overwrite: false };
        let out = select(&roots, MeshSelectIn { normal: Some(up()), plane: Some(plane), save: Some(save), ..sel("deck.inp") }).unwrap();
        assert_eq!((out.faces, out.nodes), (0, 0));
        assert_eq!(out.area, 0.0);
        assert_eq!(out.stages.rows, vec![vec![json!("start"), json!(30)], vec![json!("normal"), json!(9)], vec![json!("plane"), json!(0)]]);
        let hint = out.hint.unwrap();
        assert!(hint.contains("`plane`") && hint.contains("9") && hint.contains("Nothing was saved"), "{hint}");
        assert!(out.saved.is_none() && !dir.join("root/NOPE.sur").exists());

        let err = select(&roots, sel("deck.inp")).unwrap_err();
        assert_eq!(err.code, "INVALID_ARGUMENT");
        assert!(err.hint.unwrap().contains("normal, plane, box, cylinder, near, patch"));
        let err = select(&roots, MeshSelectIn { normal: Some(NormalFilter { direction: [0.0; 3], max_angle_deg: None }), ..sel("deck.inp") }).unwrap_err();
        assert_eq!(err.code, "INVALID_ARGUMENT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_surface_roundtrips_and_refuses_overwrite() {
        let (dir, roots) = setup(&block_deck(3, 3, 1));
        let spec = |overwrite| SaveSpec { name: "TOP".into(), kind: SaveKind::Surface, output_path: None, overwrite };
        let out = select(&roots, MeshSelectIn { normal: Some(up()), save: Some(spec(false)), ..sel("deck.inp") }).unwrap();
        let saved = out.saved.unwrap();
        assert_eq!((saved.file.as_str(), saved.card.as_str(), saved.lines), ("TOP.sur", "*INCLUDE, INPUT=TOP.sur", 10));
        let text = std::fs::read_to_string(dir.join("root/TOP.sur")).unwrap();
        assert!(text.starts_with("*SURFACE, NAME=TOP, TYPE=ELEMENT\n"));

        // The deck is unchanged; a copy with the card appended reads back the same faces.
        let original = std::fs::read_to_string(dir.join("root/deck.inp")).unwrap();
        std::fs::write(dir.join("root/deck2.inp"), format!("{original}{}\n", saved.card)).unwrap();
        let mesh = read_inp_mesh(&dir.join("root/deck2.inp")).unwrap();
        let back: Vec<(u32, u8)> = faces_of_surface(&mesh, "TOP").unwrap().iter().map(|f| (f.element, f.face)).collect();
        let mesh1 = read_inp_mesh(&dir.join("root/deck.inp")).unwrap();
        let mut want: Vec<(u32, u8)> = by_normal(&exterior_faces(&mesh1), [0.0, 0.0, 1.0], 5.0)
            .into_iter()
            .map(|i| (exterior_faces(&mesh1)[i].element, exterior_faces(&mesh1)[i].face))
            .collect();
        want.sort_unstable();
        let mut back_sorted = back.clone();
        back_sorted.sort_unstable();
        assert_eq!(back_sorted, want);
        assert_eq!(back.len(), 9);

        // Overwrite refusal, then explicit overwrite.
        let err = select(&roots, MeshSelectIn { normal: Some(up()), save: Some(spec(false)), ..sel("deck.inp") }).unwrap_err();
        assert_eq!(err.code, "RESULTS_EXIST");
        assert!(select(&roots, MeshSelectIn { normal: Some(up()), save: Some(spec(true)), ..sel("deck.inp") }).is_ok());

        // Node set with a custom path.
        let nset = SaveSpec { name: "TOPN".into(), kind: SaveKind::Nset, output_path: Some("topn.nam".into()), overwrite: false };
        let out = select(&roots, MeshSelectIn { normal: Some(up()), save: Some(nset), ..sel("deck.inp") }).unwrap();
        let saved = out.saved.unwrap();
        assert_eq!((saved.file.as_str(), saved.card.as_str(), saved.lines), ("topn.nam", "*INCLUDE, INPUT=topn.nam", 3));
        let nam = std::fs::read_to_string(dir.join("root/topn.nam")).unwrap();
        assert!(nam.starts_with("*NSET, NSET=TOPN\n") && nam.contains("17, 18"), "{nam}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_rejects_bad_names_and_paths() {
        let (dir, roots) = setup(&block_deck(1, 1, 1));
        std::fs::write(dir.join("outside.inp"), "x").unwrap();
        let go = |name: &str, out: Option<&str>| {
            let save = SaveSpec { name: name.into(), kind: SaveKind::Surface, output_path: out.map(String::from), overwrite: true };
            select(&roots, MeshSelectIn { normal: Some(up()), save: Some(save), ..sel("deck.inp") })
        };
        for bad in ["", "1abc", "a b", "a.b", "ünï", &"a".repeat(81)] {
            assert_eq!(go(bad, None).unwrap_err().code, "INVALID_ARGUMENT", "name {bad:?}");
        }
        assert!(go(&"a".repeat(80), Some("long.sur")).is_ok());
        assert_eq!(go("OK", Some("../outside.sur")).unwrap_err().code, "PATH_OUTSIDE_ROOTS");
        assert_eq!(go("OK", Some("x.txt")).unwrap_err().code, "BAD_EXTENSION");
        assert_eq!(go("OK", Some("deck.inp")).unwrap_err().code, "INVALID_ARGUMENT", "the deck itself is protected");
        assert!(std::fs::read_to_string(dir.join("root/deck.inp")).unwrap().starts_with("*NODE"));

        // The deck path must be inside the roots too.
        let err = check(&roots, MeshCheckIn { path: dir.join("outside.inp").to_str().unwrap().into() }).unwrap_err();
        assert_eq!(err.code, "PATH_OUTSIDE_ROOTS");
        let err = select(&roots, MeshSelectIn { normal: Some(up()), ..sel("../outside.inp") }).unwrap_err();
        assert_eq!(err.code, "PATH_OUTSIDE_ROOTS");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[allow(clippy::print_stdout)]
    fn rotor_deck_check() {
        let deck = Path::new("/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor/lavffcyc.inp");
        if !deck.exists() {
            eprintln!("rotor deck not present, skipping");
            return;
        }
        let roots = Roots::new(vec![deck.parent().unwrap().to_path_buf()]).unwrap();
        let out = check(&roots, MeshCheckIn { path: "lavffcyc.inp".into() }).unwrap();
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
        assert!(out.nodes > 0 && out.elements > 0);
    }
}
