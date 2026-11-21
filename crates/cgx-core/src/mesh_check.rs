//! Pre-solve sanity checks on a mesh read from an `.inp` deck: connectivity, element volume
//! and shape, mass properties and undefined set or material references.
//!
//! Everything here is a pure function over [`InpMesh`] (plus [`InpScan`] where deck cards are
//! needed). Volumes and masses use corner nodes only; values are in deck units.

use crate::mesh_faces::face_table;
use crate::parser::inp_cards::{read_card_data, InpCard, InpScan};
use crate::parser::inp_mesh::InpMesh;
use std::collections::{BTreeSet, HashMap, HashSet};

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
fn dist(a: V3, b: V3) -> f64 {
    let d = sub(a, b);
    dot(d, d).sqrt()
}

fn pos(mesh: &InpMesh, id: u32) -> Option<V3> {
    let n = &mesh.nodes[*mesh.node_index.get(&id)?];
    Some([n.x, n.y, n.z])
}

/// Corner-node coordinates of element `ei` for solids, with the face table; `None` for
/// shells, beams, unknown types, short node lists or missing nodes.
fn solid_corners(mesh: &InpMesh, ei: usize) -> Option<(Vec<V3>, &'static [&'static [usize]])> {
    let el = mesh.elements.get(ei)?;
    let (table, ncorner) = face_table(el.elem_type)?;
    if el.nodes.len() < ncorner {
        return None;
    }
    let pts: Vec<V3> = el.nodes[..ncorner].iter().map(|&id| pos(mesh, id)).collect::<Option<_>>()?;
    Some((pts, table))
}

/// Signed volume of the solid element at `element_index` (an index into `mesh.elements`),
/// by the divergence theorem over the element's own faces, triangulated on corner nodes.
///
/// Positive for a correctly oriented element, negative for an inverted one. Exact for
/// elements with planar faces; for curved (warped or quadratic-edge) faces it is an
/// approximation because mid-side nodes are ignored and each quadrilateral face is split into
/// four triangles about its mean point. Shells, beams and unknown types give `None`.
pub fn element_volume(mesh: &InpMesh, element_index: usize) -> Option<f64> {
    let (pts, table) = solid_corners(mesh, element_index)?;
    // Shift to the element centroid for precision.
    let c = {
        let mut c = [0.0; 3];
        for p in &pts {
            for k in 0..3 {
                c[k] += p[k] / pts.len() as f64;
            }
        }
        c
    };
    let tri = |a: V3, b: V3, d: V3| dot(a, cross(b, d)) / 6.0;
    let mut vol = 0.0;
    for f in table {
        let q: Vec<V3> = f.iter().map(|&i| sub(pts[i], c)).collect();
        if q.len() == 3 {
            vol += tri(q[0], q[1], q[2]);
        } else {
            let m = [
                (q[0][0] + q[1][0] + q[2][0] + q[3][0]) / 4.0,
                (q[0][1] + q[1][1] + q[2][1] + q[3][1]) / 4.0,
                (q[0][2] + q[1][2] + q[2][2] + q[3][2]) / 4.0,
            ];
            for k in 0..4 {
                vol += tri(m, q[k], q[(k + 1) % 4]);
            }
        }
    }
    Some(vol)
}

/// Groups of element ids connected through shared nodes, largest first (ties: smallest id
/// first). Ids inside a group are sorted.
pub fn components(mesh: &InpMesh) -> Vec<Vec<u32>> {
    let n = mesh.elements.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    let mut owner: HashMap<u32, usize> = HashMap::new();
    for (i, el) in mesh.elements.iter().enumerate() {
        for &nd in &el.nodes {
            match owner.get(&nd) {
                Some(&j) => {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    if a != b {
                        parent[a] = b;
                    }
                }
                None => {
                    owner.insert(nd, i);
                }
            }
        }
    }
    let mut groups: HashMap<usize, Vec<u32>> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        groups.entry(r).or_default().push(mesh.elements[i].id);
    }
    let mut out: Vec<Vec<u32>> = groups.into_values().collect();
    for g in &mut out {
        g.sort_unstable();
    }
    out.sort_by(|a, b| b.len().cmp(&a.len()).then(a[0].cmp(&b[0])));
    out
}

