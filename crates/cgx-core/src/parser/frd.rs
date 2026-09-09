//! Unified hybrid FRD parser supporting both ASCII and Binary (single/double precision) CalculiX result files.

use crate::{
    ComponentData, Dataset, Element, ElementType, FieldType, FrdError, FrdModel, Node, Result,
    StepInfo,
};
use super::reader::ByteCursor;
use std::collections::HashMap;

/// Helper to check if a byte slice is empty or only whitespace
#[inline]
fn is_blank(bytes: &[u8]) -> bool {
    bytes.iter().all(|b| b.is_ascii_whitespace())
}

/// Helper to parse an integer from a byte slice with leading/trailing spaces
#[inline]
fn parse_int<T: std::str::FromStr>(bytes: &[u8]) -> Option<T> {
    let s = std::str::from_utf8(bytes).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    s.parse::<T>().ok()
}

/// Helper to parse a float from a byte slice with leading/trailing spaces
#[inline]
fn parse_float(bytes: &[u8]) -> Option<f32> {
    let s = std::str::from_utf8(bytes).ok()?.trim();
    if s.is_empty() {
        return None;
    }
    s.parse::<f32>().ok()
}

/// Helper to safely slice bytes with bounds checking
#[inline]
fn slice_safe(line: &[u8], start: usize, end: usize) -> &[u8] {
    let len = line.len();
    if start >= len {
        b""
    } else {
        let actual_end = end.min(len);
        &line[start..actual_end]
    }
}

/// High-performance hybrid ASCII and Binary FRD parser
pub struct FrdParserEngine<'a> {
    cursor: ByteCursor<'a>,
}

impl<'a> FrdParserEngine<'a> {
    pub fn from_bytes(data: &'a [u8]) -> Self {
        Self {
            cursor: ByteCursor::new(data),
        }
    }

    /// Parse the entire FRD file into a complete `FrdModel`
    pub fn parse_all(&mut self) -> Result<FrdModel> {
        let mut model = FrdModel::new();

        while let Some(line) = self.cursor.read_line() {
            if is_blank(line) {
                continue;
            }

            if line.starts_with(b"    1C") || line.starts_with(b" 1C") || line.starts_with(b"1C") {
                let name = slice_safe(line, 6, 12);
                model.title = std::str::from_utf8(name).unwrap_or("").trim().to_string();
            } else if line.contains(&b'2') && (line.starts_with(b"    2C") || line.starts_with(b" 2C") || line.starts_with(b"2C")) {
                let nodes = self.parse_nodes_block(line)?;
                for (idx, node) in nodes.iter().enumerate() {
                    model.node_index.insert(node.id, idx);
                }
                model.nodes = nodes;
            } else if line.contains(&b'3') && (line.starts_with(b"    3C") || line.starts_with(b" 3C") || line.starts_with(b"3C")) {
                let elements = self.parse_elements_block(line)?;
                model.elements = elements;
            } else if line.starts_with(b"  100C") || line.starts_with(b" 100C") || line.starts_with(b"100C") {
                let dataset = self.parse_dataset_block(line)?;
                let step_num = dataset.step_number;
                if let Some(existing_step) = model.steps.iter_mut().find(|s| s.step_number == step_num) {
                    existing_step.datasets.push(dataset);
                } else {
                    let step_info = StepInfo {
                        step_number: step_num,
                        increment_number: 1,
                        value: dataset.value,
                        description: dataset.name.clone(),
                        datasets: vec![dataset],
                    };
                    model.steps.push(step_info);
                }
            } else if line.starts_with(b"9999") {
                break;
            }
        }

        Ok(model)
    }

