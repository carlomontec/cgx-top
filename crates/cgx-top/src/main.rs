mod ascii_plot;
mod sparkline;
mod tui;

use ascii_plot::render_ascii_plot;
use clap::{Parser, Subcommand};
use sparkline::render_sparkline;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use cgx_core::{
    consolidate_inp, diagnose, inspect_inp, parse_cvg, parse_frd_file, parse_sta, DiagCtx,
    FrdModel, InpSummary, StepInfo, Thresholds,
};

/// The terminal companion to CalculiX GraphiX (CGX).
/// Fast, zero-dependency TUI and CLI for inspecting FEA results (.frd) and input decks (.inp).
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Path to the primary file (CalculiX .frd results or .inp deck)
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,

    /// Optional companion solver input file (.inp), e.g. `cgx-top results.frd solverfile.inp`
    #[arg(value_name = "COMPANION_INP")]
    pub companion: Option<PathBuf>,

    /// Quick-look inspect an .inp deck (parity with `cgx -c solverfile.inp`)
    #[arg(short = 'c', long)]
    pub check: bool,

    /// Consolidate all *INCLUDE files in an .inp deck into a single standalone file
    #[arg(long)]
    pub consolidate: bool,

    /// Output file path for consolidation (defaults to <stem>_consolidated.inp)
    #[arg(short = 'o', long, value_name = "OUTPUT_FILE")]
    pub output: Option<PathBuf>,

    /// Print summary statistics to stdout without launching TUI
    #[arg(short, long)]
    pub summary: bool,

    /// Filter analysis to a specific node or element set (e.g. END_TIP_BLADE, +C3D20)
    #[arg(short = 'S', long, value_name = "SET_NAME")]
    pub set: Option<String>,

    /// Compute peak von Mises stress history across all steps
    #[arg(long)]
    pub mises: bool,

    /// Compute peak principal stresses (sigma_1, sigma_3) across all steps
    #[arg(long)]
    pub principal: bool,

    /// Compute peak displacement magnitude history across all steps
    #[arg(long)]
    pub disp: bool,

    /// Output query results as comma-separated values (CSV)
    #[arg(long)]
    pub csv: bool,

    /// Print all time steps/increments in tables without decimation
    #[arg(short = 'a', long)]
    pub all: bool,

    /// Show equidistant time steps/increments across the timeline (defaults to 25 points)
    #[arg(short = 'e', long = "equidistant", visible_aliases = ["samples", "sample"], value_name = "COUNT", num_args = 0..=1, default_missing_value = "25")]
    pub equidistant: Option<usize>,

    /// Suppress the ASCII/Braille history plot
    #[arg(long)]
    pub no_plot: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Consolidate all *INCLUDE files in an .inp deck into a single standalone file
    Consolidate {
        /// The root .inp file to consolidate
        #[arg(value_name = "INP_FILE")]
        file: PathBuf,

        /// Output consolidated .inp file (defaults to <stem>_consolidated.inp)
        #[arg(short = 'o', long, value_name = "OUTPUT_FILE")]
        output: Option<PathBuf>,
    },
    /// Inspect an .inp deck dependencies and sets (equivalent to -c)
    Check {
        /// The .inp file to inspect
        #[arg(value_name = "INP_FILE")]
        file: PathBuf,
    },
    /// Diagnose solver convergence from a job's .sta/.cvg telemetry
    Diagnose {
        /// Job path, with or without a .inp/.sta/.cvg extension
        #[arg(value_name = "JOB")]
        job: PathBuf,
    },
}

/// Reads `(step_number, converged)` pairs from a ccx `<job>.json` without a JSON parser.
fn read_json_steps(path: &Path) -> Vec<(u32, bool)> {
    let Ok(text) = fs::read_to_string(path) else { return Vec::new() };
    let num = |s: &str| s.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect::<String>();
    text.split("\"step_number\":")
        .skip(1)
        .filter_map(|chunk| {
            let step = num(chunk).parse().ok()?;
            let conv = chunk.split("\"converged\":").nth(1)?.trim_start();
            Some((step, conv.starts_with("true")))
        })
        .collect()
}

