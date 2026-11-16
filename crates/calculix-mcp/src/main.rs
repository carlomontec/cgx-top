//! `calculix-mcp`: native MCP server for CalculiX decks, solver telemetry and results.
//!
//! stdout carries JSON-RPC only; all diagnostics go to stderr.
#![deny(clippy::print_stdout)]

mod ccx_tools;
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

    /// Solver binary as <name>=<path>, e.g. --ccx mumps=/opt/ccx/build_mumps/CalculiX. Repeatable; the first is the default
    #[arg(long = "ccx", value_name = "NAME=PATH")]
    ccx: Vec<String>,

    /// Environment variable to pass through to the solver (e.g. DYLD_LIBRARY_PATH). Repeatable
    #[arg(long = "env-passthrough", value_name = "NAME")]
    env_passthrough: Vec<String>,

    /// Largest size in GiB a job's run directory may reach before the job is killed with DISK_QUOTA
    #[arg(long = "max-run-gb", value_name = "GIB", default_value_t = 50.0)]
    max_run_gb: f64,
}

/// Parse and validate `--ccx name=path`: the binary must exist; it is stored canonicalised.
fn parse_solvers(specs: &[String]) -> anyhow::Result<Vec<(String, PathBuf)>> {
    specs
        .iter()
        .map(|spec| {
            let (name, path) = spec.split_once('=').ok_or_else(|| anyhow::anyhow!("--ccx expects NAME=PATH, got '{}'", spec))?;
            let binary = PathBuf::from(path).canonicalize().map_err(|e| anyhow::anyhow!("solver '{}': {}: {}", name, path, e))?;
            anyhow::ensure!(binary.is_file(), "solver '{}': {} is not a file", name, binary.display());
            anyhow::ensure!(!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'), "bad solver name '{}'", name);
            Ok((name.to_string(), binary))
        })
        .collect()
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
    eprintln!("solvers: {}", if solvers.is_empty() { "none (ccx_solve disabled; pass --ccx NAME=PATH)".into() } else { solvers.iter().map(|s| s.0.as_str()).collect::<Vec<_>>().join(", ") });
    anyhow::ensure!(args.max_run_gb.is_finite() && args.max_run_gb > 0.0, "--max-run-gb must be a positive number");
    let max_run_bytes = (args.max_run_gb * 1024.0 * 1024.0 * 1024.0) as u64;
    let jobs = jobs::JobManager::new(roots.first(), solvers, args.env_passthrough, std::time::Duration::from_secs(5))
        .with_disk_limit(max_run_bytes, std::time::Duration::from_secs(10));

    let service = server::CalculixServer::new(roots, jobs.clone()).serve(rmcp::transport::stdio()).await?;
    // Stop on client disconnect (stdin EOF) or a termination signal; never leave a solver behind.
    tokio::select! {
        _ = service.waiting() => {}
        _ = stop_signal() => {}
    }
    jobs.shutdown().await;
    Ok(())
}
