use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use cgx_core::{parse_frd_file, FrdModel};

/// The terminal companion to CalculiX GraphiX (CGX).
/// Fast, zero-dependency TUI for inspecting FEA results (.frd).
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Path to the CalculiX results file (.frd)
    #[arg(value_name = "FILE")]
    pub file: PathBuf,

    /// Print summary statistics to stdout without launching TUI
    #[arg(short, long)]
    pub summary: bool,

    /// Filter analysis to a specific node or element set (e.g. END_TIP_BLADE)
    #[arg(short = 'S', long, value_name = "SET_NAME")]
    pub set: Option<String>,

    /// Compute peak von Mises stress history across all steps
    #[arg(long)]
    pub mises: bool,

    /// Compute peak principal stresses (sigma_1, sigma_3) across all steps
    #[arg(long)]
    pub principal: bool,

    /// Output query results as comma-separated values (CSV)
    #[arg(long)]
    pub csv: bool,
}

fn print_summary(path: &PathBuf, model: &FrdModel) {
    let metadata = std::fs::metadata(path).ok();
    let file_size_kb = metadata.map(|m| m.len() as f64 / 1024.0).unwrap_or(0.0);

    println!("\n┌─ File Overview ─────────────────────────────────────────────────────────┐");
    println!("│ Path: {:<66} │", path.display());
    println!("│ Title: {:<65} │", if model.title.is_empty() { "(none)" } else { &model.title });
    println!("│ Size: {:<12.2} KB │ Nodes: {:<12} │ Elements: {:<14} │", file_size_kb, model.nodes.len(), model.elements.len());
    println!("└─────────────────────────────────────────────────────────────────────────┘");

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

    // Steps & Modes summary
    println!("\nSteps / Vibration Modes ({} total):", model.steps.len());
    println!("  {:<6} {:<16} {:<12} {:<30}", "Step", "Value (Hz/s)", "Fields", "Components");
    println!("  {}", "─".repeat(68));
    for step in &model.steps {
        let field_names: Vec<String> = step.datasets.iter().map(|d| d.name.clone()).collect();
        let all_comps: Vec<String> = step.datasets.iter()
            .flat_map(|d| d.components.iter().map(|c| c.name.clone()))
            .collect();
        println!(
            "  {:<6} {:<16.6} {:<12} {:<30}",
            step.step_number,
            step.value,
            field_names.join(", "),
            all_comps.join(", ")
        );
    }
    println!();
}

fn main() -> anyhow::Result<()> {
    let args = Cli::parse();

    if !args.file.exists() {
        eprintln!("Error: File not found: {}", args.file.display());
        std::process::exit(1);
    }

    let start = std::time::Instant::now();
    let model = parse_frd_file(&args.file)?;
    let duration = start.elapsed();

    println!("Loaded in {:.2?}", duration);

    if args.summary || true {
        print_summary(&args.file, &model);
    }

    Ok(())
}