fn execute_diagnose(job: &Path) -> anyhow::Result<()> {
    let is_job_ext = job
        .extension()
        .and_then(|e| e.to_str())
        .map_or(false, |e| ["inp", "sta", "cvg"].iter().any(|x| e.eq_ignore_ascii_case(x)));
    let base = if is_job_ext { job.with_extension("") } else { job.to_path_buf() };
    let read = |ext: &str| fs::read_to_string(format!("{}.{}", base.display(), ext)).unwrap_or_default();
    let sta = parse_sta(&read("sta"));
    let cvg = parse_cvg(&read("cvg"));
    let ctx = DiagCtx {
        running: false,
        exit_failed: false,
        json_steps: read_json_steps(&PathBuf::from(format!("{}.json", base.display()))),
        stdout_tail: String::new(),
    };
    let d = diagnose(&sta, &cvg, &ctx, &Thresholds::default(), None, 10);

    println!("Job:     {}", base.display());
    println!("Verdict: {}", d.verdict.as_str());
    println!("Rows:    {} .sta, {} .cvg", sta.len(), cvg.len());
    for f in &d.findings {
        let at = match (f.step, f.inc, f.att) {
            (Some(s), Some(i), Some(a)) => format!("step {s} inc {i} att {a}"),
            _ => "-".to_string(),
        };
        let ev: Vec<String> = f.evidence.iter().map(|(k, v)| format!("{k}={v:.4}")).collect();
        println!("\n[{}] {} at {} (x{})", f.rule, f.severity.as_str(), at, f.occurrences);
        if !ev.is_empty() {
            println!("  evidence: {}", ev.join(" "));
        }
        if !f.advice.is_empty() {
            println!("  advice:   {}", f.advice);
        }
    }
    for m in &d.solver_messages {
        println!("\nsolver: {m}");
    }
    println!("\nLast {} .sta rows:", sta.len().min(10));
    println!("  {:>5} {:>5} {:>4} {:>5} {:>13} {:>13} {:>13}", "STEP", "INC", "ATT", "ITRS", "TOT TIME", "STEP TIME", "INC TIME");
    for r in &sta[sta.len().saturating_sub(10)..] {
        println!("  {:>5} {:>5} {:>4} {:>5} {:>13.6e} {:>13.6e} {:>13.6e}", r.step, r.inc, r.att, r.itrs, r.tot_time, r.step_time, r.inc_time);
    }
    Ok(())
}

fn execute_consolidate(inp_path: &Path, output_path: Option<PathBuf>) -> anyhow::Result<()> {
    if !inp_path.exists() {
        eprintln!("Error: File not found: {}", inp_path.display());
        std::process::exit(1);
    }

    let default_output = if let Some(out) = output_path {
        out
    } else {
        let parent = inp_path.parent().unwrap_or_else(|| Path::new("."));
        let stem = inp_path.file_stem().and_then(|s| s.to_str()).unwrap_or("model");
        parent.join(format!("{}_consolidated.inp", stem))
    };

    println!("Consolidating input deck: {}", inp_path.display());
    let start = std::time::Instant::now();
    let (consolidated_text, stats) = consolidate_inp(inp_path)?;
    fs::write(&default_output, &consolidated_text)?;
    let elapsed = start.elapsed();

    println!("  ✓ Successfully inlined {} included files", stats.total_files_inlined);
    for inc in &stats.inlined_files {
        println!("    - {}", inc.display());
    }
    println!(
        "\nConsolidated deck written to: {} ({} lines, took {:.2?})\n",
        default_output.display(),
        stats.total_lines_written,
        elapsed
    );

    Ok(())
}

const BOX_INNER_WIDTH: usize = 73;