/// Ids (ascending) of nodes that no element uses.
pub fn free_nodes(mesh: &InpMesh) -> Vec<u32> {
    let used: HashSet<u32> = mesh.elements.iter().flat_map(|e| e.nodes.iter().copied()).collect();
    let mut out: Vec<u32> = mesh.nodes.iter().map(|n| n.id).filter(|id| !used.contains(id)).collect();
    out.sort_unstable();
    out
}

/// Node usage outside elements: nodes no element uses, split into reference nodes (named by
/// a deck card) and truly free nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeUsage {
    /// Nodes that no element and no deck card uses, ascending.
    pub free: Vec<u32>,
    /// Nodes in no element that a card uses, ascending by id, with the keyword of the first
    /// card (in deck order) that uses each.
    pub reference: Vec<(u32, String)>,
}

fn node_id(field: &str) -> Option<u32> {
    field.trim().parse::<u32>().ok()
}

/// Node ids a card names outside element definitions, in order of appearance.
///
/// Per the CalculiX manual: `*RIGID BODY` parameters `REF NODE` and `ROT NODE`; `*COUPLING`
/// parameter `REF NODE`; `*EQUATION` data (a term-count line, then `node, dof, coefficient`
/// triples, up to four terms per line; a node set in the first term is skipped; no data is
/// read with `REMOVE`); `*MPC` data (a name, then node ids, zeros discarded); `*BOUNDARY` and
/// `*CLOAD` lines whose first field is a numeric node id.
fn card_node_ids(card: &InpCard) -> Vec<u32> {
    let mut out = Vec::new();
    match card.keyword.as_str() {
        "RIGID BODY" => {
            for key in ["REF NODE", "ROT NODE"] {
                out.extend(param(card, key).and_then(node_id));
            }
        }
        "COUPLING" => out.extend(param(card, "REF NODE").and_then(node_id)),
        "EQUATION" => {
            if card.params.iter().any(|(k, _)| k == "REMOVE" || k == "REMOVE ALL") {
                return out;
            }
            let Ok(data) = read_card_data(card, card.data_lines) else { return out };
            // `need` = fields still to read for the current equation; 0 = next line is a count.
            let mut need = 0usize;
            let mut index = 0usize;
            for line in &data {
                let fields: Vec<&str> = line.split(',').map(str::trim).filter(|f| !f.is_empty()).collect();
                if need == 0 {
                    let Some(terms) = fields.first().and_then(|f| f.parse::<usize>().ok()) else { continue };
                    need = terms * 3;
                    index = 0;
                    if fields.len() > 1 {
                        // Tolerate a count followed by terms on the same line.
                        for f in &fields[1..] {
                            if index.is_multiple_of(3) {
                                out.extend(node_id(f));
                            }
                            index += 1;
                        }
                        need = need.saturating_sub(fields.len() - 1);
                    }
                    continue;
                }
                for f in &fields {
                    if need == 0 {
                        break;
                    }
                    if index.is_multiple_of(3) {
                        out.extend(node_id(f));
                    }
                    index += 1;
                    need -= 1;
                }
            }
        }
        "MPC" => {
            let Ok(data) = read_card_data(card, card.data_lines) else { return out };
            for (i, line) in data.iter().enumerate() {
                for (j, f) in line.split(',').enumerate() {
                    if i == 0 && j == 0 {
                        continue; // MPC name
                    }
                    out.extend(node_id(f).filter(|&n| n != 0));
                }
            }
        }
        "BOUNDARY" | "CLOAD" => {
            let Ok(data) = read_card_data(card, card.data_lines) else { return out };
            for line in &data {
                out.extend(line.split(',').next().and_then(node_id));
            }
        }
        _ => {}
    }
    out
}

