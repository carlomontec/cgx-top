//! Lazy FRD access: index the file once (mesh + dataset headers), load result datasets on demand.
//!
//! `parse_frd_file` materialises every nodal value of every dataset. `FrdIndex` reads only the
//! mesh and the dataset headers, remembers where each payload lives in the file, and parses a
//! payload into dense columns only when asked. Histories are computed one dataset at a time, so
//! peak memory is one dataset per worker thread instead of the whole file.

use super::frd::{default_component_names, is_blank, parse_float, parse_int, slice_safe};
use super::reader::ByteCursor;
use super::FrdParserEngine;
use crate::{
    DispExtremaSummary, FieldType, FrdError, FrdModel, Result, StressExtremaSummary,
};
use rayon::prelude::*;
use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;

/// Header of one result dataset (`100C` block) and the location of its payload.
#[derive(Debug, Clone)]
pub struct DatasetEntry {
    pub name: String,
    pub field_type: FieldType,
    pub step_number: u32,
    /// Time point, eigenvalue or frequency, as written in the block header
    pub value: f32,
    /// Analysis type text from the block header (may be empty)
    pub analysis: String,
    /// (component name, iexist). `iexist == 1` marks a derived component absent from the payload.
    pub components: Vec<(String, i32)>,
    /// Node count announced in the block header
    pub node_count: usize,
    /// 0 = ASCII short, 1 = ASCII long, 2 = binary f32, 3 = binary f64
    pub format: u8,
    /// Byte range of the payload within the file
    pub payload: Range<usize>,
}

/// Datasets that share one step counter value, in file order.
#[derive(Debug, Clone)]
pub struct StepRef {
    pub step_number: u32,
    pub value: f32,
    /// Indices into `FrdIndex::entries()`
    pub entries: Vec<usize>,
}

/// One loaded dataset. `columns[c][i]` is component `c` at node `node_ids[i]`.
///
/// A column is empty when the component is absent from the payload and cannot be derived.
/// An unreadable ASCII field is stored as NaN.
#[derive(Debug, Clone)]
pub struct DenseDataset {
    pub name: String,
    pub field_type: FieldType,
    pub step_number: u32,
    pub value: f32,
    pub names: Vec<String>,
    pub node_ids: Vec<u32>,
    pub columns: Vec<Vec<f32>>,
}

enum Backing {
    Mmap(memmap2::Mmap),
    Bytes(Vec<u8>),
}

impl Backing {
    fn bytes(&self) -> &[u8] {
        match self {
            Backing::Mmap(m) => m,
            Backing::Bytes(b) => b,
        }
    }
}

/// Index over an FRD file: mesh and sets in memory, result payloads left on disk.
pub struct FrdIndex {
    data: Backing,
    mesh: FrdModel,
    entries: Vec<DatasetEntry>,
    steps: Vec<StepRef>,
}

fn field_type_of(name: &str) -> FieldType {
    match name.to_uppercase().as_str() {
        "DISP" => FieldType::Displacement,
        "STRESS" => FieldType::Stress,
        "STRAIN" | "TOSTRAIN" => FieldType::Strain,
        "TEMP" => FieldType::Temperature,
        "FORC" | "FORCE" => FieldType::Force,
        _ => FieldType::Other,
    }
}

#[inline]
fn has_key(line: &[u8], key: &[u8]) -> bool {
    line.starts_with(key) || (line.first() == Some(&b' ') && line[1..].starts_with(key))
}

