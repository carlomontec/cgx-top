//! Core FEA data structures representing CalculiX models, meshes, sets, and datasets.

use std::collections::HashMap;

/// 3D Nodal point
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Node {
    pub id: u32,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Standard CalculiX element topologies
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ElementType {
    // 3D Solids
    C3D8,   // 8-node brick
    C3D20,  // 20-node quadratic brick
    C3D20R, // 20-node reduced integration brick
    C3D4,   // 4-node linear tetrahedron
    C3D10,  // 10-node quadratic tetrahedron
    C3D6,   // 6-node linear wedge / prism
    C3D15,  // 15-node quadratic wedge / prism
    
    // 2D Shells / Plates
    S3,     // 3-node triangular shell (TRI3, code 7)
    S6,     // 6-node quadratic triangle shell (TRI6, code 8)
    S4,     // 4-node quadrilateral shell (QUAD4, code 9)
    S8,     // 8-node quadratic shell (QUAD8, code 10)
    
    // 1D Beams / Trusses
    B31,    // 2-node beam (BEAM2, code 11)
    B32,    // 3-node quadratic beam (BEAM3, code 12)
    T3D2,   // 2-node truss
    
    // Fallback for unknown / custom elements
    Other(i32),
}

impl ElementType {
    /// Expected number of nodes for this element topology
    pub fn node_count(&self) -> usize {
        match self {
            ElementType::C3D8 => 8,
            ElementType::C3D6 => 6,
            ElementType::C3D4 => 4,
            ElementType::C3D20 | ElementType::C3D20R => 20,
            ElementType::C3D15 => 15,
            ElementType::C3D10 => 10,
            ElementType::S3 => 3,
            ElementType::S6 => 6,
            ElementType::S4 => 4,
            ElementType::S8 => 8,
            ElementType::B31 | ElementType::T3D2 => 2,
            ElementType::B32 => 3,
            ElementType::Other(_) => 0,
        }
    }

    /// Convert CGX integer element type code (from readfrd.c) to ElementType
    pub fn from_cgx_code(code: i32) -> Self {
        match code {
            1 => ElementType::C3D8,
            2 => ElementType::C3D6,
            3 => ElementType::C3D4,
            4 => ElementType::C3D20,
            5 => ElementType::C3D15,
            6 => ElementType::C3D10,
            7 => ElementType::S3,
            8 => ElementType::S6,
            9 => ElementType::S4,
            10 => ElementType::S8,
            11 => ElementType::B31,
            12 => ElementType::B32,
            other => ElementType::Other(other),
        }
    }
}

/// Finite element definition with topology and node connectivity
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element {
    pub id: u32,
    pub elem_type: ElementType,
    pub material_id: i32,
    pub nodes: Vec<u32>,
}

/// Set classification (Node Set or Element Set)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetType {
    Node,
    Element,
    Mixed,
}

/// Named entity grouping for nodes or elements (e.g., "END_TIP_BLADE", "FIX_BASE")
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Set {
    pub name: String,
    pub set_type: SetType,
    pub node_ids: Vec<u32>,
    pub element_ids: Vec<u32>,
}

/// Identifies the physical quantity represented by a dataset block
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FieldType {
    Displacement,
    Stress,
    Strain,
    Temperature,
    Force,
    Other,
}

/// Individual component values across nodes (e.g., SXX, SYY, or D1, D2, D3)
#[derive(Debug, Clone)]
pub struct ComponentData {
    pub name: String,
    pub min_val: f32,
    pub min_node: u32,
    pub max_val: f32,
    pub max_node: u32,
    pub values: HashMap<u32, f32>,
}

/// A dataset block corresponding to an output field at a specific step/mode
#[derive(Debug, Clone)]
pub struct Dataset {
    pub name: String,
    pub field_type: FieldType,
    pub step_number: u32,
    pub value: f32, // Time point, Eigenvalue, or Frequency
    pub components: Vec<ComponentData>,
}

/// Analysis step or vibration mode shape metadata
#[derive(Debug, Clone)]
pub struct StepInfo {
    pub step_number: u32,
    pub increment_number: u32,
    pub value: f32, // Time (s) or Frequency (Hz)
    pub description: String,
    pub datasets: Vec<Dataset>,
}

/// Complete in-memory representation of a parsed CalculiX FRD model
#[derive(Debug, Default, Clone)]
pub struct FrdModel {
    pub title: String,
    pub nodes: Vec<Node>,
    pub node_index: HashMap<u32, usize>,
    pub elements: Vec<Element>,
    pub sets: HashMap<String, Set>,
    pub steps: Vec<StepInfo>,
}

