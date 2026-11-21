//! Contact candidates between parts of a solid mesh: exterior faces of different groups
//! (connected components, element sets or materials) that face each other within a gap.
//!
//! Pairing rule, tested in both directions for every face pair `(a, b)` of two different
//! groups (the pair is kept when either direction passes):
//! - the signed distance `d = (c_a - c_b) . n_b` of a's centroid from b's plane lies in
//!   `[-gap_max, gap_max]` (negative = penetration);
//! - the projection `c_a - d n_b` of a's centroid onto b's plane lies inside b, or within
//!   `0.1 sqrt(area_b)` of b's boundary;
//! - the normals oppose: the angle between `n_a` and `-n_b` is at most `max_angle_deg`.
//!
//! The search hashes every face into a uniform grid (cell size `gap_max` plus the median face
//! size) over the box that can contain a partner centroid, and looks up each centroid in one
//! cell, so the cost is near-linear in the number of faces.

use crate::mesh_check::components;
use crate::mesh_faces::{exterior_faces, Face};
use crate::parser::inp_cards::InpScan;
use crate::parser::inp_mesh::InpMesh;
use std::collections::{HashMap, HashSet};

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

/// How exterior faces are split into groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Partition {
    /// Connected components (elements sharing nodes), named `PART_1`, `PART_2`, ... by size,
    /// largest first.
    Component,
    /// Each element goes to the first element set in deck order that contains it; sets that
    /// contain every element are ignored. Elements in no other set form `NO_ELSET`.
    Elset,
    /// By the material of the first `*SOLID SECTION` that claims the element. Elements no
    /// section claims form `NO_MATERIAL`.
    Material,
}

/// One group of elements.
#[derive(Debug, Clone, PartialEq)]
pub struct ContactGroup {
    /// Upper-case name.
    pub name: String,
    /// Element ids, ascending.
    pub elements: Vec<u32>,
    /// Axis-aligned box of the nodes of the elements (zeros when none is found).
    pub bbox: (V3, V3),
}

fn param<'a>(card: &'a crate::parser::inp_cards::InpCard, key: &str) -> Option<&'a str> {
    card.params.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.as_deref())
}

fn name_of(raw: &str) -> String {
    raw.trim_matches(|c| c == '"' || c == '\'').trim().to_uppercase()
}

fn group_bbox(mesh: &InpMesh, elements: &[u32]) -> (V3, V3) {
    let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    let mut any = false;
    for eid in elements {
        let Some(&ei) = mesh.element_index.get(eid) else { continue };
        for nid in &mesh.elements[ei].nodes {
            let Some(&k) = mesh.node_index.get(nid) else { continue };
            let n = &mesh.nodes[k];
            any = true;
            for (i, v) in [n.x, n.y, n.z].into_iter().enumerate() {
                lo[i] = lo[i].min(v);
                hi[i] = hi[i].max(v);
            }
        }
    }
    if any { (lo, hi) } else { ([0.0; 3], [0.0; 3]) }
}