impl FrdIndex {
    /// Memory-map `path` and index it. Companion set files are loaded as in `parse_frd_file`.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        let mmap = unsafe { memmap2::Mmap::map(&file)? };
        let mut index = Self::build(Backing::Mmap(mmap))?;
        index.mesh.load_companion_sets(path);
        Ok(index)
    }

    /// Index an in-memory FRD image.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::build(Backing::Bytes(bytes))
    }

    fn build(data: Backing) -> Result<Self> {
        let (mesh, entries) = scan(data.bytes())?;
        let mut steps: Vec<StepRef> = Vec::new();
        for (idx, entry) in entries.iter().enumerate() {
            match steps.iter_mut().find(|s| s.step_number == entry.step_number) {
                Some(step) => step.entries.push(idx),
                None => steps.push(StepRef {
                    step_number: entry.step_number,
                    value: entry.value,
                    entries: vec![idx],
                }),
            }
        }
        Ok(Self { data, mesh, entries, steps })
    }

    /// Title, nodes, elements and sets. `steps` of this model is always empty.
    pub fn mesh(&self) -> &FrdModel {
        &self.mesh
    }

    /// Mutable mesh access, e.g. to add sets with `load_inp_sets`.
    pub fn mesh_mut(&mut self) -> &mut FrdModel {
        &mut self.mesh
    }

    pub fn entries(&self) -> &[DatasetEntry] {
        &self.entries
    }

    pub fn steps(&self) -> &[StepRef] {
        &self.steps
    }

    /// Parse the payload of dataset `entry` into dense columns.
    pub fn load(&self, entry: usize) -> Result<DenseDataset> {
        let e = self
            .entries
            .get(entry)
            .ok_or_else(|| FrdError::NotFound(format!("dataset index {}", entry)))?;
        load_dense(self.data.bytes(), e)
    }

    /// Stress extrema per step, optionally restricted to a node set.
    /// Equivalent to `FrdModel::compute_stress_history` without holding the datasets in memory.
    pub fn stress_history(&self, filter: Option<&HashSet<u32>>) -> Vec<StressExtremaSummary> {
        self.history_jobs(|e| {
            e.field_type == FieldType::Stress || e.name.to_uppercase().contains("STRESS")
        })
        .par_iter()
        .filter_map(|&(idx, step_number, value)| {
            let mut summ = self.load(idx).ok()?.stress_extrema(filter)?;
            summ.step_number = step_number;
            summ.value = value;
            Some(summ)
        })
        .collect()
    }

    /// Displacement extrema per step, optionally restricted to a node set.
    /// Equivalent to `FrdModel::compute_disp_history` without holding the datasets in memory.
    pub fn disp_history(&self, filter: Option<&HashSet<u32>>) -> Vec<DispExtremaSummary> {
        self.history_jobs(|e| {
            e.field_type == FieldType::Displacement || e.name.to_uppercase().contains("DISP")
        })
        .par_iter()
        .filter_map(|&(idx, step_number, value)| {
            let mut summ = self.load(idx).ok()?.disp_extrema(filter)?;
            summ.step_number = step_number;
            summ.value = value;
            Some(summ)
        })
        .collect()
    }

    /// (entry index, step number, step value) for every matching dataset, in step order.
    fn history_jobs(&self, matches: impl Fn(&DatasetEntry) -> bool) -> Vec<(usize, u32, f32)> {
        self.steps
            .iter()
            .flat_map(|s| s.entries.iter().map(move |&idx| (idx, s.step_number, s.value)))
            .filter(|&(idx, _, _)| matches(&self.entries[idx]))
            .collect()
    }
}

/// Read the mesh blocks and every dataset header; skip the payloads.
fn scan(data: &[u8]) -> Result<(FrdModel, Vec<DatasetEntry>)> {
    let mut engine = FrdParserEngine::from_bytes(data);
    let mut model = FrdModel::new();
    let mut entries = Vec::new();

    while let Some(line) = engine.cursor.read_line() {
        if is_blank(line) {
            continue;
        }
        if has_key(line, b"1C") || line.starts_with(b"    1C") {
            let name = slice_safe(line, 6, 12);
            model.title = std::str::from_utf8(name).unwrap_or("").trim().to_string();
        } else if has_key(line, b"2C") || line.starts_with(b"    2C") {
            let nodes = engine.parse_nodes_block(line)?;
            for (idx, node) in nodes.iter().enumerate() {
                model.node_index.insert(node.id, idx);
            }
            model.nodes = nodes;
        } else if has_key(line, b"3C") || line.starts_with(b"    3C") {
            model.elements = engine.parse_elements_block(line)?;
        } else if has_key(line, b"100C") || line.starts_with(b"  100C") {
            entries.push(scan_dataset(&mut engine.cursor, data, line)?);
        } else if line.starts_with(b"9999") {
            break;
        }
    }

    model.populate_implicit_sets();
    Ok((model, entries))
}