    /// Parse Node definitions (`2C` ... )
    fn parse_nodes_block(&mut self, header_line: &[u8]) -> Result<Vec<Node>> {
        let nod_data: usize = parse_int(slice_safe(header_line, 24, 36)).unwrap_or(0);
        let format_flag: usize = if header_line.len() > 73 {
            parse_int(slice_safe(header_line, 73, 75)).unwrap_or(0)
        } else {
            0
        };

        let mut nodes = Vec::with_capacity(if nod_data > 0 { nod_data } else { 1024 });

        match format_flag {
            // ASCII Short (0) or Long (1)
            0 | 1 => {
                while let Some(line) = self.cursor.read_line() {
                    if is_blank(line) {
                        continue;
                    }
                    if line.starts_with(b" -3") || line.starts_with(b"-3") {
                        break;
                    }
                    if line.starts_with(b" -1") || line.starts_with(b"-1") {
                        let (id, x, y, z) = if format_flag == 0 {
                            let id: u32 = parse_int(slice_safe(line, 3, 8)).unwrap_or(0);
                            let x = parse_float(slice_safe(line, 8, 20)).unwrap_or(0.0);
                            let y = parse_float(slice_safe(line, 20, 32)).unwrap_or(0.0);
                            let z = parse_float(slice_safe(line, 32, 44)).unwrap_or(0.0);
                            (id, x, y, z)
                        } else {
                            let id: u32 = parse_int(slice_safe(line, 3, 13)).unwrap_or(0);
                            let x = parse_float(slice_safe(line, 13, 25)).unwrap_or(0.0);
                            let y = parse_float(slice_safe(line, 25, 37)).unwrap_or(0.0);
                            let z = parse_float(slice_safe(line, 37, 49)).unwrap_or(0.0);
                            (id, x, y, z)
                        };
                        if id > 0 {
                            nodes.push(Node { id, x, y, z });
                        }
                    }
                }
            }
            // Binary Float (2): 16 bytes per node (int32 id + 3x float32)
            2 => {
                let total_bytes = nod_data * 16;
                let raw = self.cursor.read_exact(total_bytes).ok_or_else(|| {
                    FrdError::InvalidFormat(format!("Unexpected EOF reading {} binary float nodes", nod_data))
                })?;

                for chunk in raw.chunks_exact(16) {
                    let id = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
                    let x = f32::from_le_bytes(chunk[4..8].try_into().unwrap());
                    let y = f32::from_le_bytes(chunk[8..12].try_into().unwrap());
                    let z = f32::from_le_bytes(chunk[12..16].try_into().unwrap());
                    nodes.push(Node { id, x, y, z });
                }
            }
            // Binary Double (3): 28 bytes per node (int32 id + 3x float64)
            3 => {
                let total_bytes = nod_data * 28;
                let raw = self.cursor.read_exact(total_bytes).ok_or_else(|| {
                    FrdError::InvalidFormat(format!("Unexpected EOF reading {} binary double nodes", nod_data))
                })?;

                for chunk in raw.chunks_exact(28) {
                    let id = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
                    let x = f64::from_le_bytes(chunk[4..12].try_into().unwrap()) as f32;
                    let y = f64::from_le_bytes(chunk[12..20].try_into().unwrap()) as f32;
                    let z = f64::from_le_bytes(chunk[20..28].try_into().unwrap()) as f32;
                    nodes.push(Node { id, x, y, z });
                }
            }
            _ => {
                return Err(FrdError::InvalidFormat(format!("Unsupported node format flag: {}", format_flag)));
            }
        }

        Ok(nodes)
    }