impl FrdModel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retrieve a node by its original ID
    pub fn get_node(&self, id: u32) -> Option<&Node> {
        self.node_index.get(&id).map(|&idx| &self.nodes[idx])
    }

    /// Retrieve a set by its name (case-insensitive lookup)
    pub fn get_set(&self, name: &str) -> Option<&Set> {
        if let Some(s) = self.sets.get(name) {
            return Some(s);
        }
        let upper = name.to_uppercase();
        self.sets.iter().find_map(|(k, v)| {
            if k.to_uppercase() == upper {
                Some(v)
            } else {
                None
            }
        })
    }

    /// Resolve all unique node IDs belonging to a set (expanding element sets to constituent nodes)
    pub fn resolve_set_node_ids(&self, set_name: &str) -> Option<std::collections::HashSet<u32>> {
        let set = self.get_set(set_name)?;
        let mut node_ids = std::collections::HashSet::new();

        // Add explicit node IDs
        for &nid in &set.node_ids {
            node_ids.insert(nid);
        }

        // Expand element IDs to node IDs if present
        if !set.element_ids.is_empty() {
            let elem_map: HashMap<u32, &Element> = self.elements.iter().map(|e| (e.id, e)).collect();
            for &eid in &set.element_ids {
                if let Some(elem) = elem_map.get(&eid) {
                    for &nid in &elem.nodes {
                        node_ids.insert(nid);
                    }
                }
            }
        }

        Some(node_ids)
    }

    /// Populate implicit sets for element types (+C3D20, +C3D10, etc.) and materials (+mat1, etc.)
    pub fn populate_implicit_sets(&mut self) {
        let mut type_sets: HashMap<String, Vec<u32>> = HashMap::new();
        let mut mat_sets: HashMap<String, Vec<u32>> = HashMap::new();
        let mut all_elem_ids = Vec::with_capacity(self.elements.len());

        for el in &self.elements {
            all_elem_ids.push(el.id);
            let type_name = format!("+{:?}", el.elem_type);
            type_sets.entry(type_name).or_default().push(el.id);

            let mat_name = format!("+mat{}", el.material_id);
            mat_sets.entry(mat_name).or_default().push(el.id);
        }

        // Add all elements set
        self.sets.insert(
            "+all".to_string(),
            Set {
                name: "+all".to_string(),
                set_type: SetType::Mixed,
                node_ids: self.nodes.iter().map(|n| n.id).collect(),
                element_ids: all_elem_ids,
            },
        );

        // Add element type sets
        for (name, eids) in type_sets {
            self.sets.insert(
                name.clone(),
                Set {
                    name,
                    set_type: SetType::Element,
                    node_ids: Vec::new(),
                    element_ids: eids,
                },
            );
        }

        // Add material sets
        for (name, eids) in mat_sets {
            self.sets.insert(
                name.clone(),
                Set {
                    name,
                    set_type: SetType::Element,
                    node_ids: Vec::new(),
                    element_ids: eids,
                },
            );
        }
    }

    /// Automatically discover and merge companion sets from `<stem>.inp` and `*.nam` files
    pub fn load_companion_sets<P: AsRef<std::path::Path>>(&mut self, frd_path: P) {
        let companion_sets = crate::parser::load_companion_sets(frd_path);
        for (name, set) in companion_sets {
            let entry = self.sets.entry(name).or_insert_with(|| Set {
                name: set.name.clone(),
                set_type: set.set_type.clone(),
                node_ids: Vec::new(),
                element_ids: Vec::new(),
            });
            entry.node_ids.extend(set.node_ids);
            entry.element_ids.extend(set.element_ids);
        }
    }

    /// Load and merge sets from an explicit `.inp` or `.nam` file
    pub fn load_inp_sets<P: AsRef<std::path::Path>>(&mut self, inp_path: P) -> crate::Result<usize> {
        let inp_sets = crate::parser::parse_sets_from_inp(inp_path)?;
        let count = inp_sets.len();
        for (name, set) in inp_sets {
            let entry = self.sets.entry(name).or_insert_with(|| Set {
                name: set.name.clone(),
                set_type: set.set_type.clone(),
                node_ids: Vec::new(),
                element_ids: Vec::new(),
            });
            entry.node_ids.extend(set.node_ids);
            entry.element_ids.extend(set.element_ids);
        }
        Ok(count)
    }
}