fn print_box_top(title: &str) {
    let prefix = format!("┌─ {} ", title);
    let p_len = prefix.chars().count();
    if p_len >= BOX_INNER_WIDTH + 1 {
        let truncated: String = prefix.chars().take(BOX_INNER_WIDTH).collect();
        println!("{}─┐", truncated);
    } else {
        let dashes = (BOX_INNER_WIDTH + 1) - p_len;
        println!("{}{}┐", prefix, "─".repeat(dashes));
    }
}

fn print_box_divider() {
    println!("├{}┤", "─".repeat(BOX_INNER_WIDTH));
}

fn print_box_bottom() {
    println!("└{}┘", "─".repeat(BOX_INNER_WIDTH));
}

fn print_box_row(content: &str) {
    let inner_content_width = BOX_INNER_WIDTH.saturating_sub(2);
    let char_count = content.chars().count();
    if char_count > inner_content_width {
        let truncated: String = content.chars().take(inner_content_width.saturating_sub(3)).collect();
        println!("│ {}... │", truncated);
    } else {
        let pad = inner_content_width - char_count;
        println!("│ {}{} │", content, " ".repeat(pad));
    }
}

fn execute_inp_check(inp_path: &Path) -> anyhow::Result<()> {
    if !inp_path.exists() {
        eprintln!("Error: File not found: {}", inp_path.display());
        std::process::exit(1);
    }

    let summary: InpSummary = inspect_inp(inp_path)?;

    println!();
    print_box_top(&format!("CalculiX Input Deck Inspection: {}", inp_path.file_name().and_then(|s| s.to_str()).unwrap_or("")));
    print_box_row(&format!("Path: {}", inp_path.display()));
    print_box_row(&format!(
        "Total Lines: {:<10} │ Total Files: {:<10} │ Steps: {:<12}",
        summary.total_lines, summary.total_files_count, summary.step_count
    ));
    print_box_divider();

    if summary.includes.is_empty() {
        print_box_row("Included Files: None (standalone deck)");
    } else {
        print_box_row(&format!("Included Files ({} total):", summary.includes.len()));
        for inc in &summary.includes {
            let status = if inc.exists { "✓" } else { "✗ MISSING:" };
            let size_kb = inc.file_size_bytes as f64 / 1024.0;
            print_box_row(&format!("  {} {:<45} ({:>8.1} KB)", status, inc.raw_target, size_kb));
        }
    }

    print_box_divider();
    let mut features = Vec::new();
    if summary.has_nodes { features.push("Nodes"); }
    if summary.has_elements { features.push("Elements"); }
    if summary.boundary_count > 0 { features.push("Boundary Conditions"); }
    if !summary.materials.is_empty() { features.push("Materials"); }
    print_box_row(&format!("Features: {}", features.join(", ")));

    if !summary.node_sets.is_empty() {
        print_box_divider();
        print_box_row(&format!("Node Sets ({} total):", summary.node_sets.len()));
        let mut nsets: Vec<_> = summary.node_sets.iter().collect();
        nsets.sort_by_key(|(k, _)| (*k).clone());
        for (name, count) in nsets.iter().take(20) {
            print_box_row(&format!("  {:<35} {:>10} nodes", name, count));
        }
        if nsets.len() > 20 {
            print_box_row(&format!("  ... and {} more node sets", nsets.len() - 20));
        }
    }

    if !summary.element_sets.is_empty() {
        print_box_divider();
        print_box_row(&format!("Element Sets ({} total):", summary.element_sets.len()));
        let mut esets: Vec<_> = summary.element_sets.iter().collect();
        esets.sort_by_key(|(k, _)| (*k).clone());
        for (name, count) in esets.iter().take(20) {
            print_box_row(&format!("  {:<35} {:>10} elements", name, count));
        }
        if esets.len() > 20 {
            print_box_row(&format!("  ... and {} more element sets", esets.len() - 20));
        }
    }

    print_box_bottom();
    println!();
    Ok(())
}

