//! Mesh reader for CalculiX `.inp` decks.
//!
//! Reads `*NODE`, `*ELEMENT`, `*NSET`, `*ELSET` and element-type `*SURFACE` cards across the
//! whole `*INCLUDE` tree (via [`scan_cards`] and [`read_card_data`]). Coordinates are kept in
//! `f64` ([`Node64`]) because decks are often more precise than the `f32` result-file nodes.
//! Problems in individual data lines never abort the read; they are collected in
//! [`InpMesh::warnings`].

use crate::parser::inp_cards::{read_card_data, scan_cards, InpCard, ScanLimits};
use crate::model::{dedup_keep_order, merge_set_into};
use crate::{Element, ElementType, Result, Set, SetType};
use std::collections::HashMap;
use std::path::Path;

/// Node with double-precision coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Node64 {
    pub id: u32,
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

/// Mesh and named groups of a CalculiX input deck.
#[derive(Debug, Clone, Default)]
pub struct InpMesh {
    pub nodes: Vec<Node64>,
    pub node_index: HashMap<u32, usize>,
    pub elements: Vec<Element>,
    pub element_index: HashMap<u32, usize>,
    /// Raw `TYPE=` label (upper-case) per element, parallel to `elements`.
    pub element_types: Vec<String>,
    /// Node and element sets, names upper-cased.
    pub sets: HashMap<String, Set>,
    /// Element-face surfaces, names upper-cased: `(element id, face number 1..6)`.
    pub surfaces: HashMap<String, Vec<(u32, u8)>>,
    pub warnings: Vec<String>,
}

/// Map a CalculiX element `TYPE=` label to an [`ElementType`].
fn map_type(label: &str) -> ElementType {
    match label {
        "C3D8" | "C3D8R" | "C3D8I" => ElementType::C3D8,
        "C3D20" => ElementType::C3D20,
        "C3D20R" => ElementType::C3D20R,
        "C3D4" => ElementType::C3D4,
        "C3D10" | "C3D10T" => ElementType::C3D10,
        "C3D6" => ElementType::C3D6,
        "C3D15" => ElementType::C3D15,
        "S3" => ElementType::S3,
        "S4" | "S4R" => ElementType::S4,
        "S6" => ElementType::S6,
        "S8" | "S8R" => ElementType::S8,
        "B31" | "B31R" => ElementType::B31,
        "B32" | "B32R" => ElementType::B32,
        "T3D2" => ElementType::T3D2,
        _ => ElementType::Other(0),
    }
}

/// Parse a Fortran-style real (`1.5D+2` is accepted).
fn parse_real(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if t.contains(['D', 'd']) {
        t.replace(['D', 'd'], "E").parse().ok()
    } else {
        t.parse().ok()
    }
}

fn tokens(line: &str) -> Vec<&str> {
    line.split(',').map(str::trim).filter(|t| !t.is_empty()).collect()
}

fn unquote(s: &str) -> &str {
    s.trim_matches(|c| c == '"' || c == '\'').trim()
}

/// Value of parameter `key` (already upper-case) on `card`.
fn param_of<'a>(card: &'a InpCard, key: &str) -> Option<&'a str> {
    card.params.iter().find(|(k, _)| k == key).and_then(|(_, v)| v.as_deref())
}

