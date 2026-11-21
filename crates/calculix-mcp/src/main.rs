//! `calculix-mcp`: native MCP server for CalculiX decks, solver telemetry and results.
//!
//! stdout carries JSON-RPC only; all diagnostics go to stderr.
#![deny(clippy::print_stdout)]

mod ccx_tools;
mod cgx_tools;
mod contact_tools;
mod edit_tools;
mod error;
mod frd_tools;
mod inp_tools;
mod jobs;
mod mesh_tools;
mod paths;
mod server;

use clap::Parser;
use rmcp::ServiceExt;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Directory the server may read and write. Repeatable. Default: current directory
    #[arg(long = "root", value_name = "DIR")]
    roots: Vec<PathBuf>,

    /// Solver binary as <name>=<path>, e.g. --ccx mumps=/opt/ccx/build_mumps/CalculiX or --ccx ccx=/usr/bin/ccx. Stock ccx and CalculiX MultiSolver both work (MultiSolver adds JSON telemetry). Repeatable; the first is the default
    #[arg(long = "ccx", value_name = "NAME=PATH")]
    ccx: Vec<String>,

    /// Environment variable to pass through to the solver (e.g. DYLD_LIBRARY_PATH). Repeatable
    #[arg(long = "env-passthrough", value_name = "NAME")]
    env_passthrough: Vec<String>,

    /// Largest size in GiB a job's run directory may reach before the job is killed with DISK_QUOTA
    #[arg(long = "max-run-gb", value_name = "GIB", default_value_t = 50.0)]
    max_run_gb: f64,

    /// CalculiX GraphiX binary for cgx_run_batch (runs scripts with -bg). Without it the tool is disabled
    #[arg(long = "cgx", value_name = "PATH")]
    cgx: Option<PathBuf>,
}

/// Validate `--cgx PATH`: the binary must exist; it is stored canonicalised.
fn parse_cgx(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let binary = path.canonicalize().map_err(|e| anyhow::anyhow!("--cgx {}: {}", path.display(), e))?;
    anyhow::ensure!(binary.is_file(), "--cgx {} is not a file", binary.display());
    Ok(binary)
}

/// Parse and validate `--ccx name=path`: the binary must exist; it is stored canonicalised.
fn parse_solvers(specs: &[String]) -> anyhow::Result<Vec<jobs::Solver>> {
    specs
        .iter()
        .map(|spec| {
            let (name, path) = spec.split_once('=').ok_or_else(|| anyhow::anyhow!("--ccx expects NAME=PATH, got '{}'", spec))?;
            let binary = PathBuf::from(path).canonicalize().map_err(|e| anyhow::anyhow!("solver '{}': {}: {}", name, path, e))?;
            anyhow::ensure!(binary.is_file(), "solver '{}': {} is not a file", name, binary.display());
            anyhow::ensure!(!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'), "bad solver name '{}'", name);
            let json = advertises_json(&binary);
            Ok(jobs::Solver { name: name.to_string(), path: binary, json })
        })
        .collect()
}

/// Startup classification: MultiSolver builds contain the `CCX_JSON` marker string (the
/// environment variable that also switches JSON telemetry on); stock ccx does not. A wrapper
/// script around a MultiSolver build would not contain it and is therefore treated as stock.
fn advertises_json(path: &std::path::Path) -> bool {
    std::fs::read(path).map(|b| b.windows(8).any(|w| w == b"CCX_JSON")).unwrap_or(false)
}

/// The one-line stderr description of a classified solver.
fn solver_line(s: &jobs::Solver) -> String {
    if s.json {
        format!("solver '{}': CalculiX MultiSolver (JSON telemetry via -j)", s.name)
    } else {
        format!("solver '{}': stock CalculiX (no JSON telemetry; CalculiX MultiSolver adds per-step convergence, timings and mode tables)", s.name)
    }
}

/// Resolves when the process is asked to stop (Ctrl-C, or SIGTERM on Unix).
async fn stop_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let roots = if args.roots.is_empty() { vec![std::env::current_dir()?] } else { args.roots };
    let roots = paths::Roots::new(roots)?;
    eprintln!("calculix-mcp {}: serving on stdio, root {}", env!("CARGO_PKG_VERSION"), roots.first().display());

    let solvers = parse_solvers(&args.ccx)?;
    if solvers.is_empty() {
        eprintln!("solvers: none (ccx_solve disabled; pass --ccx NAME=PATH)");
    }
    for s in &solvers {
        eprintln!("{}", solver_line(s));
    }
    anyhow::ensure!(args.max_run_gb.is_finite() && args.max_run_gb > 0.0, "--max-run-gb must be a positive number");
    let max_run_bytes = (args.max_run_gb * 1024.0 * 1024.0 * 1024.0) as u64;
    let jobs = jobs::JobManager::new(roots.first(), solvers, args.env_passthrough.clone(), std::time::Duration::from_secs(5))
        .with_disk_limit(max_run_bytes, std::time::Duration::from_secs(10));

    let mut server = server::CalculixServer::new(roots, jobs.clone());
    match &args.cgx {
        Some(path) => {
            let binary = parse_cgx(path)?;
            eprintln!("cgx: {}", binary.display());
            server = server.with_cgx(cgx_tools::CgxConfig {
                binary,
                env_passthrough: args.env_passthrough.clone(),
                max_run_bytes,
                grace: std::time::Duration::from_secs(5),
            });
        }
        None => eprintln!("cgx: none (cgx_run_batch disabled; pass --cgx PATH)"),
    }
    let service = server.serve(rmcp::transport::stdio()).await?;
    // Stop on client disconnect (stdin EOF) or a termination signal; never leave a solver behind.
    tokio::select! {
        _ = service.waiting() => {}
        _ = stop_signal() => {}
    }
    jobs.shutdown().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solvers_are_classified_by_the_json_marker() {
        let dir = std::env::temp_dir().join(format!("calculix-mcp-main-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (multi, stock) = (dir.join("multi"), dir.join("stock"));
        std::fs::write(&multi, b"\x7fELF....getenv CCX_JSON ....").unwrap();
        std::fs::write(&stock, b"\x7fELF....nothing special....").unwrap();
        let specs = [format!("m={}", multi.display()), format!("s={}", stock.display())];
        let solvers = parse_solvers(&specs).unwrap();
        assert_eq!((solvers[0].json, solvers[1].json), (true, false));
        assert!(solver_line(&solvers[1]).starts_with("solver 's': stock CalculiX (no JSON telemetry;"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