fn scan_dataset(cursor: &mut ByteCursor, data: &[u8], header: &[u8]) -> Result<DatasetEntry> {
    let value = parse_float(slice_safe(header, 12, 24)).unwrap_or(0.0);
    let node_count: usize = parse_int(slice_safe(header, 24, 36)).unwrap_or(0);
    let step_number = parse_int(slice_safe(header, 58, 63)).unwrap_or(1);
    let analysis = std::str::from_utf8(slice_safe(header, 63, 73)).unwrap_or("").trim().to_string();
    let format: u8 = parse_int(slice_safe(header, 73, 75)).unwrap_or(0);

    let mut name = String::new();
    let mut ncomps: usize = 0;
    let mut comp_lines: usize = 0;
    let mut components: Vec<(String, i32)> = Vec::new();

    // Header records (-4, -5) up to the first payload byte.
    let payload_start = loop {
        let line_start = cursor.pos();
        let Some(line) = cursor.read_line() else { break cursor.pos() };
        if is_blank(line) {
            continue;
        }
        if has_key(line, b"-4") {
            name = std::str::from_utf8(slice_safe(line, 4, 12)).unwrap_or("").trim().to_string();
            ncomps = parse_int(slice_safe(line, 13, 18)).unwrap_or(0);
        } else if has_key(line, b"-5") {
            comp_lines += 1;
            let comp = std::str::from_utf8(slice_safe(line, 4, 12)).unwrap_or("").trim().to_string();
            let iexist = parse_int(slice_safe(line, 33, 38)).unwrap_or(0);
            if !comp.is_empty() {
                components.push((comp, iexist));
            }
        } else {
            cursor.seek(line_start);
            break line_start;
        }
        // A binary payload has no text marker: stop as soon as all component records are read,
        // so that raw bytes are never interpreted as a text line.
        if format >= 2 && ncomps > 0 && comp_lines >= ncomps {
            break cursor.pos();
        }
    };

    if components.is_empty() {
        components = default_component_names(&name, ncomps)
            .into_iter()
            .map(|n| {
                let iexist = if n.eq_ignore_ascii_case("ALL") { 1 } else { 0 };
                (n, iexist)
            })
            .collect();
    }

    let payload_end = if format >= 2 {
        let nvals = components.iter().filter(|(_, iexist)| *iexist != 1).count();
        let width = if format == 2 { 4 } else { 8 };
        let end = payload_start + node_count * (4 + nvals * width);
        if end > data.len() {
            return Err(FrdError::InvalidFormat(format!(
                "Unexpected EOF in binary dataset {} (step {})",
                name, step_number
            )));
        }
        cursor.seek(end);
        end
    } else {
        skip_ascii_payload(cursor, data, payload_start, node_count)
    };

    Ok(DatasetEntry {
        field_type: field_type_of(&name),
        name,
        step_number,
        value,
        analysis,
        components,
        node_count,
        format,
        payload: payload_start..payload_end,
    })
}

/// Advance past an ASCII payload and its `-3` terminator; returns the payload end offset.
fn skip_ascii_payload(cursor: &mut ByteCursor, data: &[u8], start: usize, node_count: usize) -> usize {
    // Fast path: fixed-length records, so the terminator position is known. Taken only if a
    // `-3` record really starts there; otherwise fall back to a line scan.
    if let Some(first) = cursor.read_line() {
        let record_len = cursor.pos() - start;
        let guess = start + node_count * record_len;
        if has_key(first, b"-1")
            && node_count > 0
            && guess < data.len()
            && data[guess - 1] == b'\n'
            && has_key(&data[guess..], b"-3")
        {
            cursor.seek(guess);
            cursor.read_line();
            return guess;
        }
    }
    cursor.seek(start);
    loop {
        let line_start = cursor.pos();
        match cursor.read_line() {
            Some(line) if has_key(line, b"-3") => return line_start,
            Some(_) => {}
            None => return cursor.pos(),
        }
    }
}