/// Split the nodes that no element uses into reference nodes and free nodes, see
/// [`NodeUsage`] and the card list in the module's `card_node_ids`.
pub fn node_usage(mesh: &InpMesh, scan: &InpScan) -> NodeUsage {
    let unused: HashSet<u32> = free_nodes(mesh).into_iter().collect();
    let mut reference: std::collections::BTreeMap<u32, String> = std::collections::BTreeMap::new();
    for card in &scan.cards {
        for id in card_node_ids(card) {
            if unused.contains(&id) {
                reference.entry(id).or_insert_with(|| card.keyword.clone());
            }
        }
    }
    let free = free_nodes(mesh).into_iter().filter(|id| !reference.contains_key(id)).collect();
    NodeUsage { free, reference: reference.into_iter().collect() }
}

/// Connection cards that can join separate parts, in this fixed order, those present in the
/// deck: RIGID BODY, TIE, CONTACT PAIR, EQUATION, MPC, COUPLING.
pub fn joining_cards(scan: &InpScan) -> Vec<String> {
    ["RIGID BODY", "TIE", "CONTACT PAIR", "EQUATION", "MPC", "COUPLING"]
        .into_iter()
        .filter(|k| scan.cards.iter().any(|c| c.keyword == *k))
        .map(String::from)
        .collect()
}

/// Element quality summary, solids only.
#[derive(Debug, Clone, PartialEq)]
pub struct QualityReport {
    /// Solid elements with volume <= 0 (inverted or collapsed), ascending ids.
    pub non_positive_volume: Vec<u32>,
    /// Up to 10 `(element id, aspect)` pairs, worst first. Aspect is the longest over the
    /// shortest corner-node edge of the element (infinite when an edge has zero length).
    pub worst_aspect: Vec<(u32, f64)>,
    /// Largest aspect over all solids; 0 when there are none.
    pub max_aspect: f64,
}

/// Compute the [`QualityReport`] of a mesh.
pub fn quality(mesh: &InpMesh) -> QualityReport {
    let mut bad = Vec::new();
    let mut aspects: Vec<(u32, f64)> = Vec::new();
    for (ei, el) in mesh.elements.iter().enumerate() {
        let Some((pts, table)) = solid_corners(mesh, ei) else { continue };
        if element_volume(mesh, ei).is_some_and(|v| v <= 0.0) {
            bad.push(el.id);
        }
        let (mut lo, mut hi) = (f64::INFINITY, 0.0f64);
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        for f in table {
            for k in 0..f.len() {
                let (a, b) = (f[k], f[(k + 1) % f.len()]);
                if seen.insert((a.min(b), a.max(b))) {
                    let l = dist(pts[a], pts[b]);
                    lo = lo.min(l);
                    hi = hi.max(l);
                }
            }
        }
        let aspect = if lo > 0.0 { hi / lo } else { f64::INFINITY };
        aspects.push((el.id, aspect));
    }
    bad.sort_unstable();
    let max_aspect = aspects.iter().map(|a| a.1).fold(0.0, f64::max);
    aspects.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    aspects.truncate(10);
    QualityReport { non_positive_volume: bad, worst_aspect: aspects, max_aspect }
}

/// Mass of one `*SOLID SECTION`.
#[derive(Debug, Clone, PartialEq)]
pub struct MaterialMass {
    pub material: String,
    pub density: Option<f64>,
    pub elset: String,
    /// Solid elements of the set not already claimed by an earlier section.
    pub elements: usize,
    pub volume: f64,
    pub mass: Option<f64>,
}

/// Volume, mass and centre of gravity of the solid elements.
#[derive(Debug, Clone, PartialEq)]
pub struct MassReport {
    /// Signed sum of all solid element volumes.
    pub volume: f64,
    /// One entry per `*SOLID SECTION` card, in deck order.
    pub by_material: Vec<MaterialMass>,
    /// `None` if any solid element has no density (or there is no solid element).
    pub mass: Option<f64>,
    /// Mass-weighted; each element centroid is the mean of its corner nodes.
    pub centre_of_gravity: Option<[f64; 3]>,
    /// Solid elements not covered by any `*SOLID SECTION`.
    pub unassigned_elements: usize,
    /// Axis-aligned box of all nodes (zeros when there are none).
    pub bounding_box: ([f64; 3], [f64; 3]),
}

fn param<'a>(card: &'a InpCard, key: &str) -> Option<&'a str> {
    card.params.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.as_deref())
}

fn name_of(raw: &str) -> String {
    raw.trim_matches(|c| c == '"' || c == '\'').trim().to_uppercase()
}

