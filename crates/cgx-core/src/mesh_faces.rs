//! Exterior faces of solid meshes read from `.inp` decks, face selection helpers and
//! `*SURFACE` / `*NSET` card writers.

use crate::parser::inp_mesh::InpMesh;
use crate::ElementType;
use std::collections::{HashMap, HashSet, VecDeque};

/// One element face with geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct Face {
    pub element: u32,
    /// CalculiX face number, 1-based (`S1`..`S6`).
    pub face: u8,
    /// Corner node ids, ordered so the right-hand normal points out of the element.
    pub corners: Vec<u32>,
    pub centroid: [f64; 3],
    /// Unit outward normal.
    pub normal: [f64; 3],
    pub area: f64,
}

// Face corner tables (0-based indices into the element's node list; mid-side nodes are
// ignored). Verified against ccx `src/addimdnodedload.f` (data statements `ifaceq`,
// `ifacet`, `ifacew`, lines 40-54; the same tables recur in dload.f, extsurface.f, ...).
// ccx lists each face counter-clockwise seen from inside, i.e. with outward right-hand
// normal: hex S1 = 4-3-2-1, S2 = 5-6-7-8, S3 = 1-2-6-5, S4 = 2-3-7-6, S5 = 3-4-8-7,
// S6 = 4-1-5-8; tet S1 = 1-3-2, S2 = 1-2-4, S3 = 2-3-4, S4 = 1-4-3; wedge S1 = 1-3-2,
// S2 = 4-5-6, S3 = 1-2-5-4, S4 = 2-3-6-5, S5 = 4-6-3-1. The node sets equal the reference
// list; only the traversal direction differs (reversed for S1, S2 and the tet/wedge faces).
const HEX_FACES: [&[usize]; 6] = [
    &[3, 2, 1, 0],
    &[4, 5, 6, 7],
    &[0, 1, 5, 4],
    &[1, 2, 6, 5],
    &[2, 3, 7, 6],
    &[3, 0, 4, 7],
];
const TET_FACES: [&[usize]; 4] = [&[0, 2, 1], &[0, 1, 3], &[1, 2, 3], &[0, 3, 2]];
const WEDGE_FACES: [&[usize]; 5] = [&[0, 2, 1], &[3, 4, 5], &[0, 1, 4, 3], &[1, 2, 5, 4], &[3, 5, 2, 0]];

pub(crate) fn face_table(t: ElementType) -> Option<(&'static [&'static [usize]], usize)> {
    match t {
        ElementType::C3D8 | ElementType::C3D20 | ElementType::C3D20R => Some((&HEX_FACES, 8)),
        ElementType::C3D4 | ElementType::C3D10 => Some((&TET_FACES, 4)),
        ElementType::C3D6 | ElementType::C3D15 => Some((&WEDGE_FACES, 6)),
        _ => None,
    }
}

// Full face tables including mid-side nodes (0-based), from the same `data` statements
// (`ifaceq`, `ifacet`, `ifacew`) as the corner tables above. The first 3 or 4 entries are
// the corners in the same order as the corner tables; the rest are the mid-side nodes.
const HEX_FACES_FULL: [&[usize]; 6] = [
    &[3, 2, 1, 0, 10, 9, 8, 11],
    &[4, 5, 6, 7, 12, 13, 14, 15],
    &[0, 1, 5, 4, 8, 17, 12, 16],
    &[1, 2, 6, 5, 9, 18, 13, 17],
    &[2, 3, 7, 6, 10, 19, 14, 18],
    &[3, 0, 4, 7, 11, 16, 15, 19],
];
const TET_FACES_FULL: [&[usize]; 4] = [&[0, 2, 1, 6, 5, 4], &[0, 1, 3, 4, 8, 7], &[1, 2, 3, 5, 9, 8], &[0, 3, 2, 7, 9, 6]];
const WEDGE_FACES_FULL: [&[usize]; 5] = [
    &[0, 2, 1, 8, 7, 6],
    &[3, 4, 5, 9, 10, 11],
    &[0, 1, 4, 3, 6, 13, 9, 12],
    &[1, 2, 5, 4, 7, 14, 10, 13],
    &[3, 5, 2, 0, 11, 14, 8, 12],
];