fn select_representative_indices(
    total: usize,
    target_count: usize,
    peak_idx: Option<usize>,
) -> Vec<usize> {
    if total <= target_count {
        return (0..total).collect();
    }

    let mut indices = HashSet::new();
    indices.insert(0);
    indices.insert(total - 1);
    if let Some(p) = peak_idx {
        indices.insert(p);
    }

    let intervals = target_count.saturating_sub(1).max(1);
    for k in 1..intervals {
        let idx = (k as f64 * (total - 1) as f64 / intervals as f64).round() as usize;
        if idx < total {
            indices.insert(idx);
        }
    }

    let mut sorted: Vec<usize> = indices.into_iter().collect();
    sorted.sort_unstable();
    sorted
}

fn print_summary(path: &Path, model: &FrdModel, show_all_steps: bool, sample_count: Option<usize>) {
    let metadata = std::fs::metadata(path).ok();
    let file_size_kb = metadata.map(|m| m.len() as f64 / 1024.0).unwrap_or(0.0);

    println!();
    print_box_top("File Overview");
    print_box_row(&format!("Path: {}", path.display()));
    print_box_row(&format!("Title: {}", if model.title.is_empty() { "(none)" } else { &model.title }));
    print_box_row(&format!(
        "Size: {:<10.2} KB │ Nodes: {:<12} │ Elements: {:<12}",
        file_size_kb, model.nodes.len(), model.elements.len()
    ));

    if let (Some(first), Some(last)) = (model.steps.first(), model.steps.last()) {
        let field_names: HashSet<String> = model.steps.iter()
            .flat_map(|s| s.datasets.iter().map(|d| d.name.clone()))
            .collect();
        let mut sorted_fields: Vec<_> = field_names.into_iter().collect();
        sorted_fields.sort();
        let fields_display = if sorted_fields.is_empty() { "none".to_string() } else { sorted_fields.join(", ") };

        if model.steps.len() > 1 {
            print_box_row(&format!(
                "Steps: {:<9} │ Range: {:.4e} -> {:.4e}",
                model.steps.len(), first.value, last.value
            ));
        } else {
            print_box_row(&format!(
                "Steps: 1         │ Value: {:.4e}",
                first.value
            ));
        }
        print_box_row(&format!("Fields: {}", fields_display));
    }
    print_box_bottom();

    // Element topology breakdown
    let mut elem_counts: HashMap<String, usize> = HashMap::new();
    for el in &model.elements {
        *elem_counts.entry(format!("{:?}", el.elem_type)).or_insert(0) += 1;
    }
    print!("Elements by type: ");
    let mut counts_vec: Vec<_> = elem_counts.into_iter().collect();
    counts_vec.sort_by(|a, b| b.1.cmp(&a.1));
    for (name, count) in counts_vec {
        print!("{}: {}  ", name, count);
    }
    println!();

    // Available Sets
    if !model.sets.is_empty() {
        println!("\nAvailable Sets ({} total):", model.sets.len());
        let mut set_names: Vec<_> = model.sets.keys().cloned().collect();
        set_names.sort();
        let display_limit = 20;
        for chunk in set_names.chunks(4) {
            println!("  {}", chunk.join("    "));
            if chunk.len() > display_limit { break; }
        }
    }

    // Steps & Modes summary
    if !model.steps.is_empty() {
        let format_step_row = |s: &StepInfo| {
            let field_names: Vec<String> = s.datasets.iter().map(|d| d.name.clone()).collect();
            let all_comps: Vec<String> = s.datasets.iter()
                .flat_map(|d| d.components.iter().map(|c| c.name.clone()))
                .collect();
            format!(
                "  {:<6} {:<6} {:<16.6e} {:<12} {:<30}",
                s.step_number,
                s.increment_number,
                s.value,
                field_names.join(", "),
                all_comps.join(", ")
            )
        };

        if show_all_steps || model.steps.len() <= 15 {
            println!("\nSteps / Increments ({} total):", model.steps.len());
            println!("  {:<6} {:<6} {:<16} {:<12} {:<30}", "Step", "Incr", "Value", "Fields", "Components");
            println!("  {}", "─".repeat(76));
            for step in &model.steps {
                println!("{}", format_step_row(step));
            }
        } else if let Some(n_samples) = sample_count {
            let chosen = select_representative_indices(model.steps.len(), n_samples, None);
            println!("\nSteps / Increments ({} total, showing {} equidistant steps):", model.steps.len(), chosen.len());
            println!("  {:<6} {:<6} {:<16} {:<12} {:<30}", "Step", "Incr", "Value", "Fields", "Components");
            println!("  {}", "─".repeat(76));
            for idx in chosen {
                println!("{}", format_step_row(&model.steps[idx]));
            }
        } else {
            println!("\nSteps / Increments ({} total, showing head & tail):", model.steps.len());
            println!("  {:<6} {:<6} {:<16} {:<12} {:<30}", "Step", "Incr", "Value", "Fields", "Components");
            println!("  {}", "─".repeat(76));
            for step in &model.steps[..5] {
                println!("{}", format_step_row(step));
            }
            let skipped = model.steps.len() - 10;
            println!("  ...    ...    ({} intermediate steps omitted, use -e/--samples or -a/--all)", skipped);
            for step in &model.steps[model.steps.len() - 5..] {
                println!("{}", format_step_row(step));
            }
        }
        println!();
    }
}