fn parse_real(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.replace(['D', 'd'], "E").parse().ok()
}

/// `*MATERIAL` name (upper-case) -> density from the `*DENSITY` card that follows it.
/// `None` when the material has no readable density.
fn material_densities(scan: &InpScan) -> HashMap<String, Option<f64>> {
    let mut out: HashMap<String, Option<f64>> = HashMap::new();
    let mut current: Option<String> = None;
    for card in &scan.cards {
        match card.keyword.as_str() {
            "MATERIAL" => {
                current = param(card, "NAME").map(name_of);
                if let Some(n) = &current {
                    out.entry(n.clone()).or_insert(None);
                }
            }
            "DENSITY" => {
                if let Some(n) = &current {
                    let first = read_card_data(card, 1)
                        .ok()
                        .and_then(|d| d.first().and_then(|l| l.split(',').next().and_then(parse_real)));
                    if first.is_some() {
                        out.insert(n.clone(), first);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Volume, mass per material section, mass and centre of gravity.
///
/// A `*SOLID SECTION` (parameters `ELSET`, `MATERIAL`) claims the solid elements of its
/// element set that no earlier section claimed. The density is the first value on the first
/// data line of the material's `*DENSITY` card. Names are case-insensitive.
pub fn mass_properties(mesh: &InpMesh, scan: &InpScan) -> MassReport {
    let densities = material_densities(scan);
    let vols: Vec<Option<f64>> = (0..mesh.elements.len()).map(|i| element_volume(mesh, i)).collect();
    let volume: f64 = vols.iter().flatten().sum();

    let mut claimed: HashSet<usize> = HashSet::new();
    let mut by_material = Vec::new();
    let mut mass_sum = 0.0;
    let mut moment = [0.0; 3];
    let mut weighted = 0usize;
    for card in scan.cards.iter().filter(|c| c.keyword == "SOLID SECTION") {
        let elset = param(card, "ELSET").map(name_of).unwrap_or_default();
        let material = param(card, "MATERIAL").map(name_of).unwrap_or_default();
        let density = densities.get(&material).copied().flatten();
        let mut entry = MaterialMass { material, density, elset: elset.clone(), elements: 0, volume: 0.0, mass: None };
        if let Some(set) = mesh.sets.get(&elset) {
            for eid in &set.element_ids {
                let Some(&ei) = mesh.element_index.get(eid) else { continue };
                let Some(v) = vols[ei] else { continue };
                if !claimed.insert(ei) {
                    continue;
                }
                entry.elements += 1;
                entry.volume += v;
                if let Some(rho) = density {
                    let m = rho * v;
                    mass_sum += m;
                    weighted += 1;
                    if let Some((pts, _)) = solid_corners(mesh, ei) {
                        for p in &pts {
                            for k in 0..3 {
                                moment[k] += m * p[k] / pts.len() as f64;
                            }
                        }
                    }
                }
            }
        }
        entry.mass = density.map(|d| d * entry.volume);
        by_material.push(entry);
    }

    let solids = vols.iter().flatten().count();
    let unassigned = (0..mesh.elements.len()).filter(|i| vols[*i].is_some() && !claimed.contains(i)).count();
    let all_weighted = solids > 0 && unassigned == 0 && weighted == solids;
    let mass = all_weighted.then_some(mass_sum);
    let centre_of_gravity = match mass {
        Some(m) if m != 0.0 => Some([moment[0] / m, moment[1] / m, moment[2] / m]),
        _ => None,
    };

    let mut lo = [f64::INFINITY; 3];
    let mut hi = [f64::NEG_INFINITY; 3];
    for n in &mesh.nodes {
        for (k, v) in [n.x, n.y, n.z].into_iter().enumerate() {
            lo[k] = lo[k].min(v);
            hi[k] = hi[k].max(v);
        }
    }
    let bounding_box = if mesh.nodes.is_empty() { ([0.0; 3], [0.0; 3]) } else { (lo, hi) };
    MassReport { volume, by_material, mass, centre_of_gravity, unassigned_elements: unassigned, bounding_box }
}

/// Cards whose data lines start with a set name (when not a number).
const SET_IN_DATA: [(&str, bool); 6] = [
    ("BOUNDARY", false),
    ("CLOAD", false),
    ("CFLUX", false),
    ("TEMPERATURE", false),
    ("DLOAD", true),
    ("DSLOAD", true),
];

/// Undefined or empty sets and undefined materials used by the deck, as
/// `file:line: message` strings in deck order.
///
/// Checked: the `NSET=` / `ELSET=` parameter of every card that uses a set (not the defining
/// cards `NODE`, `ELEMENT`, `NSET`, `ELSET`), the set named by the first field of
/// `BOUNDARY`/`CLOAD`/`CFLUX`/`TEMPERATURE`/`DLOAD`/`DSLOAD` data lines when that field is
/// not a number, and the `MATERIAL=` of `*SOLID SECTION`. Surfaces defined in the deck count as
/// defined names for `DLOAD`/`DSLOAD`.
pub fn reference_problems(mesh: &InpMesh, scan: &InpScan) -> Vec<String> {
    let materials: HashSet<String> = scan
        .cards
        .iter()
        .filter(|c| c.keyword == "MATERIAL")
        .filter_map(|c| param(c, "NAME").map(name_of))
        .collect();
    let surfaces: HashSet<String> = scan
        .cards
        .iter()
        .filter(|c| c.keyword == "SURFACE")
        .filter_map(|c| param(c, "NAME").map(name_of))
        .chain(mesh.surfaces.keys().cloned())
        .collect();
    let node_set = |n: &str| mesh.sets.get(n).map(|s| !s.node_ids.is_empty());
    let elem_set = |n: &str| mesh.sets.get(n).map(|s| !s.element_ids.is_empty());
    // `Some(true)` = usable, `Some(false)` = exists but empty, `None` = undefined.
    let describe = |what: &str, name: &str, state: Option<bool>| -> Option<String> {
        match state {
            Some(true) => None,
            Some(false) => Some(format!("{what} set '{name}' is empty")),
            None => Some(format!("{what} set '{name}' is not defined")),
        }
    };

    let mut out = Vec::new();
    for card in &scan.cards {
        let loc = format!("{}:{}", card.file.display(), card.line);
        let kw = card.keyword.as_str();
        if !matches!(kw, "NODE" | "ELEMENT" | "NSET" | "ELSET") {
            if let Some(v) = param(card, "NSET") {
                let n = name_of(v);
                if let Some(m) = describe("node", &n, node_set(&n)) {
                    out.push(format!("{loc}: *{kw}: {m}"));
                }
            }
            if let Some(v) = param(card, "ELSET") {
                let n = name_of(v);
                if let Some(m) = describe("element", &n, elem_set(&n)) {
                    out.push(format!("{loc}: *{kw}: {m}"));
                }
            }
        }
        if kw == "SOLID SECTION" {
            if let Some(v) = param(card, "MATERIAL") {
                let n = name_of(v);
                if !materials.contains(&n) {
                    out.push(format!("{loc}: *SOLID SECTION: material '{n}' is not defined"));
                }
            }
        }
        if let Some(&(_, is_load)) = SET_IN_DATA.iter().find(|(k, _)| *k == kw) {
            let Ok(data) = read_card_data(card, card.data_lines) else { continue };
            let mut reported: BTreeSet<String> = BTreeSet::new();
            for line in &data {
                let Some(first) = line.split(',').next().map(str::trim).filter(|s| !s.is_empty()) else { continue };
                if parse_real(first).is_some() {
                    continue;
                }
                let n = name_of(first);
                if !reported.insert(n.clone()) {
                    continue;
                }
                if is_load {
                    // DLOAD names an element set (or a surface for DSLOAD, also accepted
                    // for DLOAD): defined if either exists.
                    if surfaces.contains(&n) {
                        continue;
                    }
                    if let Some(m) = describe("element", &n, elem_set(&n)) {
                        out.push(format!("{loc}: *{kw}: {m}"));
                    }
                } else if let Some(m) = describe("node", &n, node_set(&n)) {
                    out.push(format!("{loc}: *{kw}: {m}"));
                }
            }
        }
    }
    out
}