/// All nodes of the element that lie on `face`: the corners plus, for quadratic elements
/// (C3D20/C3D20R: 8 nodes, C3D10: 6, C3D15: 6 or 8), the mid-side nodes, using the full ccx
/// face tables. Linear elements return the corners. Unknown elements or faces give an empty
/// list; an element with fewer nodes than its type needs falls back to the corner tables.
pub fn face_nodes(mesh: &InpMesh, face: &Face) -> Vec<u32> {
    let Some(&ei) = mesh.element_index.get(&face.element) else { return Vec::new() };
    let el = &mesh.elements[ei];
    let Some((corner_table, ncorner)) = face_table(el.elem_type) else { return Vec::new() };
    let Some(fi) = (face.face as usize).checked_sub(1) else { return Vec::new() };
    if el.nodes.len() < ncorner || fi >= corner_table.len() {
        return Vec::new();
    }
    let quadratic = match el.elem_type {
        ElementType::C3D20 | ElementType::C3D20R => el.nodes.len() >= 20,
        ElementType::C3D10 => el.nodes.len() >= 10,
        ElementType::C3D15 => el.nodes.len() >= 15,
        _ => false,
    };
    let local: &[usize] = if quadratic {
        match el.elem_type {
            ElementType::C3D20 | ElementType::C3D20R => HEX_FACES_FULL[fi],
            ElementType::C3D10 => TET_FACES_FULL[fi],
            _ => WEDGE_FACES_FULL[fi],
        }
    } else {
        corner_table[fi]
    };
    local.iter().map(|&i| el.nodes[i]).collect()
}