fn load_dense(data: &[u8], e: &DatasetEntry) -> Result<DenseDataset> {
    let raw = data
        .get(e.payload.clone())
        .ok_or_else(|| FrdError::InvalidFormat(format!("Payload of {} out of bounds", e.name)))?;
    let payload_cols: Vec<usize> = e
        .components
        .iter()
        .enumerate()
        .filter_map(|(idx, (_, iexist))| if *iexist != 1 { Some(idx) } else { None })
        .collect();
    let mut columns: Vec<Vec<f32>> = vec![Vec::new(); e.components.len()];
    let mut node_ids: Vec<u32> = Vec::with_capacity(e.node_count);

    if e.format >= 2 {
        let width = if e.format == 2 { 4 } else { 8 };
        let record = 4 + payload_cols.len() * width;
        for &c in &payload_cols {
            columns[c].reserve_exact(raw.len() / record);
        }
        for chunk in raw.chunks_exact(record) {
            node_ids.push(u32::from_le_bytes(chunk[0..4].try_into().unwrap()));
            for (k, &c) in payload_cols.iter().enumerate() {
                let at = 4 + k * width;
                let v = if width == 4 {
                    f32::from_le_bytes(chunk[at..at + 4].try_into().unwrap())
                } else {
                    f64::from_le_bytes(chunk[at..at + 8].try_into().unwrap()) as f32
                };
                columns[c].push(v);
            }
        }
    } else {
        // Only the first record line of a node is read (up to 6 values), as in the full parser.
        let (id_end, col_offset) = if e.format == 0 { (8, 8) } else { (13, 13) };
        let readable = payload_cols.len().min(6);
        for &c in &payload_cols[..readable] {
            columns[c].reserve_exact(e.node_count);
        }
        let mut cursor = ByteCursor::new(raw);
        while let Some(line) = cursor.read_line() {
            if !has_key(line, b"-1") {
                continue;
            }
            node_ids.push(parse_int(slice_safe(line, 3, id_end)).unwrap_or(0));
            for (k, &c) in payload_cols[..readable].iter().enumerate() {
                let at = col_offset + k * 12;
                columns[c].push(parse_float(slice_safe(line, at, at + 12)).unwrap_or(f32::NAN));
            }
        }
    }

    let mut ds = DenseDataset {
        name: e.name.clone(),
        field_type: e.field_type,
        step_number: e.step_number,
        value: e.value,
        names: e.components.iter().map(|(n, _)| n.clone()).collect(),
        node_ids,
        columns,
    };
    ds.derive_magnitude();
    Ok(ds)
}

impl DenseDataset {
    /// Fill an absent `ALL` component of a displacement dataset with the vector magnitude.
    fn derive_magnitude(&mut self) {
        if !self.name.to_uppercase().contains("DISP") {
            return;
        }
        let find = |names: &[&str]| {
            self.names.iter().position(|n| names.contains(&n.to_uppercase().as_str()))
        };
        let Some(all) = find(&["ALL"]) else { return };
        if !self.columns[all].is_empty() {
            return;
        }
        let axes = [find(&["D1", "X", "ND1"]), find(&["D2", "Y", "ND2"]), find(&["D3", "Z", "ND3"])];
        let n = self.node_ids.len();
        let mag: Vec<f32> = (0..n)
            .map(|i| {
                let mut sum = 0.0f32;
                for axis in axes {
                    let v = axis.and_then(|c| self.columns[c].get(i).copied()).unwrap_or(0.0);
                    sum += v * v;
                }
                sum.sqrt()
            })
            .collect();
        self.columns[all] = mag;
    }

    /// Values of the first component whose name matches one of `names` (case-insensitive).
    pub fn column(&self, names: &[&str]) -> Option<&[f32]> {
        self.names
            .iter()
            .position(|n| names.iter().any(|&m| n.trim().eq_ignore_ascii_case(m)))
            .map(|c| self.columns[c].as_slice())
    }
}
