use cgx_core::{
    compute_dataset_disp_invariants, compute_dataset_stress_invariants,
    FrdModel,
};
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveFieldView {
    Auto,
    Displacement,
    Stress,
}

impl ActiveFieldView {
    pub fn next(&self) -> Self {
        match self {
            ActiveFieldView::Auto => ActiveFieldView::Displacement,
            ActiveFieldView::Displacement => ActiveFieldView::Stress,
            ActiveFieldView::Stress => ActiveFieldView::Auto,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ActiveFieldView::Auto => "All Fields",
            ActiveFieldView::Displacement => "Displacement",
            ActiveFieldView::Stress => "Stress",
        }
    }
}

/// Precomputed details for a step
#[derive(Debug, Clone, Default)]
pub struct StepExtremaDetail {
    pub step_number: u32,
    pub increment_number: u32,
    pub value: f32,
    pub description: String,
    
    // Displacement
    pub has_disp: bool,
    pub max_disp: Option<(f32, u32)>, // (value, node_id)
    pub min_disp: Option<(f32, u32)>,
    pub disp_comps: Vec<(String, f32, u32, f32, u32)>, // (comp_name, min_val, min_node, max_val, max_node)
    
    // Stress
    pub has_stress: bool,
    pub max_von_mises: Option<(f32, u32)>,
    pub min_von_mises: Option<(f32, u32)>,
    pub max_tresca: Option<(f32, u32)>,
    pub max_s1: Option<(f32, u32)>,
    pub min_s3: Option<(f32, u32)>,
    pub stress_comps: Vec<(String, f32, u32, f32, u32)>,

    // Other fields (e.g. ENER, TEMP, etc.)
    pub other_fields: Vec<(String, Vec<(String, f32, u32, f32, u32)>)>,

    pub filtered_nodes_count: usize,
}

pub struct AppState {
    pub model: FrdModel,
    pub filename: String,
    pub file_size_kb: f64,
    
    // Navigation
    pub selected_step_idx: usize,
    pub available_sets: Vec<String>,
    pub active_set_idx: usize,
    pub active_nodes: Option<HashSet<u32>>,
    pub field_view: ActiveFieldView,

    // Modals
    pub show_set_modal: bool,
    pub set_modal_cursor: usize,
    pub set_search_query: String,
    pub show_help_modal: bool,

    // Cached chart data
    pub chart_values: Vec<f32>,
    pub chart_label: String,

    pub should_quit: bool,
}

impl AppState {
    pub fn new(model: FrdModel, filename: String, file_size_kb: f64, initial_set: Option<&str>) -> Self {
        // Collect and sort all available set names
        let mut available_sets = vec!["[ALL NODES]".to_string()];
        let mut set_names: Vec<String> = model.sets.keys().cloned().collect();
        set_names.sort();
        available_sets.extend(set_names);

        let initial_idx = if let Some(s) = initial_set {
            available_sets.iter().position(|name| name.eq_ignore_ascii_case(s)).unwrap_or(0)
        } else {
            0
        };

        let mut app = Self {
            model,
            filename,
            file_size_kb,
            selected_step_idx: 0,
            available_sets,
            active_set_idx: initial_idx,
            active_nodes: None,
            field_view: ActiveFieldView::Auto,
            show_set_modal: false,
            set_modal_cursor: 0,
            set_search_query: String::new(),
            show_help_modal: false,
            chart_values: Vec::new(),
            chart_label: String::new(),
            should_quit: false,
        };

        app.update_active_set(initial_idx);
        app
    }

    pub fn update_active_set(&mut self, idx: usize) {
        if self.available_sets.is_empty() {
            return;
        }
        self.active_set_idx = idx % self.available_sets.len();
        let set_name = &self.available_sets[self.active_set_idx];

        if self.active_set_idx == 0 || set_name == "[ALL NODES]" {
            self.active_nodes = None;
        } else {
            self.active_nodes = self.model.resolve_set_node_ids(set_name);
        }

        self.recompute_chart();
    }

    pub fn next_set(&mut self) {
        if self.available_sets.is_empty() { return; }
        let next_idx = (self.active_set_idx + 1) % self.available_sets.len();
        self.update_active_set(next_idx);
    }

    pub fn prev_set(&mut self) {
        if self.available_sets.is_empty() { return; }
        let prev_idx = if self.active_set_idx == 0 {
            self.available_sets.len() - 1
        } else {
            self.active_set_idx - 1
        };
        self.update_active_set(prev_idx);
    }

    pub fn reset_all_nodes(&mut self) {
        self.update_active_set(0);
    }