type V3 = [f64; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn cross(a: V3, b: V3) -> V3 {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn norm(a: V3) -> f64 {
    dot(a, a).sqrt()
}

/// Build the geometry of face `face` (1-based) of element index `ei`, or `None` when the
/// element is not a supported solid, the face number is invalid or a node is missing.
fn build_face(mesh: &InpMesh, ei: usize, face: u8) -> Option<Face> {
    let el = &mesh.elements[ei];
    let (table, ncorner) = face_table(el.elem_type)?;
    let local = table.get((face as usize).checked_sub(1)?)?;
    if el.nodes.len() < ncorner {
        return None;
    }
    let pos = |id: u32| -> Option<V3> {
        let n = &mesh.nodes[*mesh.node_index.get(&id)?];
        Some([n.x, n.y, n.z])
    };
    let mut ecen = [0.0; 3];
    for &id in &el.nodes[..ncorner] {
        let p = pos(id)?;
        for k in 0..3 {
            ecen[k] += p[k] / ncorner as f64;
        }
    }
    let mut corners: Vec<u32> = local.iter().map(|&i| el.nodes[i]).collect();
    let mut pts: Vec<V3> = corners.iter().map(|&id| pos(id)).collect::<Option<_>>()?;
    let mut fcen = [0.0; 3];
    for p in &pts {
        for k in 0..3 {
            fcen[k] += p[k] / pts.len() as f64;
        }
    }
    // Area vector: triangle directly, quad as the sum of triangles (0,1,2) and (0,2,3).
    let tri = |a: V3, b: V3, c: V3| cross(sub(b, a), sub(c, a));
    let mut avec = tri(pts[0], pts[1], pts[2]);
    let mut area = norm(avec) / 2.0;
    if pts.len() == 4 {
        let t2 = tri(pts[0], pts[2], pts[3]);
        area += norm(t2) / 2.0;
        avec = [avec[0] + t2[0], avec[1] + t2[1], avec[2] + t2[2]];
    }
    let len = norm(avec);
    let mut normal = if len > 0.0 { [avec[0] / len, avec[1] / len, avec[2] / len] } else { [0.0; 3] };
    // Robust orientation: flip when the normal points toward the element centroid.
    if dot(normal, sub(fcen, ecen)) < 0.0 {
        normal = [-normal[0], -normal[1], -normal[2]];
        corners.reverse();
        pts.reverse();
    }
    Some(Face { element: el.id, face, corners, centroid: fcen, normal, area })
}

/// All exterior faces of the solid elements (hex, tet, wedge families), sorted by
/// `(element, face)`. A face is exterior when its corner-node set occurs exactly once in
/// the mesh. Shells and beams are ignored.
pub fn exterior_faces(mesh: &InpMesh) -> Vec<Face> {
    let mut count: HashMap<[u32; 4], u32> = HashMap::new();
    let key_of = |ei: usize, f: usize| -> Option<[u32; 4]> {
        let el = &mesh.elements[ei];
        let (table, ncorner) = face_table(el.elem_type)?;
        if el.nodes.len() < ncorner {
            return None;
        }
        let mut ids: Vec<u32> = table[f].iter().map(|&i| el.nodes[i]).collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() < 3 {
            return None; // collapsed face
        }
        let mut key = [u32::MAX; 4];
        key[..ids.len()].copy_from_slice(&ids);
        Some(key)
    };
    for ei in 0..mesh.elements.len() {
        if let Some((table, _)) = face_table(mesh.elements[ei].elem_type) {
            for f in 0..table.len() {
                if let Some(k) = key_of(ei, f) {
                    *count.entry(k).or_insert(0) += 1;
                }
            }
        }
    }
    let mut out = Vec::new();
    for ei in 0..mesh.elements.len() {
        if let Some((table, _)) = face_table(mesh.elements[ei].elem_type) {
            for f in 0..table.len() {
                if key_of(ei, f).is_some_and(|k| count[&k] == 1) {
                    if let Some(face) = build_face(mesh, ei, f as u8 + 1) {
                        out.push(face);
                    }
                }
            }
        }
    }
    out.sort_by_key(|f| (f.element, f.face));
    out
}

/// Faces of the element-face surface `name` (case-insensitive) in deck order. Entries that
/// reference unknown elements, non-solid elements or missing nodes are skipped.
pub fn faces_of_surface(mesh: &InpMesh, name: &str) -> Option<Vec<Face>> {
    let entries = mesh.surfaces.get(&name.to_uppercase())?;
    Some(
        entries
            .iter()
            .filter_map(|&(eid, f)| build_face(mesh, *mesh.element_index.get(&eid)?, f))
            .collect(),
    )
}

fn unit(v: V3) -> V3 {
    let l = norm(v);
    if l > 0.0 { [v[0] / l, v[1] / l, v[2] / l] } else { v }
}

fn cos_limit(max_angle_deg: f64) -> f64 {
    max_angle_deg.to_radians().cos() - 1e-12
}

/// Indices of faces whose normal is within `max_angle_deg` of `direction`.
pub fn by_normal(faces: &[Face], direction: [f64; 3], max_angle_deg: f64) -> Vec<usize> {
    let d = unit(direction);
    let lim = cos_limit(max_angle_deg);
    (0..faces.len()).filter(|&i| dot(faces[i].normal, d) >= lim).collect()
}

/// Indices of faces whose corner nodes all lie within `tol` of the plane through `point`
/// with the given `normal`.
pub fn on_plane(mesh: &InpMesh, faces: &[Face], point: [f64; 3], normal: [f64; 3], tol: f64) -> Vec<usize> {
    let n = unit(normal);
    (0..faces.len())
        .filter(|&i| {
            faces[i].corners.iter().all(|id| {
                mesh.node_index.get(id).is_some_and(|&k| {
                    let nd = &mesh.nodes[k];
                    dot(sub([nd.x, nd.y, nd.z], point), n).abs() <= tol
                })
            })
        })
        .collect()
}

/// Indices of faces whose centroid lies inside the box `[min, max]` (inclusive).
pub fn in_box(faces: &[Face], min: [f64; 3], max: [f64; 3]) -> Vec<usize> {
    (0..faces.len())
        .filter(|&i| (0..3).all(|k| faces[i].centroid[k] >= min[k] && faces[i].centroid[k] <= max[k]))
        .collect()
}

/// Indices of faces whose corner nodes all lie at distance `radius` (+- `tol`) from the
/// axis through `axis_point` with direction `axis_dir`.
pub fn on_cylinder(
    mesh: &InpMesh,
    faces: &[Face],
    axis_point: [f64; 3],
    axis_dir: [f64; 3],
    radius: f64,
    tol: f64,
) -> Vec<usize> {
    let d = unit(axis_dir);
    (0..faces.len())
        .filter(|&i| {
            faces[i].corners.iter().all(|id| {
                mesh.node_index.get(id).is_some_and(|&k| {
                    let nd = &mesh.nodes[k];
                    let r = sub([nd.x, nd.y, nd.z], axis_point);
                    let along = dot(r, d);
                    let radial = [r[0] - along * d[0], r[1] - along * d[1], r[2] - along * d[2]];
                    (norm(radial) - radius).abs() <= tol
                })
            })
        })
        .collect()
}

/// Indices of faces whose centroid lies within `radius` of `point`.
pub fn near_point(faces: &[Face], point: [f64; 3], radius: f64) -> Vec<usize> {
    (0..faces.len()).filter(|&i| norm(sub(faces[i].centroid, point)) <= radius).collect()
}

/// Index of the face whose centroid is closest to `point` (first on ties).
pub fn nearest_face(faces: &[Face], point: [f64; 3]) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, f) in faces.iter().enumerate() {
        let d = norm(sub(f.centroid, point));
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((i, d));
        }
    }
    best.map(|b| b.0)
}