/// Split the elements of `mesh` into named groups. Every element belongs to exactly one
/// group; empty groups are not returned. Order: components by size (largest first); element
/// sets and materials in deck order, then the `NO_ELSET` / `NO_MATERIAL` remainder.
pub fn partition_groups(mesh: &InpMesh, scan: &InpScan, partition: Partition) -> Vec<ContactGroup> {
    let mut named: Vec<(String, Vec<u32>)> = Vec::new();
    match partition {
        Partition::Component => {
            for (i, ids) in components(mesh).into_iter().enumerate() {
                named.push((format!("PART_{}", i + 1), ids));
            }
        }
        Partition::Elset | Partition::Material => {
            // (group name, element set name) candidates in deck order.
            let mut order: Vec<(String, String)> = Vec::new();
            let mut seen: HashSet<(String, String)> = HashSet::new();
            for card in &scan.cards {
                let entry = match (partition, card.keyword.as_str()) {
                    (Partition::Elset, "ELEMENT") => param(card, "ELSET").map(|s| (name_of(s), name_of(s))),
                    (Partition::Elset, "ELSET") => param(card, "ELSET").map(|s| (name_of(s), name_of(s))),
                    (Partition::Material, "SOLID SECTION") => match (param(card, "ELSET"), param(card, "MATERIAL")) {
                        (Some(e), Some(m)) => Some((name_of(m), name_of(e))),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(e) = entry {
                    if seen.insert(e.clone()) {
                        order.push(e);
                    }
                }
            }
            let all = mesh.elements.len();
            let mut owner: HashMap<u32, usize> = HashMap::new();
            let mut slot: HashMap<String, usize> = HashMap::new();
            for (group, set) in order {
                let Some(s) = mesh.sets.get(&set) else { continue };
                let members: HashSet<u32> =
                    s.element_ids.iter().copied().filter(|e| mesh.element_index.contains_key(e)).collect();
                if partition == Partition::Elset && members.len() >= all {
                    continue; // a set of every element says nothing about parts
                }
                for eid in &s.element_ids {
                    if !mesh.element_index.contains_key(eid) || owner.contains_key(eid) {
                        continue;
                    }
                    let gi = *slot.entry(group.clone()).or_insert_with(|| {
                        named.push((group.clone(), Vec::new()));
                        named.len() - 1
                    });
                    owner.insert(*eid, gi);
                    named[gi].1.push(*eid);
                }
            }
            let rest: Vec<u32> = mesh.elements.iter().map(|e| e.id).filter(|id| !owner.contains_key(id)).collect();
            if !rest.is_empty() {
                let name = if partition == Partition::Elset { "NO_ELSET" } else { "NO_MATERIAL" };
                named.push((name.to_string(), rest));
            }
        }
    }
    named
        .into_iter()
        .filter(|(_, ids)| !ids.is_empty())
        .map(|(name, mut elements)| {
            elements.sort_unstable();
            elements.dedup();
            let bbox = group_bbox(mesh, &elements);
            ContactGroup { name, elements, bbox }
        })
        .collect()
}

/// Search parameters.
#[derive(Debug, Clone, PartialEq)]
pub struct ContactSearch {
    /// Largest absolute signed gap (> 0).
    pub gap_max: f64,
    /// Largest angle between `n_a` and `-n_b`, degrees.
    pub max_angle_deg: f64,
    /// Indices into the group list. Empty: all group pairs. One index: pairs that involve
    /// it. Several: pairs whose two sides are both listed.
    pub groups: Vec<usize>,
}

/// One face pair; indices into [`ContactReport::faces`], `a` on side A.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FacePair {
    pub a: usize,
    pub b: usize,
    /// Signed gap (negative = penetration); the mean of both directions when both pass.
    pub gap: f64,
}

/// Contact candidates between two groups.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupPair {
    /// Group indices, `side_a < side_b`.
    pub side_a: usize,
    pub side_b: usize,
    /// Paired faces on each side (indices into [`ContactReport::faces`]), ascending.
    pub faces_a: Vec<usize>,
    pub faces_b: Vec<usize>,
    pub area_a: f64,
    pub area_b: f64,
    pub gap_min: f64,
    pub gap_mean: f64,
    pub gap_max: f64,
    /// `min(area_a, area_b)`: the smaller of the two paired areas, each face counted once.
    pub overlap_area: f64,
    /// Median area of the paired faces on each side.
    pub median_area_a: f64,
    pub median_area_b: f64,
    /// Face pairs, sorted by gap (most penetrating first).
    pub pairs: Vec<FacePair>,
}

impl GroupPair {
    /// True when side A has the finer mesh (smaller median face area; ties go to A).
    /// CalculiX: the slave (dependent) side should be at least as fine as the master.
    pub fn slave_is_a(&self) -> bool {
        self.median_area_a <= self.median_area_b
    }
}

/// Result of [`find_contact_pairs`].
#[derive(Debug, Clone, PartialEq)]
pub struct ContactReport {
    /// Exterior faces of the selected groups, sorted by `(element, face)`.
    pub faces: Vec<Face>,
    /// Group index of each face, parallel to `faces`.
    pub face_group: Vec<usize>,
    /// Exterior face count per group (all groups, parallel to the group list).
    pub faces_per_group: Vec<usize>,
    /// Directional face tests the grid proposed (centroid inside the partner's search box).
    pub near_checks: usize,
    /// Of those, tests whose normals oppose.
    pub opposing_checks: usize,
    /// Distinct face pairs found.
    pub face_pairs: usize,
    /// Group pairs, largest overlap area first.
    pub pairs: Vec<GroupPair>,
    /// Grid cell size used.
    pub cell_size: f64,
}

/// Geometry used by the tests.
struct Geo {
    centroid: V3,
    normal: V3,
    /// In-plane edge data: (start vertex, unit in-plane edge direction) per edge.
    edges: Vec<(V3, V3)>,
    tol: f64,
    lo: V3,
    hi: V3,
    ok: bool,
}