    /// Parse Element definitions (`3C` ... )
    fn parse_elements_block(&mut self, header_line: &[u8]) -> Result<Vec<Element>> {
        let elem_data: usize = parse_int(slice_safe(header_line, 24, 36)).unwrap_or(0);
        let format_flag: usize = if header_line.len() > 73 {
            parse_int(slice_safe(header_line, 73, 75)).unwrap_or(0)
        } else {
            0
        };

        let mut elements = Vec::with_capacity(if elem_data > 0 { elem_data } else { 1024 });

        match format_flag {
            // ASCII Short (0) or Long (1)
            0 | 1 => {
                let mut pending_elem: Option<(u32, ElementType, i32, usize)> = None;
                while let Some(line) = self.cursor.read_line() {
                    if is_blank(line) {
                        continue;
                    }
                    if line.starts_with(b" -3") || line.starts_with(b"-3") {
                        break;
                    }
                    if line.starts_with(b" -1") || line.starts_with(b"-1") {
                        let (elem_id, type_code, _group, mat) = if format_flag == 0 {
                            let elem_id: u32 = parse_int(slice_safe(line, 3, 8)).unwrap_or(0);
                            let type_code: i32 = parse_int(slice_safe(line, 8, 13)).unwrap_or(0);
                            let group: i32 = parse_int(slice_safe(line, 13, 18)).unwrap_or(0);
                            let mat: i32 = parse_int(slice_safe(line, 18, 23)).unwrap_or(0);
                            (elem_id, type_code, group, mat)
                        } else {
                            let elem_id: u32 = parse_int(slice_safe(line, 3, 13)).unwrap_or(0);
                            let type_code: i32 = parse_int(slice_safe(line, 13, 18)).unwrap_or(0);
                            let group: i32 = parse_int(slice_safe(line, 18, 23)).unwrap_or(0);
                            let mat: i32 = parse_int(slice_safe(line, 23, 28)).unwrap_or(0);
                            (elem_id, type_code, group, mat)
                        };

                        let elem_type = ElementType::from_cgx_code(type_code);
                        let expected_nodes = elem_type.node_count();
                        pending_elem = Some((elem_id, elem_type, mat, expected_nodes));
                    } else if line.starts_with(b" -2") || line.starts_with(b"-2") {
                        if let Some((elem_id, elem_type, mat, expected_nodes)) = pending_elem {
                            let col_width = if format_flag == 0 { 5 } else { 10 };
                            let max_cols_per_line = if format_flag == 0 { 15 } else { 10 };
                            let mut node_ids = Vec::with_capacity(expected_nodes);

                            let count = expected_nodes.min(max_cols_per_line);
                            for j in 0..count {
                                let start = 3 + j * col_width;
                                let end = start + col_width;
                                if let Some(nid) = parse_int::<u32>(slice_safe(line, start, end)) {
                                    if nid > 0 {
                                        node_ids.push(nid);
                                    }
                                }
                            }

                            while node_ids.len() < expected_nodes {
                                if let Some(next_line) = self.cursor.read_line() {
                                    if next_line.starts_with(b" -2") || next_line.starts_with(b"-2") {
                                        let remaining = expected_nodes - node_ids.len();
                                        let c = remaining.min(max_cols_per_line);
                                        for j in 0..c {
                                            let start = 3 + j * col_width;
                                            let end = start + col_width;
                                            if let Some(nid) = parse_int::<u32>(slice_safe(next_line, start, end)) {
                                                if nid > 0 {
                                                    node_ids.push(nid);
                                                }
                                            }
                                        }
                                    } else {
                                        break;
                                    }
                                } else {
                                    break;
                                }
                            }

                            elements.push(Element {
                                id: elem_id,
                                elem_type,
                                material_id: mat,
                                nodes: node_ids,
                            });
                            pending_elem = None;
                        }
                    }
                }
            }
            // Binary format (2)
            2 => {
                for _ in 0..elem_data {
                    let hdr = self.cursor.read_exact(16).ok_or_else(|| {
                        FrdError::InvalidFormat("Unexpected EOF reading binary element header".into())
                    })?;

                    let elem_id = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
                    let type_code = i32::from_le_bytes(hdr[4..8].try_into().unwrap());
                    let _group = i32::from_le_bytes(hdr[8..12].try_into().unwrap());
                    let mat = i32::from_le_bytes(hdr[12..16].try_into().unwrap());

                    let elem_type = ElementType::from_cgx_code(type_code);
                    let n_nodes = elem_type.node_count();

                    let nod_bytes = self.cursor.read_exact(n_nodes * 4).ok_or_else(|| {
                        FrdError::InvalidFormat("Unexpected EOF reading binary element connectivity".into())
                    })?;

                    let mut node_ids = Vec::with_capacity(n_nodes);
                    for chunk in nod_bytes.chunks_exact(4) {
                        node_ids.push(u32::from_le_bytes(chunk.try_into().unwrap()));
                    }

                    elements.push(Element {
                        id: elem_id,
                        elem_type,
                        material_id: mat,
                        nodes: node_ids,
                    });
                }
            }
            _ => {
                return Err(FrdError::InvalidFormat(format!("Unsupported element format flag: {}", format_flag)));
            }
        }

        Ok(elements)
    }