    pub fn next_step(&mut self) {
        if !self.model.steps.is_empty() && self.selected_step_idx + 1 < self.model.steps.len() {
            self.selected_step_idx += 1;
        }
    }

    pub fn prev_step(&mut self) {
        if self.selected_step_idx > 0 {
            self.selected_step_idx -= 1;
        }
    }

    pub fn jump_to_step(&mut self, step_1_based: usize) {
        if step_1_based >= 1 && step_1_based <= self.model.steps.len() {
            self.selected_step_idx = step_1_based - 1;
        }
    }

    pub fn toggle_field_view(&mut self) {
        self.field_view = self.field_view.next();
        self.recompute_chart();
    }

    pub fn recompute_chart(&mut self) {
        if self.model.steps.is_empty() {
            self.chart_values.clear();
            self.chart_label.clear();
            return;
        }

        let filter = self.active_nodes.as_ref();

        // Check if steps have displacement or stress datasets
        let has_mises = self.model.steps.iter().any(|s| {
            s.datasets.iter().any(|d| d.field_type == cgx_core::FieldType::Stress)
        });
        let has_disp = self.model.steps.iter().any(|s| {
            s.datasets.iter().any(|d| d.field_type == cgx_core::FieldType::Displacement)
        });

        match self.field_view {
            ActiveFieldView::Stress if has_mises => {
                let history = self.model.compute_stress_history(filter);
                self.chart_values = history.iter().map(|s| s.max_von_mises.0).collect();
                self.chart_label = "Peak von Mises Stress".to_string();
            }
            ActiveFieldView::Displacement if has_disp => {
                let history = self.model.compute_disp_history(filter);
                self.chart_values = history.iter().map(|s| s.max_disp.0).collect();
                self.chart_label = "Peak Displacement Magnitude".to_string();
            }
            _ => {
                // Auto: prioritize stress, then disp, then step values (frequencies/time)
                if has_mises {
                    let history = self.model.compute_stress_history(filter);
                    self.chart_values = history.iter().map(|s| s.max_von_mises.0).collect();
                    self.chart_label = "Peak von Mises Stress".to_string();
                } else if has_disp {
                    let history = self.model.compute_disp_history(filter);
                    self.chart_values = history.iter().map(|s| s.max_disp.0).collect();
                    self.chart_label = "Peak Displacement Magnitude".to_string();
                } else {
                    self.chart_values = self.model.steps.iter().map(|s| s.value).collect();
                    self.chart_label = "Step Values / Frequencies".to_string();
                }
            }
        }
    }

    pub fn compute_step_details(&self, step_idx: usize) -> Option<StepExtremaDetail> {
        let step = self.model.steps.get(step_idx)?;
        let filter = self.active_nodes.as_ref();
        let candidate_nodes_count = if let Some(f) = filter {
            f.len()
        } else {
            self.model.nodes.len()
        };

        let mut detail = StepExtremaDetail {
            step_number: step.step_number,
            increment_number: step.increment_number,
            value: step.value,
            description: step.description.clone(),
            filtered_nodes_count: candidate_nodes_count,
            ..Default::default()
        };

        for ds in &step.datasets {
            match ds.field_type {
                cgx_core::FieldType::Displacement => {
                    detail.has_disp = true;
                    if let Some(disp_summary) = compute_dataset_disp_invariants(ds, filter) {
                        detail.max_disp = Some(disp_summary.max_disp);
                        detail.min_disp = Some(disp_summary.min_disp);
                    }
                    // Individual components
                    for comp in &ds.components {
                        if let Some((min_val, min_node, max_val, max_node)) = get_component_extrema(comp, filter) {
                            detail.disp_comps.push((comp.name.clone(), min_val, min_node, max_val, max_node));
                        }
                    }
                }
                cgx_core::FieldType::Stress => {
                    detail.has_stress = true;
                    if let Some(stress_summary) = compute_dataset_stress_invariants(ds, filter) {
                        detail.max_von_mises = Some(stress_summary.max_von_mises);
                        detail.min_von_mises = Some(stress_summary.min_von_mises);
                        detail.max_tresca = Some(stress_summary.max_tresca);
                        detail.max_s1 = Some(stress_summary.max_principal_s1);
                        detail.min_s3 = Some(stress_summary.min_principal_s3);
                    }
                    for comp in &ds.components {
                        if let Some((min_val, min_node, max_val, max_node)) = get_component_extrema(comp, filter) {
                            detail.stress_comps.push((comp.name.clone(), min_val, min_node, max_val, max_node));
                        }
                    }
                }
                _ => {
                    let mut comps = Vec::new();
                    for comp in &ds.components {
                        if let Some((min_val, min_node, max_val, max_node)) = get_component_extrema(comp, filter) {
                            comps.push((comp.name.clone(), min_val, min_node, max_val, max_node));
                        }
                    }
                    if !comps.is_empty() {
                        detail.other_fields.push((ds.name.clone(), comps));
                    }
                }
            }
        }

        Some(detail)
    }