fn geo_of(mesh: &InpMesh, f: &Face) -> Geo {
    let pts: Vec<V3> = f
        .corners
        .iter()
        .filter_map(|id| mesh.node_index.get(id).map(|&k| [mesh.nodes[k].x, mesh.nodes[k].y, mesh.nodes[k].z]))
        .collect();
    let n = f.normal;
    let ok = pts.len() == f.corners.len() && pts.len() >= 3 && f.area > 0.0 && dot(n, n) > 0.5;
    let tol = 0.1 * f.area.max(0.0).sqrt();
    let mut edges = Vec::with_capacity(pts.len());
    let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    let mut warp: f64 = 0.0;
    for (k, &p) in pts.iter().enumerate() {
        let q = pts[(k + 1) % pts.len()];
        let e = sub(q, p);
        let en = dot(e, n);
        let ip = [e[0] - en * n[0], e[1] - en * n[1], e[2] - en * n[2]];
        let l = dot(ip, ip).sqrt();
        edges.push((p, if l > 0.0 { [ip[0] / l, ip[1] / l, ip[2] / l] } else { [0.0; 3] }));
        warp = warp.max(dot(sub(p, f.centroid), n).abs());
        for i in 0..3 {
            lo[i] = lo[i].min(p[i]);
            hi[i] = hi[i].max(p[i]);
        }
    }
    Geo { centroid: f.centroid, normal: n, edges, tol, lo, hi, ok }
}

/// Directional test of a's centroid against face b: `Some(signed gap)` when the gap and
/// projection conditions hold. The normal condition is tested separately.
fn gap_onto(a: &Geo, b: &Geo, gap_max: f64) -> Option<f64> {
    let w = sub(a.centroid, b.centroid);
    let d = dot(w, b.normal);
    if d.is_nan() || d.abs() > gap_max {
        return None;
    }
    // Signed in-plane distance to each edge line, positive inside (corners run
    // counter-clockwise seen from the outward normal). det(e, p - v, n) ignores the
    // normal components of e and p - v, so no explicit projection is needed.
    for &(v, e) in &b.edges {
        if e == [0.0; 3] {
            continue;
        }
        let s = dot(cross(e, sub(a.centroid, v)), b.normal);
        if s < -b.tol {
            return None;
        }
    }
    Some(d)
}

fn opposing(a: &Geo, b: &Geo, cos_lim: f64) -> bool {
    -dot(a.normal, b.normal) >= cos_lim
}

fn cos_limit(max_angle_deg: f64) -> f64 {
    max_angle_deg.to_radians().cos() - 1e-12
}

/// Whether the group pair (ga, gb) passes the `groups` filter.
fn allowed(sel: &[usize], ga: usize, gb: usize) -> bool {
    match sel.len() {
        0 => true,
        1 => ga == sel[0] || gb == sel[0],
        _ => sel.contains(&ga) && sel.contains(&gb),
    }
}

/// Exterior faces with their group index, keeping only groups that can take part.
fn grouped_faces(mesh: &InpMesh, groups: &[ContactGroup], sel: &[usize]) -> (Vec<Face>, Vec<usize>, Vec<usize>) {
    let mut owner: HashMap<u32, usize> = HashMap::new();
    for (gi, g) in groups.iter().enumerate() {
        for e in &g.elements {
            owner.entry(*e).or_insert(gi);
        }
    }
    let mut per_group = vec![0usize; groups.len()];
    let mut faces = Vec::new();
    let mut fg = Vec::new();
    // A group takes part if some other group can pair with it under the filter.
    let active: Vec<bool> = (0..groups.len()).map(|g| (0..groups.len()).any(|h| h != g && allowed(sel, g, h))).collect();
    for f in exterior_faces(mesh) {
        let Some(&g) = owner.get(&f.element) else { continue };
        per_group[g] += 1;
        if active[g] {
            faces.push(f);
            fg.push(g);
        }
    }
    (faces, fg, per_group)
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { 0.5 * (v[n / 2 - 1] + v[n / 2]) }
}

/// `(min face index, max face index)` -> (gap with the lower index tested onto the higher,
/// gap the other way round).
type Hits = HashMap<(usize, usize), (Option<f64>, Option<f64>)>;

/// `(element, face)` of a face.
pub type FaceKey = (u32, u8);
/// One row of [`contact_face_pairs_brute_force`]: the two faces and the signed gap.
pub type FacePairRow = (FaceKey, FaceKey, f64);