/// Read the mesh of the deck `root`, following `*INCLUDE` cards.
///
/// Fails only when the root file cannot be read or the scan fails (include cycle, limits).
pub fn read_inp_mesh(root: &Path) -> Result<InpMesh> {
    let mut mesh = InpMesh::default();
    let mut nsets: HashMap<String, Vec<u32>> = HashMap::new();
    let mut elsets: HashMap<String, Vec<u32>> = HashMap::new();
    let mut surfaces: HashMap<String, Vec<(u32, u8)>> = HashMap::new();

    let scan = scan_cards(root, &ScanLimits::default())?;
    for card in &scan.cards {
        if !matches!(card.keyword.as_str(), "NODE" | "ELEMENT" | "NSET" | "ELSET" | "SURFACE") {
            continue;
        }
        let data = read_card_data(card, card.data_lines)?;
        let loc = format!("{}:{}", card.file.display(), card.line);
        match card.keyword.as_str() {
            "NODE" => {
                let nset = param_of(card, "NSET").map(|v| unquote(v).to_uppercase());
                for (k, line) in data.iter().enumerate() {
                    let t = tokens(line);
                    let Some(id) = t.first().and_then(|s| s.parse::<u32>().ok()) else {
                        mesh.warnings
                            .push(format!("{loc}: bad node id in data line {}: '{}'", k + 1, line.trim()));
                        continue;
                    };
                    let mut c = [0.0f64; 3];
                    let mut ok = true;
                    for (i, slot) in c.iter_mut().enumerate() {
                        if let Some(s) = t.get(i + 1) {
                            match parse_real(s) {
                                Some(v) => *slot = v,
                                None => {
                                    ok = false;
                                    break;
                                }
                            }
                        }
                    }
                    if !ok {
                        mesh.warnings.push(format!(
                            "{loc}: non-numeric coordinate for node {id} (data line {})",
                            k + 1
                        ));
                        continue;
                    }
                    if mesh.node_index.contains_key(&id) {
                        mesh.warnings.push(format!(
                            "{loc}: duplicate node id {id} (data line {}), keeping the first",
                            k + 1
                        ));
                    } else {
                        mesh.node_index.insert(id, mesh.nodes.len());
                        mesh.nodes.push(Node64 { id, x: c[0], y: c[1], z: c[2] });
                    }
                    if let Some(n) = &nset {
                        nsets.entry(n.clone()).or_default().push(id);
                    }
                }
            }
            "ELEMENT" => {
                let label = param_of(card, "TYPE").map(|v| unquote(v).to_uppercase()).unwrap_or_default();
                let et = map_type(&label);
                let expected = et.node_count();
                let elset = param_of(card, "ELSET").map(|v| unquote(v).to_uppercase());
                let mut buf = String::new();
                let mut start_k = 0usize;
                let mut pending: Vec<(usize, String)> = Vec::new();
                for (k, line) in data.iter().enumerate() {
                    let t = line.trim();
                    if buf.is_empty() {
                        start_k = k;
                    }
                    buf.push_str(t);
                    let ntok = buf.split(',').filter(|s| !s.trim().is_empty()).count();
                    let complete = !t.ends_with(',') || (expected > 0 && ntok > expected);
                    if complete {
                        pending.push((start_k, std::mem::take(&mut buf)));
                    } else {
                        buf.push(' ');
                    }
                }
                if !buf.trim().is_empty() {
                    pending.push((start_k, buf));
                }
                for (k, text) in pending {
                    let t = tokens(&text);
                    let Some(id) = t.first().and_then(|s| s.parse::<u32>().ok()) else {
                        mesh.warnings
                            .push(format!("{loc}: bad element id in data line {}: '{}'", k + 1, text.trim()));
                        continue;
                    };
                    let mut nodes = Vec::with_capacity(t.len().saturating_sub(1));
                    let mut ok = true;
                    for s in &t[1..] {
                        match s.parse::<u32>() {
                            Ok(n) => nodes.push(n),
                            Err(_) => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    if !ok {
                        mesh.warnings.push(format!(
                            "{loc}: non-numeric node in element {id} (data line {})",
                            k + 1
                        ));
                        continue;
                    }
                    if expected > 0 && nodes.len() != expected {
                        mesh.warnings.push(format!(
                            "{loc}: element {id} of type {label} has {} nodes, expected {expected}",
                            nodes.len()
                        ));
                    }
                    if mesh.element_index.contains_key(&id) {
                        mesh.warnings.push(format!(
                            "{loc}: duplicate element id {id} (data line {}), keeping the first",
                            k + 1
                        ));
                        continue;
                    }
                    mesh.element_index.insert(id, mesh.elements.len());
                    mesh.elements.push(Element { id, elem_type: et, material_id: 0, nodes });
                    mesh.element_types.push(label.clone());
                    if let Some(n) = &elset {
                        elsets.entry(n.clone()).or_default().push(id);
                    }
                }
            }
            kw @ ("NSET" | "ELSET") => {
                let is_node = kw == "NSET";
                let Some(name) = param_of(card, kw).map(|v| unquote(v).to_uppercase()) else {
                    mesh.warnings.push(format!("{loc}: *{kw} without a {kw}= name"));
                    continue;
                };
                let generate = card.params.iter().any(|(k, _)| k == "GENERATE");
                let mut ids: Vec<u32> = Vec::new();
                for (k, line) in data.iter().enumerate() {
                    let t = tokens(line);
                    if generate {
                        let nums: Vec<Option<u32>> = t.iter().map(|s| s.parse::<u32>().ok()).collect();
                        if t.len() < 2 || nums.iter().any(|n| n.is_none()) {
                            mesh.warnings.push(format!(
                                "{loc}: bad GENERATE row in data line {}: '{}'",
                                k + 1,
                                line.trim()
                            ));
                            continue;
                        }
                        let first = nums[0].unwrap();
                        let last = nums[1].unwrap();
                        let inc = nums.get(2).copied().flatten().unwrap_or(1);
                        if inc == 0 {
                            mesh.warnings
                                .push(format!("{loc}: zero increment in GENERATE (data line {})", k + 1));
                            continue;
                        }
                        let mut v = first;
                        while v <= last {
                            ids.push(v);
                            match v.checked_add(inc) {
                                Some(n) => v = n,
                                None => break,
                            }
                        }
                    } else {
                        for s in t {
                            if let Ok(id) = s.parse::<u32>() {
                                ids.push(id);
                            } else {
                                let key = unquote(s).to_uppercase();
                                let src = if is_node { &nsets } else { &elsets };
                                match src.get(&key) {
                                    Some(members) => ids.extend(members.iter().copied()),
                                    None => mesh.warnings.push(format!(
                                        "{loc}: unknown {} set '{s}' in *{kw} {name} (data line {})",
                                        if is_node { "node" } else { "element" },
                                        k + 1
                                    )),
                                }
                            }
                        }
                    }
                }
                let target = if is_node { &mut nsets } else { &mut elsets };
                target.entry(name).or_default().extend(ids);
            }
            "SURFACE" => {
                let ty = param_of(card, "TYPE").map(|v| unquote(v).to_uppercase());
                if ty.as_deref().is_some_and(|t| t != "ELEMENT") {
                    mesh.warnings.push(format!(
                        "{loc}: *SURFACE of TYPE={} skipped (only TYPE=ELEMENT is read)",
                        ty.unwrap_or_default()
                    ));
                    continue;
                }
                let Some(name) = param_of(card, "NAME").map(|v| unquote(v).to_uppercase()) else {
                    mesh.warnings.push(format!("{loc}: *SURFACE without NAME="));
                    continue;
                };
                let entry = surfaces.entry(name.clone()).or_default();
                for (k, line) in data.iter().enumerate() {
                    let t = tokens(line);
                    if t.len() < 2 {
                        mesh.warnings.push(format!(
                            "{loc}: surface row without face label in data line {}: '{}'",
                            k + 1,
                            line.trim()
                        ));
                        continue;
                    }
                    let label = t[1].to_uppercase();
                    let face = label
                        .strip_prefix('S')
                        .and_then(|d| d.parse::<u8>().ok())
                        .filter(|f| (1..=6).contains(f));
                    let Some(face) = face else {
                        mesh.warnings.push(format!(
                            "{loc}: unsupported face label '{}' in surface {name} (data line {})",
                            t[1],
                            k + 1
                        ));
                        continue;
                    };
                    if let Ok(eid) = t[0].parse::<u32>() {
                        entry.push((eid, face));
                    } else {
                        let key = unquote(t[0]).to_uppercase();
                        match elsets.get(&key) {
                            Some(members) => entry.extend(members.iter().map(|&e| (e, face))),
                            None => mesh.warnings.push(format!(
                                "{loc}: unknown element set '{}' in surface {name} (data line {})",
                                t[0],
                                k + 1
                            )),
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Surfaces: drop repeated (element, face) pairs, keep first-seen order.
    for (name, mut faces) in surfaces {
        let mut seen = std::collections::HashSet::with_capacity(faces.len());
        faces.retain(|p| seen.insert(*p));
        mesh.surfaces.insert(name, faces);
    }

    // Sets: a name used for both a node set and an element set becomes a mixed set.
    for (name, mut ids) in nsets {
        dedup_keep_order(&mut ids);
        let set = Set { name: name.clone(), set_type: SetType::Node, node_ids: ids, element_ids: Vec::new() };
        merge_set_into(&mut mesh.sets, name, set);
    }
    for (name, mut ids) in elsets {
        dedup_keep_order(&mut ids);
        let set = Set { name: name.clone(), set_type: SetType::Element, node_ids: Vec::new(), element_ids: ids };
        merge_set_into(&mut mesh.sets, name.clone(), set);
        if let Some(s) = mesh.sets.get_mut(&name) {
            if !s.node_ids.is_empty() {
                s.set_type = SetType::Mixed;
            }
        }
    }
    Ok(mesh)
}