    pub fn filtered_set_list(&self) -> Vec<(usize, &String)> {
        if self.set_search_query.trim().is_empty() {
            self.available_sets.iter().enumerate().collect()
        } else {
            let q = self.set_search_query.to_lowercase();
            self.available_sets
                .iter()
                .enumerate()
                .filter(|(_, name)| name.to_lowercase().contains(&q))
                .collect()
        }
    }
}

fn get_component_extrema(
    comp: &cgx_core::ComponentData,
    filter: Option<&HashSet<u32>>,
) -> Option<(f32, u32, f32, u32)> {
    if comp.values.is_empty() {
        if filter.is_none() {
            return Some((comp.min_val, comp.min_node, comp.max_val, comp.max_node));
        }
        return None;
    }

    let mut min_val = f32::INFINITY;
    let mut min_node = 0;
    let mut max_val = -f32::INFINITY;
    let mut max_node = 0;
    let mut count = 0;

    for (&nid, &val) in &comp.values {
        if let Some(f) = filter {
            if !f.contains(&nid) {
                continue;
            }
        }
        count += 1;
        if val < min_val {
            min_val = val;
            min_node = nid;
        }
        if val > max_val {
            max_val = val;
            max_node = nid;
        }
    }

    if count > 0 {
        Some((min_val, min_node, max_val, max_node))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cgx_core::{Node, Set, SetType, StepInfo};

    fn make_test_model() -> FrdModel {
        let mut model = FrdModel::new();
        model.nodes.push(Node { id: 1, x: 0.0, y: 0.0, z: 0.0 });
        model.nodes.push(Node { id: 2, x: 1.0, y: 0.0, z: 0.0 });
        model.nodes.push(Node { id: 3, x: 2.0, y: 0.0, z: 0.0 });
        for (i, n) in model.nodes.iter().enumerate() {
            model.node_index.insert(n.id, i);
        }

        model.sets.insert("NFIX".to_string(), Set {
            name: "NFIX".to_string(),
            set_type: SetType::Node,
            node_ids: vec![1],
            element_ids: vec![],
        });
        model.sets.insert("NTIP".to_string(), Set {
            name: "NTIP".to_string(),
            set_type: SetType::Node,
            node_ids: vec![3],
            element_ids: vec![],
        });

        for s in 1..=5 {
            model.steps.push(StepInfo {
                step_number: s,
                increment_number: 1,
                value: s as f32 * 10.0,
                description: format!("Mode {}", s),
                datasets: vec![],
            });
        }

        model
    }

    #[test]
    fn test_app_state_navigation() {
        let model = make_test_model();
        let mut app = AppState::new(model, "test.frd".to_string(), 10.0, None);

        // Initial state
        assert_eq!(app.selected_step_idx, 0);
        assert_eq!(app.active_set_idx, 0); // [ALL NODES]
        assert!(app.active_nodes.is_none());

        // Step navigation: S / W
        app.next_step();
        assert_eq!(app.selected_step_idx, 1);
        app.next_step();
        assert_eq!(app.selected_step_idx, 2);
        app.prev_step();
        assert_eq!(app.selected_step_idx, 1);

        // Jump to step with keys 1-9
        app.jump_to_step(4);
        assert_eq!(app.selected_step_idx, 3); // 4-th step (index 3)

        // Set rotation: D / A
        app.next_set();
        assert_eq!(app.active_set_idx, 1);
        assert!(app.active_nodes.is_some());

        app.next_set();
        assert_eq!(app.active_set_idx, 2);

        // Quick reset to ALL nodes with key '0'
        app.reset_all_nodes();
        assert_eq!(app.active_set_idx, 0);
        assert!(app.active_nodes.is_none());

        // Field view toggling
        assert_eq!(app.field_view, ActiveFieldView::Auto);
        app.toggle_field_view();
        assert_eq!(app.field_view, ActiveFieldView::Displacement);
        app.toggle_field_view();
        assert_eq!(app.field_view, ActiveFieldView::Stress);
        app.toggle_field_view();
        assert_eq!(app.field_view, ActiveFieldView::Auto);

        // Search modal filtering
        app.set_search_query = "fix".to_string();
        let filtered = app.filtered_set_list();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].1, "NFIX");
    }
}