    /// Parse Nodal Results Dataset block (`100C` ... )
    fn parse_dataset_block(&mut self, header_line: &[u8]) -> Result<Dataset> {
        let value = parse_float(slice_safe(header_line, 12, 24)).unwrap_or(0.0);
        let nod_data: usize = parse_int(slice_safe(header_line, 24, 36)).unwrap_or(0);
        let step_number = parse_int(slice_safe(header_line, 58, 63)).unwrap_or(1);
        let format_flag: usize = if header_line.len() > 73 {
            parse_int(slice_safe(header_line, 73, 75)).unwrap_or(0)
        } else {
            0
        };

        let mut field_name = String::new();
        let mut component_names: Vec<String> = Vec::new();

        // Read metadata lines (-4 and -5)
        while let Some(line) = self.cursor.read_line() {
            if is_blank(line) {
                continue;
            }
            if line.starts_with(b" -4") || line.starts_with(b"-4") {
                let name_bytes = slice_safe(line, 4, 12);
                field_name = std::str::from_utf8(name_bytes).unwrap_or("").trim().to_string();
            } else if line.starts_with(b" -5") || line.starts_with(b"-5") {
                let comp_bytes = slice_safe(line, 4, 12);
                let comp_name = std::str::from_utf8(comp_bytes).unwrap_or("").trim().to_string();
                if !comp_name.is_empty() {
                    component_names.push(comp_name);
                }
            } else if line.starts_with(b" -1") || line.starts_with(b"-1") {
                // If ASCII format, we've encountered the first data line.
                // We'll process it and subsequent ASCII lines.
                return self.parse_ascii_dataset_payload(header_line, field_name, component_names, line, step_number, value, format_flag);
            } else {
                // In binary format, data begins directly after the last -5 record without a leading -1 text marker!
                // Rewind cursor back by length of line + 1 (newline)
                let rewind_len = line.len() + 1;
                self.cursor.rewind(rewind_len);
                break;
            }
        }

        // Process binary dataset payload
        self.parse_binary_dataset_payload(field_name, component_names, nod_data, step_number, value, format_flag)
    }

    /// Process ASCII dataset lines
    fn parse_ascii_dataset_payload(
        &mut self,
        _header_line: &[u8],
        field_name: String,
        component_names: Vec<String>,
        first_line: &[u8],
        step_number: u32,
        value: f32,
        format_flag: usize,
    ) -> Result<Dataset> {
        let mut components: Vec<ComponentData> = component_names
            .iter()
            .map(|cname| ComponentData {
                name: cname.clone(),
                min_val: f32::MAX,
                min_node: 0,
                max_val: f32::MIN,
                max_node: 0,
                values: HashMap::new(),
            })
            .collect();

        let col_offset = if format_flag == 0 { 8 } else { 13 };

        let mut process_line = |line: &[u8], is_first: bool| {
            if is_first {
                let node_id: u32 = if format_flag == 0 {
                    parse_int(slice_safe(line, 3, 8)).unwrap_or(0)
                } else {
                    parse_int(slice_safe(line, 3, 13)).unwrap_or(0)
                };

                let max_primary = components.len().min(6);
                for i in 0..max_primary {
                    let start = col_offset + i * 12;
                    let end = start + 12;
                    if let Some(val) = parse_float(slice_safe(line, start, end)) {
                        components[i].values.insert(node_id, val);
                        if val < components[i].min_val {
                            components[i].min_val = val;
                            components[i].min_node = node_id;
                        }
                        if val > components[i].max_val {
                            components[i].max_val = val;
                            components[i].max_node = node_id;
                        }
                    }
                }
            }
        };

        process_line(first_line, true);

        while let Some(line) = self.cursor.read_line() {
            if is_blank(line) {
                continue;
            }
            if line.starts_with(b" -3") || line.starts_with(b"-3") {
                break;
            }
            if line.starts_with(b" -1") || line.starts_with(b"-1") {
                process_line(line, true);
            }
        }

        let field_type = match field_name.to_uppercase().as_str() {
            "DISP" => FieldType::Displacement,
            "STRESS" => FieldType::Stress,
            "STRAIN" | "TOSTRAIN" => FieldType::Strain,
            "TEMP" => FieldType::Temperature,
            "FORC" | "FORCE" => FieldType::Force,
            _ => FieldType::Other,
        };

        Ok(Dataset {
            name: field_name,
            field_type,
            step_number,
            value,
            components,
        })
    }