/// Raw directional search on prepared faces. Returns (near, opposing, pairs) or `None` when
/// more than `budget` near checks would be needed.
fn grid_search(
    geos: &[Geo],
    fg: &[usize],
    sel: &[usize],
    gap_max: f64,
    cos_lim: f64,
    cell: f64,
    budget: usize,
) -> Option<(usize, usize, Hits)> {
    let key = |p: V3| -> [i64; 3] { [(p[0] / cell).floor() as i64, (p[1] / cell).floor() as i64, (p[2] / cell).floor() as i64] };
    let mut grid: HashMap<[i64; 3], Vec<u32>> = HashMap::new();
    for (j, g) in geos.iter().enumerate() {
        if !g.ok {
            continue;
        }
        let r = gap_max + g.tol + (0..g.edges.len()).map(|k| dot(sub(g.edges[k].0, g.centroid), g.normal).abs()).fold(0.0, f64::max);
        let lo = key([g.lo[0] - r, g.lo[1] - r, g.lo[2] - r]);
        let hi = key([g.hi[0] + r, g.hi[1] + r, g.hi[2] + r]);
        for x in lo[0]..=hi[0] {
            for y in lo[1]..=hi[1] {
                for z in lo[2]..=hi[2] {
                    grid.entry([x, y, z]).or_default().push(j as u32);
                }
            }
        }
    }
    let (mut near, mut opp) = (0usize, 0usize);
    // (min, max) index -> (gap with a = min tested onto max, gap with a = max onto min)
    let mut hits: Hits = HashMap::new();
    for (i, a) in geos.iter().enumerate() {
        if !a.ok {
            continue;
        }
        let Some(list) = grid.get(&key(a.centroid)) else { continue };
        for &j in list {
            let j = j as usize;
            if fg[i] == fg[j] || !allowed(sel, fg[i], fg[j]) {
                continue;
            }
            let b = &geos[j];
            // Exact box test (the cell is coarser than the box).
            let r = gap_max + b.tol + (0..b.edges.len()).map(|k| dot(sub(b.edges[k].0, b.centroid), b.normal).abs()).fold(0.0, f64::max);
            if (0..3).any(|k| a.centroid[k] < b.lo[k] - r || a.centroid[k] > b.hi[k] + r) {
                continue;
            }
            near += 1;
            if near > budget {
                return None;
            }
            if !opposing(a, b, cos_lim) {
                continue;
            }
            opp += 1;
            if let Some(d) = gap_onto(a, b, gap_max) {
                let e = hits.entry((i.min(j), i.max(j))).or_insert((None, None));
                if i < j { e.0 = Some(d) } else { e.1 = Some(d) }
            }
        }
    }
    Some((near, opp, hits))
}

fn merged_gap(h: (Option<f64>, Option<f64>)) -> f64 {
    match h {
        (Some(x), Some(y)) => 0.5 * (x + y),
        (Some(x), None) | (None, Some(x)) => x,
        (None, None) => f64::NAN,
    }
}

fn grid_cell(geos: &[Geo], faces: &[Face], gap_max: f64) -> f64 {
    let med = median(geos.iter().zip(faces).filter(|(g, _)| g.ok).map(|(_, f)| f.area.sqrt()).collect());
    let c = gap_max + med;
    if c > 0.0 && c.is_finite() { c } else { 1.0 }
}

/// Find contact candidates between `groups` (from [`partition_groups`]).
pub fn find_contact_pairs(mesh: &InpMesh, groups: &[ContactGroup], opts: &ContactSearch) -> ContactReport {
    let (faces, fg, faces_per_group) = grouped_faces(mesh, groups, &opts.groups);
    let geos: Vec<Geo> = faces.iter().map(|f| geo_of(mesh, f)).collect();
    let cell = grid_cell(&geos, &faces, opts.gap_max);
    let (near, opp, hits) =
        grid_search(&geos, &fg, &opts.groups, opts.gap_max, cos_limit(opts.max_angle_deg), cell, usize::MAX).unwrap_or_default();
    let face_pairs = hits.len();

    let mut by_groups: HashMap<(usize, usize), Vec<FacePair>> = HashMap::new();
    for ((i, j), h) in hits {
        let gap = merged_gap(h);
        let (a, b) = if fg[i] < fg[j] { (i, j) } else { (j, i) };
        by_groups.entry((fg[a], fg[b])).or_default().push(FacePair { a, b, gap });
    }
    let mut pairs: Vec<GroupPair> = by_groups
        .into_iter()
        .map(|((ga, gb), mut fp)| {
            fp.sort_by(|x, y| x.gap.total_cmp(&y.gap).then(x.a.cmp(&y.a)).then(x.b.cmp(&y.b)));
            let mut fa: Vec<usize> = fp.iter().map(|p| p.a).collect();
            let mut fb: Vec<usize> = fp.iter().map(|p| p.b).collect();
            fa.sort_unstable();
            fa.dedup();
            fb.sort_unstable();
            fb.dedup();
            let area_a: f64 = fa.iter().map(|&i| faces[i].area).sum();
            let area_b: f64 = fb.iter().map(|&i| faces[i].area).sum();
            let gaps: Vec<f64> = fp.iter().map(|p| p.gap).collect();
            GroupPair {
                side_a: ga,
                side_b: gb,
                area_a,
                area_b,
                gap_min: gaps.iter().copied().fold(f64::INFINITY, f64::min),
                gap_max: gaps.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                gap_mean: gaps.iter().sum::<f64>() / gaps.len() as f64,
                overlap_area: area_a.min(area_b),
                median_area_a: median(fa.iter().map(|&i| faces[i].area).collect()),
                median_area_b: median(fb.iter().map(|&i| faces[i].area).collect()),
                faces_a: fa,
                faces_b: fb,
                pairs: fp,
            }
        })
        .collect();
    pairs.sort_by(|x, y| y.overlap_area.total_cmp(&x.overlap_area).then(x.side_a.cmp(&y.side_a)).then(x.side_b.cmp(&y.side_b)));
    ContactReport { faces, face_group: fg, faces_per_group, near_checks: near, opposing_checks: opp, face_pairs, pairs, cell_size: cell }
}