fn report_stress_history(
    model: &FrdModel,
    filter_nodes: Option<&HashSet<u32>>,
    set_name: Option<&str>,
    principal: bool,
    csv: bool,
    show_all: bool,
    sample_count: Option<usize>,
    no_plot: bool,
) {
    let history = model.compute_stress_history(filter_nodes);
    if history.is_empty() {
        let available_fields: HashSet<String> = model.steps.iter()
            .flat_map(|s| s.datasets.iter().map(|d| d.name.clone()))
            .collect();
        let fields_str = if available_fields.is_empty() {
            "none".to_string()
        } else {
            let mut list: Vec<_> = available_fields.into_iter().collect();
            list.sort();
            list.join(", ")
        };
        println!("No stress datasets found in model. Available fields in file: {}", fields_str);
        return;
    }

    if csv {
        if principal {
            println!("step,increment,value,max_s1,max_s1_node,min_s3,min_s3_node,max_mises,max_mises_node,nodes_count");
            for s in &history {
                println!(
                    "{},{},{:.6e},{:.6e},{},{:.6e},{},{:.6e},{},{}",
                    s.step_number, s.increment_number, s.value,
                    s.max_principal_s1.0, s.max_principal_s1.1,
                    s.min_principal_s3.0, s.min_principal_s3.1,
                    s.max_von_mises.0, s.max_von_mises.1,
                    s.node_count
                );
            }
        } else {
            println!("step,increment,value,max_mises,max_mises_node,min_mises,min_mises_node,max_tresca,nodes_count");
            for s in &history {
                println!(
                    "{},{},{:.6e},{:.6e},{},{:.6e},{},{:.6e},{}",
                    s.step_number, s.increment_number, s.value,
                    s.max_von_mises.0, s.max_von_mises.1,
                    s.min_von_mises.0, s.min_von_mises.1,
                    s.max_tresca.0,
                    s.node_count
                );
            }
        }
        return;
    }

    let filter_str = if let Some(name) = set_name {
        format!("Set '{}' ({} nodes)", name, history[0].node_count)
    } else {
        format!("All Nodes ({} nodes)", history[0].node_count)
    };

    let (peak_label, global_peak_val, peak_node, peak_step, peak_incr, peak_step_val, peak_idx) = if principal {
        let mut max = (-f32::INFINITY, 0, 0, 0, 0.0_f32);
        let mut p_idx = 0;
        for (idx, s) in history.iter().enumerate() {
            if s.max_principal_s1.0 > max.0 {
                max = (s.max_principal_s1.0, s.max_principal_s1.1, s.step_number, s.increment_number, s.value);
                p_idx = idx;
            }
        }
        ("S1", max.0, max.1, max.2, max.3, max.4, p_idx)
    } else {
        let mut max = (-f32::INFINITY, 0, 0, 0, 0.0_f32);
        let mut p_idx = 0;
        for (idx, s) in history.iter().enumerate() {
            if s.max_von_mises.0 > max.0 {
                max = (s.max_von_mises.0, s.max_von_mises.1, s.step_number, s.increment_number, s.value);
                p_idx = idx;
            }
        }
        ("von Mises", max.0, max.1, max.2, max.3, max.4, p_idx)
    };

    let target_samples = sample_count.unwrap_or(25);
    let is_decimated = !show_all && (history.len() > target_samples || sample_count.is_some());

    println!();
    print_box_top(if principal {
        "Principal Stresses Extrema History"
    } else {
        "von Mises Stress Extrema History"
    });
    print_box_row(&format!("Filter: {}", filter_str));
    print_box_row(&format!(
        "Peak {}: {:.4e} (Step {}, Incr {}, Node {}, Val: {:.4e})",
        peak_label, global_peak_val, peak_step, peak_incr, peak_node, peak_step_val
    ));

    if is_decimated {
        print_box_row(&format!(
            "Timeline: {} steps (showing {} representative steps, * = peak, use -a/--all to show all)",
            history.len(),
            if history.len() <= target_samples { history.len() } else { target_samples }
        ));
    }

    if principal {
        println!("├──────┬──────┬──────────────┬──────────────────┬────────┬──────────────────┬────────┤");
        println!("│ Step │ Incr │ Value        │ Peak S1          │ Node   │ Min S3           │ Node   │");
        println!("├──────┼──────┼──────────────┼──────────────────┼────────┼──────────────────┼────────┤");
        let chosen_indices = if is_decimated {
            select_representative_indices(history.len(), target_samples, Some(peak_idx))
        } else {
            (0..history.len()).collect()
        };
        for &idx in &chosen_indices {
            let s = &history[idx];
            let marker = if idx == peak_idx && is_decimated { "*" } else { " " };
            println!(
                "│ {:<4} │ {:<4} │ {:<12.4e} │ {:<15.6e}{} │ {:<6} │ {:<16.6e} │ {:<6} │",
                s.step_number, s.increment_number, s.value,
                s.max_principal_s1.0, marker, s.max_principal_s1.1,
                s.min_principal_s3.0, s.min_principal_s3.1
            );
        }
    } else {
        println!("├──────┬──────┬──────────────┬──────────────────┬────────┬────────────────┤");
        println!("│ Step │ Incr │ Value        │ Peak Mises       │ Node   │ Peak Tresca    │");
        println!("├──────┼──────┼──────────────┼──────────────────┼────────┼────────────────┤");
        let chosen_indices = if is_decimated {
            select_representative_indices(history.len(), target_samples, Some(peak_idx))
        } else {
            (0..history.len()).collect()
        };
        for &idx in &chosen_indices {
            let s = &history[idx];
            let marker = if idx == peak_idx && is_decimated { "*" } else { " " };
            println!(
                "│ {:<4} │ {:<4} │ {:<12.4e} │ {:<15.6e}{} │ {:<6} │ {:<14.6e} │",
                s.step_number, s.increment_number, s.value,
                s.max_von_mises.0, marker, s.max_von_mises.1, s.max_tresca.0
            );
        }
    }
    println!("├──────┴──────┴──────────────┴──────────────────┴────────┴────────────────┤");

    let peak_vals: Vec<f32> = if principal {
        history.iter().map(|s| s.max_principal_s1.0).collect()
    } else {
        history.iter().map(|s| s.max_von_mises.0).collect()
    };

    if !no_plot && history.len() > 5 {
        let first_lbl = format!("Step {}", history.first().unwrap().step_number);
        let last_lbl = format!("Step {}", history.last().unwrap().step_number);
        let plot_lines = render_ascii_plot(&peak_vals, 56, 7, Some(&first_lbl), Some(&last_lbl));
        for line in plot_lines {
            print_box_row(&line);
        }
        print_box_divider();
    }

    let spark = render_sparkline(&peak_vals);
    let min_val = peak_vals.iter().copied().fold(f32::INFINITY, f32::min);
    let max_val = peak_vals.iter().copied().fold(-f32::INFINITY, f32::max);
    let label = if principal { "Peak S1" } else { "Peak von Mises" };
    let trend_str = format!("Trend ({}): {} (Min: {:.2e}, Max: {:.2e})", label, spark, min_val, max_val);
    print_box_row(&trend_str);
    print_box_bottom();
    println!();
}

