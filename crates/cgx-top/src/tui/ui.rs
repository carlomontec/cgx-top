use crate::ascii_plot::render_ascii_plot;
use crate::tui::app::{ActiveFieldView, AppState};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Wrap},
    Frame,
};

pub fn draw(f: &mut Frame, app: &AppState) {
    let size = f.area();

    // Top-level vertical split: Header, Main Body, Chart, Footer
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Min(8),    // Body (Steps + Details)
            Constraint::Length(10), // Bottom Chart
            Constraint::Length(1), // Footer
        ])
        .split(size);

    render_header(f, app, chunks[0]);
    render_body(f, app, chunks[1]);
    render_chart(f, app, chunks[2]);
    render_footer(f, app, chunks[3]);

    // Render modal overlays if active
    if app.show_set_modal {
        render_set_modal(f, app, size);
    } else if app.show_help_modal {
        render_help_modal(f, size);
    }
}

fn render_header(f: &mut Frame, app: &AppState, area: Rect) {
    let active_set_name = &app.available_sets[app.active_set_idx];
    let node_count_str = if let Some(ref nodes) = app.active_nodes {
        format!("{} nodes", nodes.len())
    } else {
        format!("{} nodes (all)", app.model.nodes.len())
    };

    let title_line = Line::from(vec![
        Span::styled(" cgx-top ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(&app.filename, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" ({:.2} KB) ", app.file_size_kb), Style::default().fg(Color::DarkGray)),
        Span::styled("│ ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("Nodes: {} ", app.model.nodes.len()), Style::default().fg(Color::Green)),
        Span::styled(format!("Elements: {} ", app.model.elements.len()), Style::default().fg(Color::Green)),
        Span::styled(format!("Steps: {} ", app.model.steps.len()), Style::default().fg(Color::Yellow)),
        Span::styled("│ Set: ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("[{}] ", active_set_name), Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::styled(format!("({}) ", node_count_str), Style::default().fg(Color::DarkGray)),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan));

    let p = Paragraph::new(title_line).block(block);
    f.render_widget(p, area);
}

fn render_body(f: &mut Frame, app: &AppState, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(32), // Left: Steps list
            Constraint::Percentage(68), // Right: Dataset details
        ])
        .split(area);

    render_steps_list(f, app, chunks[0]);
    render_step_details(f, app, chunks[1]);
}

fn render_steps_list(f: &mut Frame, app: &AppState, area: Rect) {
    let items: Vec<ListItem> = app
        .model
        .steps
        .iter()
        .enumerate()
        .map(|(idx, s)| {
            let is_selected = idx == app.selected_step_idx;
            let prefix = if is_selected { "▶ " } else { "  " };

            let label = if !s.description.is_empty() {
                format!("{}Step {:<3} (t={:.3e}) {}", prefix, s.step_number, s.value, s.description)
            } else {
                format!("{}Step {:<3} (val={:.3e})", prefix, s.step_number, s.value)
            };

            let style = if is_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            ListItem::new(label).style(style)
        })
        .collect();

    let title = format!(" Steps / Modes / Loads ({}) [W/S, 1-9] ", app.model.steps.len());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::White));

    let list = List::new(items).block(block);
    f.render_widget(list, area);
}

fn render_step_details(f: &mut Frame, app: &AppState, area: Rect) {
    let step_detail = app.compute_step_details(app.selected_step_idx);
    let active_set_name = &app.available_sets[app.active_set_idx];

    let mut lines = Vec::new();

    if let Some(detail) = step_detail {
        // Step Banner
        let mut banner_spans = vec![
            Span::styled(format!("Step {} ", detail.step_number), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(format!("│ Incr {} ", detail.increment_number), Style::default().fg(Color::White)),
            Span::styled(format!("│ Value: {:.6e} ", detail.value), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ];
        if !detail.description.is_empty() {
            banner_spans.push(Span::styled(format!("│ {} ", detail.description), Style::default().fg(Color::LightCyan)));
        }
        banner_spans.extend(vec![
            Span::styled(format!("│ Set: [{}] ", active_set_name), Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            Span::styled(format!("({} nodes)  ◄ A/D cycle (0=ALL) ►", detail.filtered_nodes_count), Style::default().fg(Color::DarkGray)),
        ]);
        lines.push(Line::from(banner_spans));
        lines.push(Line::from("─".repeat(area.width.saturating_sub(2) as usize)));

        // Displacement section
        if detail.has_disp && (app.field_view == ActiveFieldView::Auto || app.field_view == ActiveFieldView::Displacement) {
            lines.push(Line::from(vec![
                Span::styled("● NODAL DISPLACEMENT ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
                Span::styled(format!("(Field: DISP)"), Style::default().fg(Color::DarkGray)),
            ]));

            if let Some((max_val, max_node)) = detail.max_disp {
                let min_str = detail.min_disp.map(|(v, _)| format!("{:.4e}", v)).unwrap_or_else(|| "0.00e0".to_string());
                lines.push(Line::from(vec![
                    Span::styled("   Peak Magnitude (ALL): ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("{:.6e}", max_val), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" at Node #{}", max_node), Style::default().fg(Color::LightCyan)),
                    Span::styled(format!("  (Min: {})", min_str), Style::default().fg(Color::DarkGray)),
                ]));
            }

            for (comp, min_v, min_n, max_v, max_n) in &detail.disp_comps {
                lines.push(Line::from(vec![
                    Span::styled(format!("   {:<5} ", comp), Style::default().fg(Color::LightBlue)),
                    Span::styled(format!("min: {:<12.4e} (Node #{:<5})", min_v, min_n), Style::default().fg(Color::Gray)),
                    Span::styled(format!("  │  max: {:<12.4e} (Node #{})", max_v, max_n), Style::default().fg(Color::White)),
                ]));
            }
            lines.push(Line::default());
        }

        // Stress section
        if detail.has_stress && (app.field_view == ActiveFieldView::Auto || app.field_view == ActiveFieldView::Stress) {
            lines.push(Line::from(vec![
                Span::styled("● CAUCHY STRESS & INVARIANTS ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
                Span::styled(format!("(Field: STRESS)"), Style::default().fg(Color::DarkGray)),
            ]));

            if let Some((vm_max, vm_node)) = detail.max_von_mises {
                lines.push(Line::from(vec![
                    Span::styled("   von Mises: ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                    Span::styled(format!("{:.6e}", vm_max), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" at Node #{}", vm_node), Style::default().fg(Color::LightCyan)),
                ]));
            }

            if let (Some((s1_max, s1_node)), Some((s3_min, s3_node))) = (detail.max_s1, detail.min_s3) {
                lines.push(Line::from(vec![
                    Span::styled(format!("   S1 (Max Principal): {:<12.4e} (#{})  │  S3 (Min Principal): {:<12.4e} (#{})", s1_max, s1_node, s3_min, s3_node), Style::default().fg(Color::Gray)),
                ]));
            }

            if let Some((tresca_max, tr_node)) = detail.max_tresca {
                lines.push(Line::from(vec![
                    Span::styled(format!("   Tresca Max Shear:   {:<12.4e} (Node #{})", tresca_max, tr_node), Style::default().fg(Color::Gray)),
                ]));
            }

            // Component stresses
            let comp_strs: Vec<String> = detail.stress_comps.iter().take(6).map(|(name, min_v, _, max_v, _)| {
                format!("{}: [{:.2e} .. {:.2e}]", name, min_v, max_v)
            }).collect();
            if !comp_strs.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("   Components: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(comp_strs.join("  "), Style::default().fg(Color::DarkGray)),
                ]));
            }
            lines.push(Line::default());
        }

        // Other fields
        for (field_name, comps) in &detail.other_fields {
            lines.push(Line::from(vec![
                Span::styled(format!("● FIELD: {} ", field_name), Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            ]));
            for (comp, min_v, min_n, max_v, max_n) in comps {
                lines.push(Line::from(vec![
                    Span::styled(format!("   {:<8} ", comp), Style::default().fg(Color::LightMagenta)),
                    Span::styled(format!("min: {:<12.4e} (#{})  │  max: {:<12.4e} (#{})", min_v, min_n, max_v, max_n), Style::default().fg(Color::Gray)),
                ]));
            }
            lines.push(Line::default());
        }
    } else {
        lines.push(Line::from("No step data available."));
    }

    let title = format!(" Step Extrema & Invariant Details [Tab: {}] ", app.field_view.label());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::White));

    let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_chart(f: &mut Frame, app: &AppState, area: Rect) {
    let plot_cols = (area.width.saturating_sub(18) as usize).max(10);
    let plot_rows = (area.height.saturating_sub(3) as usize).max(4);

    let first_lbl = if !app.model.steps.is_empty() {
        format!("Step 1")
    } else {
        "".to_string()
    };
    let last_lbl = if !app.model.steps.is_empty() {
        format!("Step {}", app.model.steps.len())
    } else {
        "".to_string()
    };

    let plot_lines = render_ascii_plot(
        &app.chart_values,
        plot_cols,
        plot_rows,
        Some(&first_lbl),
        Some(&last_lbl),
    );

    let mut lines = Vec::new();
    for l in plot_lines {
        lines.push(Line::from(l));
    }

    // Add cursor indicator pointing to active step along X axis
    if !app.chart_values.is_empty() && app.chart_values.len() > 1 {
        let n = app.chart_values.len();
        let col_offset = ((app.selected_step_idx as f64 / (n - 1) as f64) * (plot_cols - 1) as f64).round() as usize;
        let left_pad = 13 + col_offset;
        lines.push(Line::from(vec![
            Span::styled(format!("{:>left_pad$}▲ (Step {})", "", app.selected_step_idx + 1), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]));
    }

    let title = format!(" {} History across Steps [Filter: {}] ", app.chart_label, app.available_sets[app.active_set_idx]);
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow));

    let p = Paragraph::new(lines).block(block);
    f.render_widget(p, area);
}

fn render_footer(f: &mut Frame, _app: &AppState, area: Rect) {
    let footer_line = Line::from(vec![
        Span::styled(" [W/S] ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled("Step  ", Style::default().fg(Color::White)),
        Span::styled(" [A/D] ", Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::styled("Rotate Set  ", Style::default().fg(Color::White)),
        Span::styled(" [0] ", Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::styled("All Nodes  ", Style::default().fg(Color::White)),
        Span::styled(" [1-9] ", Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled("Jump Step  ", Style::default().fg(Color::White)),
        Span::styled(" [Space] ", Style::default().fg(Color::Black).bg(Color::LightBlue).add_modifier(Modifier::BOLD)),
        Span::styled("Set List  ", Style::default().fg(Color::White)),
        Span::styled(" [Tab] ", Style::default().fg(Color::Black).bg(Color::LightYellow).add_modifier(Modifier::BOLD)),
        Span::styled("Field  ", Style::default().fg(Color::White)),
        Span::styled(" [?] ", Style::default().fg(Color::Black).bg(Color::Gray).add_modifier(Modifier::BOLD)),
        Span::styled("Help  ", Style::default().fg(Color::White)),
        Span::styled(" [q/Esc] ", Style::default().fg(Color::Black).bg(Color::Red).add_modifier(Modifier::BOLD)),
        Span::styled("Quit", Style::default().fg(Color::White)),
    ]);

    let p = Paragraph::new(footer_line).alignment(Alignment::Center);
    f.render_widget(p, area);
}

fn render_set_modal(f: &mut Frame, app: &AppState, area: Rect) {
    let modal_area = centered_rect(60, 65, area);
    f.render_widget(Clear, modal_area);

    let filtered = app.filtered_set_list();
    let items: Vec<ListItem> = filtered
        .iter()
        .enumerate()
        .map(|(idx, &(orig_idx, set_name))| {
            let is_active = orig_idx == app.active_set_idx;
            let is_cursor = idx == app.set_modal_cursor;

            let marker = if is_active { "✓ " } else { "  " };
            let count_info = if orig_idx == 0 {
                format!("({} nodes total)", app.model.nodes.len())
            } else if let Some(nodes) = app.model.resolve_set_node_ids(set_name) {
                format!("({} nodes)", nodes.len())
            } else {
                "(0 nodes)".to_string()
            };

            let text = format!("{}{:<25} {}", marker, set_name, count_info);

            let style = if is_cursor {
                Style::default().fg(Color::Black).bg(Color::Magenta).add_modifier(Modifier::BOLD)
            } else if is_active {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            ListItem::new(text).style(style)
        })
        .collect();

    let search_title = if app.set_search_query.is_empty() {
        " Select Active Node/Element Set (Type to filter, Enter to apply, Esc to close) ".to_string()
    } else {
        format!(" Search: '{}' (Enter to apply, Esc to close) ", app.set_search_query)
    };

    let block = Block::default()
        .title(search_title)
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Magenta));

    let list = List::new(items).block(block);
    f.render_widget(list, modal_area);
}

fn render_help_modal(f: &mut Frame, area: Rect) {
    let modal_area = centered_rect(60, 50, area);
    f.render_widget(Clear, modal_area);

    let help_text = vec![
        Line::from(vec![Span::styled("KEYBOARD SHORTCUTS & NAVIGATION", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]),
        Line::from("─".repeat(50)),
        Line::from(vec![
            Span::styled("  W  or  ↑         ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("Previous Step / Mode / Time Increment", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  S  or  ↓         ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("Next Step / Mode / Time Increment", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  A  or  ←         ", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            Span::styled("Rotate to Previous Set", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  D  or  →         ", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            Span::styled("Rotate to Next Set", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  0 (Zero)         ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled("Reset to [ALL NODES] (clear set filter)", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  1 - 9            ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("Jump directly to Step 1 through 9", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  Space / Enter    ", Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD)),
            Span::styled("Open Set Selector Dialog", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  Tab              ", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
            Span::styled("Switch active field view (All ↔ Disp ↔ Stress)", Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("  q  or  Esc       ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::styled("Quit cgx-top", Style::default().fg(Color::White)),
        ]),
    ];

    let block = Block::default()
        .title(" Help & Keybindings [Press Esc to Close] ")
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::default().fg(Color::Cyan));

    let p = Paragraph::new(help_text).block(block);
    f.render_widget(p, modal_area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}