/// Reference O(n²) search with the same rule, for tests: distinct face pairs as
/// `((element, face), (element, face), gap)` with the first face of the lower group index,
/// sorted.
pub fn contact_face_pairs_brute_force(
    mesh: &InpMesh,
    groups: &[ContactGroup],
    opts: &ContactSearch,
) -> Vec<FacePairRow> {
    let (faces, fg, _) = grouped_faces(mesh, groups, &opts.groups);
    let geos: Vec<Geo> = faces.iter().map(|f| geo_of(mesh, f)).collect();
    let cos_lim = cos_limit(opts.max_angle_deg);
    let mut out = Vec::new();
    for i in 0..faces.len() {
        for j in i + 1..faces.len() {
            if fg[i] == fg[j] || !allowed(&opts.groups, fg[i], fg[j]) || !geos[i].ok || !geos[j].ok {
                continue;
            }
            if !opposing(&geos[i], &geos[j], cos_lim) {
                continue;
            }
            let h = (gap_onto(&geos[i], &geos[j], opts.gap_max), gap_onto(&geos[j], &geos[i], opts.gap_max));
            if h == (None, None) {
                continue;
            }
            let (a, b) = if fg[i] < fg[j] { (i, j) } else { (j, i) };
            out.push(((faces[a].element, faces[a].face), (faces[b].element, faces[b].face), merged_gap(h)));
        }
    }
    out.sort_by_key(|x| (x.0, x.1));
    out
}

/// When nothing is found within `opts.gap_max`, the smallest signed gap between any two
/// groups found by doubling the gap up to the model size: `(group a, group b, gap)`.
/// `None` when no opposing faces are found, or the search would exceed about 20 million
/// face tests.
pub fn nearest_opposing_gap(mesh: &InpMesh, groups: &[ContactGroup], opts: &ContactSearch) -> Option<(usize, usize, f64)> {
    let (faces, fg, _) = grouped_faces(mesh, groups, &opts.groups);
    if faces.is_empty() {
        return None;
    }
    let geos: Vec<Geo> = faces.iter().map(|f| geo_of(mesh, f)).collect();
    let (mut lo, mut hi) = ([f64::INFINITY; 3], [f64::NEG_INFINITY; 3]);
    for g in &geos {
        for k in 0..3 {
            lo[k] = lo[k].min(g.lo[k]);
            hi[k] = hi[k].max(g.hi[k]);
        }
    }
    let diag = dot(sub(hi, lo), sub(hi, lo)).sqrt();
    let cos_lim = cos_limit(opts.max_angle_deg);
    let mut gap = opts.gap_max.max(diag * 1e-6) * 2.0;
    let mut budget = 20_000_000usize;
    while gap <= 2.0 * diag {
        let cell = grid_cell(&geos, &faces, gap);
        let (near, _, hits) = grid_search(&geos, &fg, &opts.groups, gap, cos_lim, cell, budget)?;
        budget = budget.checked_sub(near)?;
        let best = hits
            .into_iter()
            .map(|((i, j), h)| (fg[i].min(fg[j]), fg[i].max(fg[j]), merged_gap(h)))
            .min_by(|x, y| x.2.abs().total_cmp(&y.2.abs()));
        if best.is_some() {
            return best;
        }
        gap *= 2.0;
    }
    None
}