fn report_disp_history(
    model: &FrdModel,
    filter_nodes: Option<&HashSet<u32>>,
    set_name: Option<&str>,
    csv: bool,
    show_all: bool,
    sample_count: Option<usize>,
    no_plot: bool,
) {
    let history = model.compute_disp_history(filter_nodes);
    if history.is_empty() {
        let available_fields: HashSet<String> = model.steps.iter()
            .flat_map(|s| s.datasets.iter().map(|d| d.name.clone()))
            .collect();
        let fields_str = if available_fields.is_empty() {
            "none".to_string()
        } else {
            let mut list: Vec<_> = available_fields.into_iter().collect();
            list.sort();
            list.join(", ")
        };
        println!("No displacement datasets found in model. Available fields in file: {}", fields_str);
        return;
    }

    if csv {
        println!("step,increment,value,max_disp,max_node,min_disp,min_node,nodes_count");
        for s in &history {
            println!(
                "{},{},{:.6e},{:.6e},{},{:.6e},{},{}",
                s.step_number, s.increment_number, s.value,
                s.max_disp.0, s.max_disp.1,
                s.min_disp.0, s.min_disp.1,
                s.node_count
            );
        }
        return;
    }

    let filter_str = if let Some(name) = set_name {
        format!("Set '{}' ({} nodes)", name, history[0].node_count)
    } else {
        format!("All Nodes ({} nodes)", history[0].node_count)
    };

    let mut global_max = (-f32::INFINITY, 0, 0, 0, 0.0_f32);
    let mut peak_idx = 0;
    for (idx, s) in history.iter().enumerate() {
        if s.max_disp.0 > global_max.0 {
            global_max = (s.max_disp.0, s.max_disp.1, s.step_number, s.increment_number, s.value);
            peak_idx = idx;
        }
    }

    let target_samples = sample_count.unwrap_or(25);
    let is_decimated = !show_all && (history.len() > target_samples || sample_count.is_some());

    println!();
    print_box_top("Displacement Magnitude Extrema History");
    print_box_row(&format!("Filter: {}", filter_str));
    print_box_row(&format!(
        "Peak Disp: {:.4e} (Step {}, Incr {}, Node {}, Val: {:.4e})",
        global_max.0, global_max.2, global_max.3, global_max.1, global_max.4
    ));
    if is_decimated {
        let chosen_count = select_representative_indices(history.len(), target_samples, Some(peak_idx)).len();
        print_box_row(&format!(
            "Timeline: {} steps (showing {} representative steps, * = peak, use -a/--all to view all)",
            history.len(), chosen_count
        ));
    }
    println!("├──────┬──────┬──────────────┬──────────────────┬────────┬────────────────┤");
    println!("│ Step │ Incr │ Value        │ Peak Disp        │ Node   │ Min Disp       │");
    println!("├──────┼──────┼──────────────┼──────────────────┼────────┼────────────────┤");

    let chosen_indices = if is_decimated {
        select_representative_indices(history.len(), target_samples, Some(peak_idx))
    } else {
        (0..history.len()).collect()
    };

    for &idx in &chosen_indices {
        let s = &history[idx];
        let marker = if idx == peak_idx && is_decimated { "*" } else { " " };
        println!(
            "│ {:<4} │ {:<4} │ {:<12.4e} │ {:<15.6e}{} │ {:<6} │ {:<14.6e} │",
            s.step_number, s.increment_number, s.value,
            s.max_disp.0, marker, s.max_disp.1, s.min_disp.0
        );
    }
    println!("├──────┴──────┴──────────────┴──────────────────┴────────┴────────────────┤");

    let peak_vals: Vec<f32> = history.iter().map(|s| s.max_disp.0).collect();

    if !no_plot && history.len() > 5 {
        let first_lbl = format!("Step {}", history.first().unwrap().step_number);
        let last_lbl = format!("Step {}", history.last().unwrap().step_number);
        let plot_lines = render_ascii_plot(&peak_vals, 56, 7, Some(&first_lbl), Some(&last_lbl));
        for line in plot_lines {
            print_box_row(&line);
        }
        print_box_divider();
    }

    let spark = render_sparkline(&peak_vals);
    let min_val = peak_vals.iter().copied().fold(f32::INFINITY, f32::min);
    let max_val = peak_vals.iter().copied().fold(-f32::INFINITY, f32::max);
    let trend_str = format!("Trend (Peak Disp): {} (Min: {:.2e}, Max: {:.2e})", spark, min_val, max_val);
    print_box_row(&trend_str);
    print_box_bottom();
    println!();
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    // 1. Check for subcommands
    if let Some(cmd) = args.command {
        match cmd {
            Commands::Consolidate { file, output } => {
                return execute_consolidate(&file, output);
            }
            Commands::Check { file } => {
                return execute_inp_check(&file);
            }
            Commands::Diagnose { job } => {
                return execute_diagnose(&job);
            }
        }
    }

    let primary_file = match args.file {
        Some(f) => f,
        None => {
            eprintln!("Error: No input file specified. Run with --help for usage.");
            std::process::exit(1);
        }
    };

    let is_inp = primary_file.extension().and_then(|s| s.to_str()).map(|ext| ext.eq_ignore_ascii_case("inp")).unwrap_or(false);

    // 2. Handle consolidation flag on primary file
    if args.consolidate {
        return execute_consolidate(&primary_file, args.output);
    }

    // 3. Handle check mode or standalone .inp file
    if is_inp || args.check {
        return execute_inp_check(&primary_file);
    }

    // 4. FRD Post-processing mode
    let start = std::time::Instant::now();
    let mut model = parse_frd_file(&primary_file)?;
    let load_time = start.elapsed();

    // If a companion .inp was passed as second argument: `cgx-top results.frd solverfile.inp`
    if let Some(comp) = args.companion {
        if comp.exists() {
            let loaded = model.load_inp_sets(&comp)?;
            println!("Loaded {} sets from companion .inp {}", loaded, comp.display());
        } else {
            eprintln!("Warning: Companion .inp not found at {}", comp.display());
        }
    }

    // Resolve set filter if requested
    let filter_nodes = if let Some(ref sname) = args.set {
        match model.resolve_set_node_ids(sname) {
            Some(nodes) => Some(nodes),
            None => {
                eprintln!("Error: Set '{}' not found in model or companion sets.", sname);
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    if args.mises || args.principal {
        report_stress_history(
            &model,
            filter_nodes.as_ref(),
            args.set.as_deref(),
            args.principal,
            args.csv,
            args.all,
            args.equidistant,
            args.no_plot,
        );
    } else if args.disp {
        report_disp_history(
            &model,
            filter_nodes.as_ref(),
            args.set.as_deref(),
            args.csv,
            args.all,
            args.equidistant,
            args.no_plot,
        );
    } else if !std::io::stdout().is_terminal() || args.summary || args.all || args.equidistant.is_some() {
        // Non-interactive batch summary view
        println!("Loaded in {:.2?}", load_time);
        print_summary(&primary_file, &model, args.all, args.equidistant);
    } else {
        // Launch Interactive Terminal TUI Dashboard!
        tui::run_tui(model, &primary_file, args.set.as_deref())?;
    }

    Ok(())
}
