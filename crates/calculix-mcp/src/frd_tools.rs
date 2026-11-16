//! Result tools: `frd_summary`, `frd_sets`, `frd_query_set`.
//!
//! Every call opens a fresh `FrdIndex` (headers only) and loads just the datasets it needs.
//! Responses carry aggregates with the responsible node ids, never a per-node field.

use crate::error::{nearest, ToolError};
use crate::paths::Roots;
use cgx_core::{
    DenseDataset, DispExtremaSummary, FieldType, FrdIndex, SetType, StepRef, StressExtremaSummary,
    StressTensor,
};
use rayon::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

const UNITS_NOTE: &str =
    "Values are unit-agnostic (deck units). 'value' is time or frequency depending on the analysis.";

// ---------- shared pieces ----------

/// JSON number with the shortest decimal form of an f32; non-finite becomes null.
pub fn num(v: f32) -> Value {
    if v.is_finite() {
        v.to_string().parse::<f64>().map(Value::from).unwrap_or(Value::Null)
    } else {
        Value::Null
    }
}

/// `[value, node]` cells for a table row, or two nulls.
fn pair(p: Option<(f32, u32)>) -> [Value; 2] {
    match p {
        Some((v, n)) => [num(v), json!(n)],
        None => [Value::Null, Value::Null],
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Page {
    pub returned: usize,
    pub total: usize,
    pub truncated: bool,
    /// Pass as `offset` to get the next page; null when complete
    pub next_offset: Option<usize>,
}

impl Page {
    /// Clamp `offset`/`limit` to `total` and describe the resulting window.
    pub fn window(total: usize, offset: usize, limit: Option<usize>, default: usize, max: usize) -> (std::ops::Range<usize>, Page) {
        let start = offset.min(total);
        let end = (start + limit.unwrap_or(default).clamp(1, max)).min(total);
        let page = Page {
            returned: end - start,
            total,
            truncated: end - start < total,
            next_offset: (end < total).then_some(end),
        };
        (start..end, page)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Select {
    #[default]
    All,
    Last,
    FirstLast,
    Equidistant,
    List,
}

/// Which steps to include. Steps are addressed by `idx`, their zero-based position in the file.
#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct StepSel {
    #[serde(default)]
    pub select: Select,
    /// Number of steps for `equidistant` (default 10)
    pub n: Option<usize>,
    /// Step positions for `list`
    #[serde(default)]
    pub idx: Vec<usize>,
}

fn pick_steps(sel: &StepSel, total: usize) -> Result<Vec<usize>, ToolError> {
    if total == 0 {
        return Ok(Vec::new());
    }
    Ok(match sel.select {
        Select::All => (0..total).collect(),
        Select::Last => vec![total - 1],
        Select::FirstLast => {
            if total == 1 { vec![0] } else { vec![0, total - 1] }
        }
        Select::Equidistant => {
            let n = sel.n.unwrap_or(10).clamp(2, 200).min(total);
            if n < 2 {
                vec![0]
            } else {
                let mut v: Vec<usize> = (0..n).map(|i| i * (total - 1) / (n - 1)).collect();
                v.dedup();
                v
            }
        }
        Select::List => {
            if let Some(bad) = sel.idx.iter().find(|&&i| i >= total) {
                return Err(ToolError::new("UNKNOWN_STEP", format!("step idx {} does not exist", bad))
                    .valid(vec![format!("0..={}", total - 1)])
                    .hint("Call frd_summary to list the steps."));
            }
            if sel.idx.is_empty() {
                return Err(ToolError::invalid("select=list needs a non-empty idx array"));
            }
            sel.idx.clone()
        }
    })
}

fn open(roots: &Roots, path: &str, inp_path: Option<&str>) -> Result<(FrdIndex, std::path::PathBuf), ToolError> {
    let frd = roots.existing(path, &["frd"])?;
    let mut index = FrdIndex::open(&frd)
        .map_err(|e| ToolError::new("FRD_INVALID", format!("cannot read {}: {}", roots.display(&frd), e)))?;
    if let Some(inp) = inp_path {
        let inp = roots.existing(inp, &["inp", "nam"])?;
        index
            .mesh_mut()
            .load_inp_sets(&inp)
            .map_err(|e| ToolError::new("DECK_INVALID", format!("cannot read sets from {}: {}", roots.display(&inp), e)))?;
    }
    Ok((index, frd))
}

/// Entry of `step` holding the primary real dataset called `name`, else the first of `kind`.
///
/// Both tests are exact: the name is compared whole, and `field_type` is set only for the
/// exact names `DISP`, `STRESS`, ... So the imaginary parts `STRESSI`/`ERRORI` and the complex
/// `PDISP` (all `FieldType::Other`) are never taken for the real field.
fn primary(index: &FrdIndex, step: &StepRef, name: &str, kind: FieldType) -> Option<usize> {
    let entries = index.entries();
    step.entries
        .iter()
        .copied()
        .find(|&e| entries[e].name.eq_ignore_ascii_case(name))
        .or_else(|| step.entries.iter().copied().find(|&e| entries[e].field_type == kind))
}

struct StepExtrema {
    disp: Option<DispExtremaSummary>,
    /// `disp` is the PDISP amplitude bound, not a DISP magnitude
    disp_from_pdisp: bool,
    stress: Option<StressExtremaSummary>,
}

/// Extrema of the primary DISP and STRESS datasets of `step`. With `pdisp_fallback`, a step
/// that has no DISP dataset uses the amplitude bound of its PDISP dataset for `disp`.
fn step_extrema(
    index: &FrdIndex,
    step: &StepRef,
    filter: Option<&HashSet<u32>>,
    disp: bool,
    stress: bool,
    pdisp_fallback: bool,
) -> StepExtrema {
    let load = |e: Option<usize>| e.and_then(|e| index.load(e).ok());
    let disp_entry = if disp { primary(index, step, "DISP", FieldType::Displacement) } else { None };
    let pdisp_entry = if disp && pdisp_fallback && disp_entry.is_none() {
        step.entries.iter().copied().find(|&e| index.entries()[e].name.eq_ignore_ascii_case("PDISP"))
    } else {
        None
    };
    let (disp, disp_from_pdisp) = match (disp_entry, pdisp_entry) {
        (Some(e), _) => (load(Some(e)).and_then(|d| d.disp_extrema(filter)), false),
        (None, Some(e)) => (load(Some(e)).and_then(|d| d.pdisp_extrema(filter)), true),
        (None, None) => (None, false),
    };
    StepExtrema {
        disp_from_pdisp: disp_from_pdisp && disp.is_some(),
        disp,
        stress: if stress {
            load(primary(index, step, "STRESS", FieldType::Stress)).and_then(|d| d.stress_extrema(filter))
        } else {
            None
        },
    }
}

/// Step number as in the input deck when the file records it (third field of `1PSTEP`),
/// otherwise the running block counter of the `100C` header.
fn deck_step(step: &StepRef) -> u32 {
    step.pstep.map(|p| p.2).unwrap_or(step.step_number)
}

/// Track the extreme of one quantity across steps as `[value, node, idx]`.
#[derive(Default)]
struct Peak(Option<(f32, u32, usize)>);

impl Peak {
    fn feed(&mut self, candidate: Option<(f32, u32)>, idx: usize, smaller_wins: bool) {
        if let Some((v, n)) = candidate {
            let better = match self.0 {
                None => !v.is_nan(),
                Some((best, _, _)) => if smaller_wins { v < best } else { v > best },
            };
            if better {
                self.0 = Some((v, n, idx));
            }
        }
    }

    fn json(&self) -> Value {
        match self.0 {
            Some((v, n, i)) => json!([num(v), n, i]),
            None => Value::Null,
        }
    }
}

// ---------- frd_summary ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FrdSummaryIn {
    /// Path to a CalculiX .frd result file (ASCII or binary)
    pub path: String,
    #[serde(default)]
    pub steps: StepSel,
    #[serde(default)]
    pub offset: usize,
    /// Step rows per page (default 20, max 200)
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DatasetInfo {
    pub name: String,
    pub kind: String,
    pub components: Vec<String>,
    /// Number of steps that contain it
    pub steps: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FrdSummaryOut {
    pub file: String,
    pub bytes: u64,
    pub title: String,
    pub nodes: usize,
    pub elements: usize,
    pub sets: usize,
    pub steps_total: usize,
    pub datasets: Vec<DatasetInfo>,
    /// One row per step: whole-model peak displacement magnitude and von Mises stress with their node ids.
    /// `mode` and `nodal_diameter` columns are present only when the file records them.
    pub steps: Table,
    /// Peaks over ALL selected steps (not only this page) as [value, node, idx]; null if the field is absent
    pub envelope: Map<String, Value>,
    pub notes: Vec<String>,
    pub page: Page,
}

pub fn summary(roots: &Roots, p: FrdSummaryIn) -> Result<FrdSummaryOut, ToolError> {
    let (index, frd) = open(roots, &p.path, None)?;
    let chosen = pick_steps(&p.steps, index.steps().len())?;

    let extrema: Vec<StepExtrema> = chosen
        .par_iter()
        .map(|&i| step_extrema(&index, &index.steps()[i], None, true, true, true))
        .collect();

    let (mut u_max, mut vm_max) = (Peak::default(), Peak::default());
    for (&i, x) in chosen.iter().zip(&extrema) {
        u_max.feed(x.disp.as_ref().map(|d| d.max_disp), i, false);
        vm_max.feed(x.stress.as_ref().map(|s| s.max_von_mises), i, false);
    }

    // Mode and nodal diameter columns only when some step of the file carries them.
    let has_mode = index.steps().iter().any(|s| s.mode.is_some());
    let has_nd = index.steps().iter().any(|s| s.nodal_diameter.is_some());
    let mut columns: Vec<String> = ["idx", "step", "value", "analysis"].map(String::from).to_vec();
    if has_mode {
        columns.push("mode".into());
    }
    if has_nd {
        columns.push("nodal_diameter".into());
    }
    columns.extend(["u_max", "u_max_node", "vm_max", "vm_max_node"].map(String::from));

    let (window, page) = Page::window(chosen.len(), p.offset, p.limit, 20, 200);
    let rows = window
        .map(|k| {
            let (i, x) = (chosen[k], &extrema[k]);
            let step = &index.steps()[i];
            let analysis = step.entries.first().map(|&e| index.entries()[e].analysis.clone()).unwrap_or_default();
            let [u, un] = pair(x.disp.as_ref().map(|d| d.max_disp));
            let [vm, vmn] = pair(x.stress.as_ref().map(|s| s.max_von_mises));
            let mut row = vec![json!(i), json!(deck_step(step)), num(step.value), json!(analysis)];
            if has_mode {
                row.push(json!(step.mode));
            }
            if has_nd {
                row.push(json!(step.nodal_diameter));
            }
            row.extend([u, un, vm, vmn]);
            row
        })
        .collect();

    // Dataset catalogue in order of first appearance.
    let mut datasets: Vec<DatasetInfo> = Vec::new();
    for e in index.entries() {
        match datasets.iter_mut().find(|d| d.name == e.name) {
            Some(d) => d.steps += 1,
            None => datasets.push(DatasetInfo {
                name: e.name.clone(),
                kind: format!("{:?}", e.field_type),
                components: e.components.iter().map(|c| c.0.clone()).collect(),
                steps: 1,
            }),
        }
    }

    let mut notes = vec![UNITS_NOTE.to_string()];
    let from_pdisp: Vec<String> =
        chosen.iter().zip(&extrema).filter(|(_, x)| x.disp_from_pdisp).map(|(i, _)| i.to_string()).collect();
    if !from_pdisp.is_empty() {
        notes.push(format!(
            "Steps with idx {} have no DISP dataset; their u_max is sqrt(MAG1^2 + MAG2^2 + MAG3^2) of the complex PDISP dataset (magnitude and phase per component). This is an upper bound of the vector amplitude over one period, reached only where the three phases coincide.",
            from_pdisp.join(", ")
        ));
    }
    if index.entries().iter().any(|e| e.analysis.to_uppercase().contains("MODAL")) {
        notes.push("Steps with analysis MODAL hold mode shapes: 'value' is the eigenfrequency, and their displacement and stress amplitudes are normalised, not physical magnitudes.".into());
    }
    let mut envelope = Map::new();
    envelope.insert("u_max".into(), u_max.json());
    envelope.insert("vm_max".into(), vm_max.json());

    Ok(FrdSummaryOut {
        file: roots.display(&frd),
        bytes: std::fs::metadata(&frd).map(|m| m.len()).unwrap_or(0),
        title: index.mesh().title.clone(),
        nodes: index.mesh().nodes.len(),
        elements: index.mesh().elements.len(),
        sets: index.mesh().sets.len(),
        steps_total: index.steps().len(),
        datasets,
        steps: Table { columns, rows },
        envelope,
        notes,
        page,
    })
}

// ---------- frd_sets ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FrdSetsIn {
    /// Path to a .frd result file
    pub path: String,
    /// Optional .inp deck (or .nam file) whose *NSET / *ELSET definitions are added
    pub inp_path: Option<String>,
    /// Case-insensitive substring of the set name
    pub filter: Option<String>,
    #[serde(default)]
    pub offset: usize,
    /// Rows per page (default 50, max 200)
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FrdSetsOut {
    /// Columns: name, kind (node | element | mixed), nodes, elements
    pub sets: Table,
    pub page: Page,
}

fn kind_str(kind: &SetType) -> &'static str {
    match kind {
        SetType::Node => "node",
        SetType::Element => "element",
        SetType::Mixed => "mixed",
    }
}

pub fn sets(roots: &Roots, p: FrdSetsIn) -> Result<FrdSetsOut, ToolError> {
    let (index, _) = open(roots, &p.path, p.inp_path.as_deref())?;
    let needle = p.filter.as_deref().map(str::to_lowercase);
    let mut all: Vec<_> = index
        .mesh()
        .sets
        .values()
        .filter(|s| needle.as_ref().is_none_or(|n| s.name.to_lowercase().contains(n)))
        .collect();
    all.sort_by(|a, b| a.name.cmp(&b.name));

    let (window, page) = Page::window(all.len(), p.offset, p.limit, 50, 200);
    let rows = all[window]
        .iter()
        .map(|s| vec![json!(s.name), json!(kind_str(&s.set_type)), json!(s.node_ids.len()), json!(s.element_ids.len())])
        .collect();
    Ok(FrdSetsOut {
        sets: Table { columns: ["name", "kind", "nodes", "elements"].map(String::from).to_vec(), rows },
        page,
    })
}

// ---------- frd_query_set ----------

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Quantity {
    Stress,
    Displacement,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TopBy {
    VonMises,
    S1,
    S3Min,
    Tresca,
    Hydrostatic,
    Disp,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FrdQueryIn {
    /// Path to a .frd result file
    pub path: String,
    /// Exact set name from frd_sets, including implicit sets such as +all
    pub set: String,
    /// Optional .inp deck (or .nam file) whose *NSET / *ELSET definitions are added
    pub inp_path: Option<String>,
    pub quantity: Quantity,
    #[serde(default)]
    pub steps: StepSel,
    #[serde(default)]
    pub offset: usize,
    /// Step rows per page (default 20, max 200)
    pub limit: Option<usize>,
    /// Also return the N most extreme nodes (0 to 50, default 0)
    #[serde(default)]
    pub top_n: usize,
    /// Ranking quantity for top_n. Default: von_mises for stress, disp for displacement
    pub top_by: Option<TopBy>,
    /// Step idx used for the ranking. Default: the step where top_by peaks
    pub top_step: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SetInfo {
    pub name: String,
    pub kind: String,
    /// Resolved node count
    pub nodes: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TopNodes {
    pub by: TopBy,
    pub idx: usize,
    /// Columns: node, value, x, y, z
    pub nodes: Table,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FrdQueryOut {
    pub set: SetInfo,
    pub quantity: Quantity,
    /// One row per step. Stress: von Mises max/min, s1 max, s3 min, Tresca max, hydrostatic max, each followed by its node id. Displacement: magnitude max/min with node ids.
    pub steps: Table,
    /// Extremes over ALL selected steps (not only this page) as [value, node, idx]
    pub envelope: Map<String, Value>,
    pub top: Option<TopNodes>,
    pub notes: Vec<String>,
    pub page: Page,
}

const STRESS_COLUMNS: [&str; 15] = [
    "idx", "step", "value", "vm_max", "vm_max_node", "vm_min", "vm_min_node", "s1_max", "s1_max_node",
    "s3_min", "s3_min_node", "tresca_max", "tresca_max_node", "hydro_max", "hydro_max_node",
];
const DISP_COLUMNS: [&str; 7] = ["idx", "step", "value", "u_max", "u_max_node", "u_min", "u_min_node"];

pub fn query_set(roots: &Roots, p: FrdQueryIn) -> Result<FrdQueryOut, ToolError> {
    if p.top_n > 50 {
        return Err(ToolError::new("LIMIT_EXCEEDED", "top_n must be at most 50"));
    }
    let stress = p.quantity == Quantity::Stress;
    let top_by = p.top_by.unwrap_or(if stress { TopBy::VonMises } else { TopBy::Disp });
    if stress == (top_by == TopBy::Disp) {
        let valid = if stress { vec!["von_mises", "s1", "s3_min", "tresca", "hydrostatic"] } else { vec!["disp"] };
        return Err(ToolError::invalid(format!("top_by does not belong to quantity {:?}", p.quantity))
            .valid(valid.into_iter().map(String::from).collect()));
    }

    let (index, _) = open(roots, &p.path, p.inp_path.as_deref())?;
    let mesh = index.mesh();
    let unknown_set = |message: String| {
        ToolError::new("UNKNOWN_SET", message)
            .valid(nearest(&p.set, mesh.sets.keys().cloned()))
            .hint("Call frd_sets (optionally with filter, or inp_path to add deck sets).")
    };
    let set = mesh.get_set(&p.set).ok_or_else(|| unknown_set(format!("no set named '{}'", p.set)))?;
    let ids = mesh.resolve_set_node_ids(&p.set).filter(|ids| !ids.is_empty())
        .ok_or_else(|| unknown_set(format!("set '{}' resolves to 0 nodes", p.set)))?;

    let chosen = pick_steps(&p.steps, index.steps().len())?;
    let extrema: Vec<StepExtrema> = chosen
        .par_iter()
        .map(|&i| step_extrema(&index, &index.steps()[i], Some(&ids), !stress, stress, false))
        .collect();

    // Envelope over all selected steps.
    let keys: &[&str] = if stress { &["vm_max", "s1_max", "s3_min", "tresca_max", "hydro_max"] } else { &["u_max"] };
    let mut peaks: Vec<Peak> = keys.iter().map(|_| Peak::default()).collect();
    for (&i, x) in chosen.iter().zip(&extrema) {
        if let Some(s) = &x.stress {
            peaks[0].feed(Some(s.max_von_mises), i, false);
            peaks[1].feed(Some(s.max_principal_s1), i, false);
            peaks[2].feed(Some(s.min_principal_s3), i, true);
            peaks[3].feed(Some(s.max_tresca), i, false);
            peaks[4].feed(Some(s.max_hydrostatic), i, false);
        }
        if let Some(d) = &x.disp {
            peaks[0].feed(Some(d.max_disp), i, false);
        }
    }
    let envelope: Map<String, Value> = keys.iter().zip(&peaks).map(|(k, p)| (k.to_string(), p.json())).collect();

    let (window, page) = Page::window(chosen.len(), p.offset, p.limit, 20, 200);
    let rows: Vec<Vec<Value>> = window
        .map(|k| {
            let (i, x) = (chosen[k], &extrema[k]);
            let step = &index.steps()[i];
            let mut row = vec![json!(i), json!(deck_step(step)), num(step.value)];
            if stress {
                let s = x.stress.as_ref();
                for cell in [
                    s.map(|s| s.max_von_mises), s.map(|s| s.min_von_mises), s.map(|s| s.max_principal_s1),
                    s.map(|s| s.min_principal_s3), s.map(|s| s.max_tresca), s.map(|s| s.max_hydrostatic),
                ] {
                    row.extend(pair(cell));
                }
            } else {
                let d = x.disp.as_ref();
                row.extend(pair(d.map(|d| d.max_disp)));
                row.extend(pair(d.map(|d| d.min_disp)));
            }
            row
        })
        .collect();

    let mut notes = vec![UNITS_NOTE.to_string()];
    let missing = extrema.iter().filter(|x| if stress { x.stress.is_none() } else { x.disp.is_none() }).count();
    if missing > 0 {
        notes.push(format!(
            "{} of {} selected steps have no {} dataset; their cells are null.",
            missing, chosen.len(), if stress { "STRESS" } else { "DISP" }
        ));
    }

    // Top-N nodes at one step.
    let mut top = None;
    if p.top_n > 0 {
        let peak_idx = match top_by {
            TopBy::VonMises | TopBy::Disp => peaks[0].0,
            TopBy::S1 => peaks[1].0,
            TopBy::S3Min => peaks[2].0,
            TopBy::Tresca => peaks[3].0,
            TopBy::Hydrostatic => peaks[4].0,
        }
        .map(|p| p.2);
        let idx = match p.top_step.or(peak_idx) {
            Some(i) if i < index.steps().len() => i,
            Some(i) => {
                return Err(ToolError::new("UNKNOWN_STEP", format!("top_step {} does not exist", i))
                    .valid(vec![format!("0..={}", index.steps().len().saturating_sub(1))]))
            }
            None => return Err(ToolError::new("UNKNOWN_DATASET", "no selected step contains the requested field")),
        };
        let (name, kind) = if stress { ("STRESS", FieldType::Stress) } else { ("DISP", FieldType::Displacement) };
        let ds = primary(&index, &index.steps()[idx], name, kind)
            .and_then(|e| index.load(e).ok())
            .ok_or_else(|| ToolError::new("UNKNOWN_DATASET", format!("step idx {} has no {} dataset", idx, name)))?;
        let mut ranked = node_values(&ds, top_by, &ids)
            .ok_or_else(|| ToolError::new("UNKNOWN_DATASET", format!("{} lacks the components needed for {:?}", name, top_by)))?;
        let ascending = top_by == TopBy::S3Min;
        ranked.retain(|r| !r.1.is_nan());
        ranked.sort_by(|a, b| {
            let ord = a.1.partial_cmp(&b.1).unwrap();
            (if ascending { ord } else { ord.reverse() }).then(a.0.cmp(&b.0))
        });
        ranked.truncate(p.top_n);
        let rows = ranked
            .into_iter()
            .map(|(nid, v)| {
                let (x, y, z) = mesh.get_node(nid).map(|n| (num(n.x), num(n.y), num(n.z))).unwrap_or_default();
                vec![json!(nid), num(v), x, y, z]
            })
            .collect();
        top = Some(TopNodes {
            by: top_by,
            idx,
            nodes: Table { columns: ["node", "value", "x", "y", "z"].map(String::from).to_vec(), rows },
        });
    }

    Ok(FrdQueryOut {
        set: SetInfo { name: set.name.clone(), kind: kind_str(&set.set_type).into(), nodes: ids.len() },
        quantity: p.quantity,
        steps: Table {
            columns: (if stress { STRESS_COLUMNS.to_vec() } else { DISP_COLUMNS.to_vec() }).into_iter().map(String::from).collect(),
            rows,
        },
        envelope,
        top,
        notes,
        page,
    })
}

/// `(node, value)` of one scalar quantity for every node of the dataset that is in `ids`.
fn node_values(ds: &DenseDataset, by: TopBy, ids: &HashSet<u32>) -> Option<Vec<(u32, f32)>> {
    let n = ds.node_ids.len();
    let full = |names: &[&str]| ds.column(names).filter(|c| c.len() == n);
    let rows = ds.node_ids.iter().enumerate().filter(|(_, nid)| ids.contains(nid));

    if by == TopBy::Disp {
        if let Some(all) = full(&["ALL", "MAG", "TOTAL"]) {
            return Some(rows.map(|(i, &nid)| (nid, all[i])).collect());
        }
        let (x, y, z) = (full(&["D1", "X", "DX", "U1"])?, full(&["D2", "Y", "DY", "U2"])?, full(&["D3", "Z", "DZ", "U3"])?);
        return Some(rows.map(|(i, &nid)| (nid, (x[i] * x[i] + y[i] * y[i] + z[i] * z[i]).sqrt())).collect());
    }

    let c = [
        full(&["SXX", "XX", "S11", "11"])?, full(&["SYY", "YY", "S22", "22"])?, full(&["SZZ", "ZZ", "S33", "33"])?,
        full(&["SXY", "XY", "S12", "12"])?, full(&["SYZ", "YZ", "S23", "23"])?, full(&["SZX", "ZX", "S31", "31", "S13", "13"])?,
    ];
    Some(
        rows.map(|(i, &nid)| {
            let t = StressTensor::new(c[0][i], c[1][i], c[2][i], c[3][i], c[4][i], c[5][i]);
            let v = match by {
                TopBy::VonMises => t.von_mises(),
                TopBy::S1 => t.principal_stresses().0,
                TopBy::S3Min => t.principal_stresses().2,
                TopBy::Tresca => t.max_shear(),
                TopBy::Hydrostatic => t.hydrostatic(),
                TopBy::Disp => unreachable!(),
            };
            (nid, v)
        })
        .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn step_selection() {
        let sel = |select, n, idx: &[usize]| StepSel { select, n, idx: idx.to_vec() };
        assert_eq!(pick_steps(&sel(Select::All, None, &[]), 3).unwrap(), [0, 1, 2]);
        assert_eq!(pick_steps(&sel(Select::Last, None, &[]), 3).unwrap(), [2]);
        assert_eq!(pick_steps(&sel(Select::FirstLast, None, &[]), 1).unwrap(), [0]);
        assert_eq!(pick_steps(&sel(Select::Equidistant, Some(3), &[]), 101).unwrap(), [0, 50, 100]);
        assert_eq!(pick_steps(&sel(Select::Equidistant, Some(10), &[]), 4).unwrap(), [0, 1, 2, 3]);
        assert_eq!(pick_steps(&sel(Select::List, None, &[2, 0]), 3).unwrap(), [2, 0]);
        assert_eq!(pick_steps(&sel(Select::List, None, &[3]), 3).unwrap_err().code, "UNKNOWN_STEP");
        assert!(pick_steps(&sel(Select::All, None, &[]), 0).unwrap().is_empty());
    }

    #[test]
    fn page_window_and_numbers() {
        let (w, p) = Page::window(45, 20, None, 20, 200);
        assert_eq!((w, p.returned, p.truncated, p.next_offset), (20..40, 20, true, Some(40)));
        let (w, p) = Page::window(45, 40, Some(999), 20, 200);
        assert_eq!((w, p.next_offset), (40..45, None));
        assert_eq!(Page::window(3, 9, None, 20, 200).0, 3..3);
        // Shortest f32 form, never the widened f64 digits.
        assert_eq!(num(0.1).to_string(), "0.1");
        assert_eq!(num(355.2169).to_string(), "355.2169");
        assert_eq!(num(f32::NAN), Value::Null);
    }

    /// End-to-end on a real result file; skipped when the sample is absent.
    #[test]
    fn real_file_summary_sets_and_query() {
        let dir = PathBuf::from("/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/test_NewLib");
        if !dir.join("beam_benchmark.frd").exists() {
            eprintln!("Skipping: beam_benchmark.frd not found");
            return;
        }
        let roots = Roots::new(vec![dir]).unwrap();
        let s = summary(&roots, FrdSummaryIn { path: "beam_benchmark.frd".into(), steps: StepSel::default(), offset: 0, limit: None }).unwrap();
        assert_eq!((s.nodes, s.steps_total, s.steps.rows.len()), (38475, 1, 1));
        assert_eq!(s.steps.rows[0].len(), s.steps.columns.len());
        assert!(!s.steps.columns.iter().any(|c| c == "mode" || c == "nodal_diameter"), "static run has no mode columns");
        assert!(!s.notes.iter().any(|n| n.contains("PDISP")));

        let query = |set: &str, quantity, top_n| FrdQueryIn {
            path: "beam_benchmark.frd".into(), set: set.into(), inp_path: None, quantity,
            steps: StepSel::default(), offset: 0, limit: None, top_n, top_by: None, top_step: None,
        };
        // Whole-model set reproduces the summary peak.
        let all = query_set(&roots, query("+all", Quantity::Stress, 3)).unwrap();
        assert_eq!(all.envelope["vm_max"], s.envelope["vm_max"]);
        assert_eq!(all.steps.rows[0].len(), STRESS_COLUMNS.len());
        let top = all.top.unwrap();
        assert_eq!(top.nodes.rows.len(), 3);
        assert_eq!(top.nodes.rows[0][1], all.envelope["vm_max"][0], "first ranked node carries the peak");
        assert_eq!(top.nodes.rows[0][0], all.envelope["vm_max"][1]);

        let tip = query_set(&roots, query("NNTIP", Quantity::Displacement, 0)).unwrap();
        assert_eq!(tip.set.nodes, 161);
        assert_eq!(tip.steps.rows[0].len(), DISP_COLUMNS.len());

        let err = query_set(&roots, query("ntip", Quantity::Stress, 0)).unwrap_err();
        assert_eq!((err.code, err.valid[0].as_str()), ("UNKNOWN_SET", "NNTIP"));
        assert_eq!(query_set(&roots, query("+all", Quantity::Stress, 51)).unwrap_err().code, "LIMIT_EXCEEDED");

        let listed = sets(&roots, FrdSetsIn { path: "beam_benchmark.frd".into(), inp_path: None, filter: Some("nn".into()), offset: 0, limit: None }).unwrap();
        assert_eq!(listed.sets.rows.len(), 2);
        assert_eq!(listed.sets.rows[0][2], json!(161));
    }

    /// Cyclic symmetry modal run with complex results; skipped when the sample is absent.
    #[test]
    fn cyclic_file_summary_has_modes_and_pdisp_amplitude() {
        let dir = PathBuf::from("/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor");
        if !dir.join("lavffcyc.frd").exists() {
            eprintln!("Skipping: lavffcyc.frd not found");
            return;
        }
        let roots = Roots::new(vec![dir]).unwrap();
        let s = summary(&roots, FrdSummaryIn { path: "lavffcyc.frd".into(), steps: StepSel::default(), offset: 0, limit: None }).unwrap();
        assert_eq!(s.steps_total, 4);
        let col = |name: &str| s.steps.columns.iter().position(|c| c == name).unwrap_or_else(|| panic!("no column {}", name));
        let (mode, nd, u, vm) = (col("mode"), col("nodal_diameter"), col("u_max"), col("vm_max"));
        for (k, row) in s.steps.rows.iter().enumerate() {
            assert_eq!(row.len(), s.steps.columns.len());
            assert!(row[u].is_number(), "step idx {} has u_max", k);
            assert!(row[vm].is_number(), "step idx {} has vm_max", k);
            if k == 0 {
                assert_eq!((&row[mode], &row[nd]), (&Value::Null, &Value::Null));
            } else {
                assert!(row[mode].is_number());
                assert_eq!(row[nd], json!(5));
            }
        }
        let note = s.notes.iter().find(|n| n.contains("PDISP")).expect("PDISP note");
        assert!(note.contains("idx 1, 2, 3") && note.contains("upper bound"), "{}", note);
    }
}