/// Flood fill from `seed` over faces that share an edge (two corner nodes), crossing an
/// edge only when the angle between the two normals is at most `max_angle_deg`. Returns
/// the sorted indices including the seed; empty when `seed` is out of range.
pub fn connected_patch(faces: &[Face], seed: usize, max_angle_deg: f64) -> Vec<usize> {
    if seed >= faces.len() {
        return Vec::new();
    }
    let mut edges: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
    for (i, f) in faces.iter().enumerate() {
        let n = f.corners.len();
        for k in 0..n {
            let (a, b) = (f.corners[k], f.corners[(k + 1) % n]);
            edges.entry((a.min(b), a.max(b))).or_default().push(i);
        }
    }
    let lim = cos_limit(max_angle_deg);
    let mut seen: HashSet<usize> = HashSet::new();
    seen.insert(seed);
    let mut queue = VecDeque::from([seed]);
    while let Some(i) = queue.pop_front() {
        let f = &faces[i];
        let n = f.corners.len();
        for k in 0..n {
            let (a, b) = (f.corners[k], f.corners[(k + 1) % n]);
            for &j in &edges[&(a.min(b), a.max(b))] {
                if !seen.contains(&j) && dot(f.normal, faces[j].normal) >= lim {
                    seen.insert(j);
                    queue.push_back(j);
                }
            }
        }
    }
    let mut out: Vec<usize> = seen.into_iter().collect();
    out.sort_unstable();
    out
}

/// `*SURFACE, NAME=<name>, TYPE=ELEMENT` card with one `element, S<n>` line per face.
pub fn write_surface_card(name: &str, faces: &[&Face]) -> String {
    let mut s = format!("*SURFACE, NAME={name}, TYPE=ELEMENT\n");
    for f in faces {
        s.push_str(&format!("{}, S{}\n", f.element, f.face));
    }
    s
}

/// `*NSET, NSET=<name>` card, ids sorted and de-duplicated, ten per line separated by
/// `, ` with no trailing comma (CalculiX reads a data line as complete at its end; the
/// `.nam` files written by CGX use one id per line with a trailing comma, which is also
/// accepted, but is not required).
pub fn write_nset_card(name: &str, node_ids: &[u32]) -> String {
    let mut ids = node_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut s = format!("*NSET, NSET={name}\n");
    for chunk in ids.chunks(10) {
        let line: Vec<String> = chunk.iter().map(|i| i.to_string()).collect();
        s.push_str(&line.join(", "));
        s.push('\n');
    }
    s
}