    /// Process Binary dataset payload
    fn parse_binary_dataset_payload(
        &mut self,
        field_name: String,
        component_names: Vec<String>,
        nod_data: usize,
        step_number: u32,
        value: f32,
        format_flag: usize,
    ) -> Result<Dataset> {
        let ncomps = component_names.len();
        let mut components: Vec<ComponentData> = component_names
            .iter()
            .map(|cname| ComponentData {
                name: cname.clone(),
                min_val: f32::MAX,
                min_node: 0,
                max_val: f32::MIN,
                max_node: 0,
                values: HashMap::with_capacity(nod_data),
            })
            .collect();

        if format_flag == 2 {
            // Float32: 4 bytes node_id + ncomps * 4 bytes float values
            let bytes_per_record = 4 + ncomps * 4;
            let total_bytes = nod_data * bytes_per_record;
            let raw = self.cursor.read_exact(total_bytes).ok_or_else(|| {
                FrdError::InvalidFormat(format!("Unexpected EOF reading binary float dataset {}", field_name))
            })?;

            for chunk in raw.chunks_exact(bytes_per_record) {
                let node_id = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
                for (i, val_chunk) in chunk[4..].chunks_exact(4).enumerate() {
                    if i < ncomps {
                        let val = f32::from_le_bytes(val_chunk.try_into().unwrap());
                        components[i].values.insert(node_id, val);
                        if val < components[i].min_val {
                            components[i].min_val = val;
                            components[i].min_node = node_id;
                        }
                        if val > components[i].max_val {
                            components[i].max_val = val;
                            components[i].max_node = node_id;
                        }
                    }
                }
            }
        } else if format_flag == 3 {
            // Float64: 4 bytes node_id + ncomps * 8 bytes double values
            let bytes_per_record = 4 + ncomps * 8;
            let total_bytes = nod_data * bytes_per_record;
            let raw = self.cursor.read_exact(total_bytes).ok_or_else(|| {
                FrdError::InvalidFormat(format!("Unexpected EOF reading binary double dataset {}", field_name))
            })?;

            for chunk in raw.chunks_exact(bytes_per_record) {
                let node_id = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
                for (i, val_chunk) in chunk[4..].chunks_exact(8).enumerate() {
                    if i < ncomps {
                        let val = f64::from_le_bytes(val_chunk.try_into().unwrap()) as f32;
                        components[i].values.insert(node_id, val);
                        if val < components[i].min_val {
                            components[i].min_val = val;
                            components[i].min_node = node_id;
                        }
                        if val > components[i].max_val {
                            components[i].max_val = val;
                            components[i].max_node = node_id;
                        }
                    }
                }
            }
        }

        let field_type = match field_name.to_uppercase().as_str() {
            "DISP" => FieldType::Displacement,
            "STRESS" => FieldType::Stress,
            "STRAIN" | "TOSTRAIN" => FieldType::Strain,
            "TEMP" => FieldType::Temperature,
            "FORC" | "FORCE" => FieldType::Force,
            _ => FieldType::Other,
        };

        Ok(Dataset {
            name: field_name,
            field_type,
            step_number,
            value,
            components,
        })
    }
}
