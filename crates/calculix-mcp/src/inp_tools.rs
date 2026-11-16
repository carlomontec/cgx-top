//! Deck tools: `inp_inspect`, `inp_consolidate`.

use crate::error::ToolError;
use crate::frd_tools::{Page, Table};
use crate::paths::Roots;
use cgx_core::{consolidate_inp, inspect_inp, read_card_data, scan_cards, InpCard, InpScan, ScanLimits};
use std::collections::BTreeMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InpInspectIn {
    /// Path to the root .inp deck
    pub path: String,
    /// Case-insensitive substring filter on set names
    pub set_filter: Option<String>,
    /// Cap for the includes and sets lists (default 50, max 200)
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct IncludeInfo {
    pub target: String,
    /// file:line of the *INCLUDE card
    pub from: String,
    pub exists: bool,
    pub bytes: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StepOut {
    pub n: u32,
    /// file:line of the *STEP card
    pub at: String,
    /// Raw *STEP parameters, e.g. NLGEOM, PERTURBATION
    pub params: String,
    /// First procedure card, e.g. STATIC, FREQUENCY
    pub procedure: Option<String>,
    /// Keyword -> count inside the step
    pub cards: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExternalRefOut {
    pub card: String,
    pub param: String,
    pub target: String,
    pub at: String,
    pub exists: bool,
}

fn scan(roots: &Roots, path: &std::path::Path) -> Result<InpScan, ToolError> {
    scan_cards(path, &ScanLimits::default())
        .map_err(|e| ToolError::new("DECK_INVALID", format!("{}: {}", roots.display(path), e)))
}

fn at(roots: &Roots, file: &std::path::Path, line: usize) -> String {
    format!("{}:{}", roots.display(file), line)
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct InpInspectOut {
    /// Any of CYCLIC_SYMMETRY, CONTACT, NLGEOM, PERTURBATION, MISSING_INCLUDES, NO_STEPS, NO_MESH
    pub flags: Vec<String>,
    /// Model-level keyword -> count (cards outside steps)
    pub census: BTreeMap<String, usize>,
    /// Step plan in deck order (capped by limit)
    pub step_plan: Vec<StepOut>,
    /// Files referenced by INPUT= / FILE= parameters on cards other than *INCLUDE
    pub external_refs: Vec<ExternalRefOut>,
    pub root_file: String,
    pub lines: usize,
    pub files: usize,
    pub steps: usize,
    pub boundary_cards: usize,
    pub has_nodes: bool,
    pub has_elements: bool,
    pub materials: Vec<String>,
    pub missing_includes: usize,
    /// Missing includes first, then file order
    pub includes: Vec<IncludeInfo>,
    pub includes_total: usize,
    /// Columns: name, kind (node | element), count. Largest sets first
    pub sets: Table,
    /// Describes the sets table
    pub page: Page,
}

pub fn inspect(roots: &Roots, p: InpInspectIn) -> Result<InpInspectOut, ToolError> {
    let path = roots.existing(&p.path, &["inp"])?;
    let s = inspect_inp(&path).map_err(|e| ToolError::new("DECK_INVALID", e.to_string()))?;
    let cap = p.limit.unwrap_or(50).clamp(1, 200);

    let mut includes: Vec<_> = s.includes.iter().collect();
    includes.sort_by_key(|i| i.exists); // stable: missing (false) first
    let includes_total = includes.len();
    let includes = includes
        .into_iter()
        .take(cap)
        .map(|i| IncludeInfo {
            target: i.raw_target.clone(),
            from: format!("{}:{}", roots.display(&i.included_from), i.line_number),
            exists: i.exists,
            bytes: i.file_size_bytes,
        })
        .collect();

    let needle = p.set_filter.as_deref().map(str::to_lowercase);
    let mut sets: Vec<(&String, &'static str, usize)> = s
        .node_sets
        .iter()
        .map(|(n, c)| (n, "node", *c))
        .chain(s.element_sets.iter().map(|(n, c)| (n, "element", *c)))
        .filter(|(n, _, _)| needle.as_ref().is_none_or(|f| n.to_lowercase().contains(f)))
        .collect();
    sets.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(b.0)));
    let (window, page) = Page::window(sets.len(), 0, Some(cap), 50, 200);
    let rows = sets[window].iter().map(|(n, k, c)| vec![json!(n), json!(k), json!(c)]).collect();

    let cards = scan(roots, &path)?;
    Ok(InpInspectOut {
        flags: cards.flags().into_iter().map(String::from).collect(),
        census: cards.census(),
        step_plan: cards
            .steps()
            .into_iter()
            .take(cap)
            .map(|st| StepOut { n: st.number, at: at(roots, &st.file, st.line), params: st.params, procedure: st.procedure, cards: st.cards })
            .collect(),
        external_refs: cards
            .external_refs()
            .into_iter()
            .take(cap)
            .map(|r| ExternalRefOut { at: at(roots, &r.file, r.line), card: r.card, param: r.param, target: r.target, exists: r.exists })
            .collect(),
        root_file: roots.display(&s.root_file),
        lines: s.total_lines,
        files: s.total_files_count,
        steps: s.step_count,
        boundary_cards: s.boundary_count,
        has_nodes: s.has_nodes,
        has_elements: s.has_elements,
        materials: s.materials.clone(),
        missing_includes: s.includes.iter().filter(|i| !i.exists).count(),
        includes,
        includes_total,
        sets: Table { columns: ["name", "kind", "count"].map(String::from).to_vec(), rows },
        page,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InpConsolidateIn {
    /// Path to the root .inp deck
    pub path: String,
    /// Output file. Default: <stem>_consolidated.inp beside the root deck
    pub output_path: Option<String>,
    /// Replace an existing output file (default false)
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct InpConsolidateOut {
    pub output_file: String,
    pub files_inlined: usize,
    pub lines_written: usize,
    pub bytes: usize,
    /// First 50 inlined files
    pub inlined: Vec<String>,
}

pub fn consolidate(roots: &Roots, p: InpConsolidateIn) -> Result<InpConsolidateOut, ToolError> {
    let path = roots.existing(&p.path, &["inp"])?;
    let default_out = path.with_file_name(format!(
        "{}_consolidated.inp",
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("deck")
    ));
    let out = match &p.output_path {
        Some(raw) => roots.creatable(raw, &["inp"])?,
        None => roots.creatable(&default_out.to_string_lossy(), &["inp"])?,
    };
    if out == path {
        return Err(ToolError::invalid("output_path must differ from the input deck"));
    }
    if out.exists() && !p.overwrite {
        return Err(ToolError::new("RESULTS_EXIST", format!("{} already exists", roots.display(&out)))
            .hint("Pass overwrite=true or choose another output_path."));
    }
    let (text, stats) = consolidate_inp(&path).map_err(|e| ToolError::new("DECK_INVALID", e.to_string()))?;
    std::fs::write(&out, &text).map_err(ToolError::internal)?;
    Ok(InpConsolidateOut {
        output_file: roots.display(&out),
        files_inlined: stats.total_files_inlined,
        lines_written: stats.total_lines_written,
        bytes: text.len(),
        inlined: stats.inlined_files.iter().take(50).map(|f| roots.display(f)).collect(),
    })
}

// ---------- inp_cards ----------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct InpCardsIn {
    /// Path to the root .inp deck
    pub path: String,
    /// Keyword, case-insensitive, leading * optional: "CONTACT PAIR", "*boundary"
    pub keyword: String,
    /// 0 = model level only, n = inside step n, omit = anywhere
    pub step: Option<u32>,
    /// Data lines shown per card (default 5, max 50)
    pub data_lines: Option<usize>,
    #[serde(default)]
    pub offset: usize,
    /// Cards per page (default 10, max 50)
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CardOut {
    /// file:line of the card header
    pub at: String,
    /// 0 = model level
    pub step: u32,
    pub header: String,
    pub data: Vec<String>,
    /// Data lines the card has in total
    pub data_total: usize,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct InpCardsOut {
    pub cards: Vec<CardOut>,
    /// Set when nothing matched: the most frequent keywords in the deck
    pub hint: Option<String>,
    pub page: Page,
}

fn header_text(card: &InpCard) -> String {
    let mut h = format!("*{}", card.keyword);
    for (k, v) in &card.params {
        h += &match v {
            Some(v) => format!(", {}={}", k, v),
            None => format!(", {}", k),
        };
    }
    h
}

pub fn cards(roots: &Roots, p: InpCardsIn) -> Result<InpCardsOut, ToolError> {
    let path = roots.existing(&p.path, &["inp"])?;
    let scanned = scan(roots, &path)?;
    let found = scanned.find(&p.keyword, p.step);
    let lines = p.data_lines.unwrap_or(5).min(50);
    let (window, page) = Page::window(found.len(), p.offset, p.limit, 10, 50);

    let mut out = Vec::new();
    for card in &found[window] {
        let data = if lines == 0 { Vec::new() } else { read_card_data(card, lines).map_err(ToolError::internal)? };
        out.push(CardOut {
            at: at(roots, &card.file, card.line),
            step: card.step,
            header: header_text(card),
            data: data.into_iter().map(|l| l.chars().take(300).collect()).collect(),
            data_total: card.data_lines,
        });
    }

    let hint = found.is_empty().then(|| {
        let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
        for c in &scanned.cards {
            *counts.entry(c.keyword.as_str()).or_default() += 1;
        }
        let mut top: Vec<_> = counts.into_iter().collect();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let names: Vec<&str> = top.into_iter().take(10).map(|t| t.0).collect();
        format!("No such card. Most frequent keywords in this deck: {}", names.join(", "))
    });
    Ok(InpCardsOut { cards: out, hint, page })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspect_and_cards_on_a_small_deck() {
        let dir = std::env::temp_dir().join(format!("calculix-mcp-inp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mesh.inc"), "*NODE, NSET=Nall\n1,0.,0.,0.\n2,1.,0.,0.\n*ELEMENT, TYPE=B31, ELSET=Eall\n1,1,2\n").unwrap();
        std::fs::write(
            dir.join("deck.inp"),
            "*INCLUDE, INPUT=mesh.inc\n*INCLUDE, INPUT=gone.inc\n*BOUNDARY\n1,1,6\n** comment\n*MATERIAL, NAME=STEEL\n*ELASTIC\n210000.,0.3\n\
*STEP, NLGEOM\n*STATIC\n0.1,1.\n*CLOAD\n2,2,-1.\n2,3,-2.\n2,1,-3.\n*END STEP\n",
        )
        .unwrap();
        let roots = Roots::new(vec![dir.clone()]).unwrap();

        let out = inspect(&roots, InpInspectIn { path: "deck.inp".into(), set_filter: None, limit: None }).unwrap();
        assert_eq!(out.missing_includes, 1);
        assert!(!out.includes[0].exists, "missing include listed first");
        assert!(out.flags.contains(&"NLGEOM".to_string()) && out.flags.contains(&"MISSING_INCLUDES".to_string()));
        assert_eq!(out.census["BOUNDARY"], 1);
        assert_eq!((out.step_plan.len(), out.step_plan[0].procedure.as_deref()), (1, Some("STATIC")));
        assert_eq!(out.step_plan[0].at, "deck.inp:9");

        let query = |keyword: &str, step, data_lines| InpCardsIn { path: "deck.inp".into(), keyword: keyword.into(), step, data_lines, offset: 0, limit: None };
        let c = cards(&roots, query("*cload", None, Some(2))).unwrap();
        assert_eq!((c.cards.len(), c.cards[0].step, c.cards[0].data_total), (1, 1, 3));
        assert_eq!(c.cards[0].data, ["2,2,-1.", "2,3,-2."]);
        assert_eq!(c.cards[0].header, "*CLOAD");
        assert_eq!(cards(&roots, query("node", None, None)).unwrap().cards[0].at, "mesh.inc:1");
        assert!(cards(&roots, query("CLOAD", Some(0), None)).unwrap().cards.is_empty());
        let none = cards(&roots, query("CONTACT PAIR", None, None)).unwrap();
        assert!(none.cards.is_empty() && none.hint.unwrap().contains("INCLUDE"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
