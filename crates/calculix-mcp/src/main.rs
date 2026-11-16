//! `calculix-mcp`: native MCP server for CalculiX decks, solver telemetry and results.
//!
//! stdout carries JSON-RPC only; all diagnostics go to stderr.
#![deny(clippy::print_stdout)]

mod ccx_tools;
mod error;
mod frd_tools;
mod inp_tools;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let roots = if args.roots.is_empty() { vec![std::env::current_dir()?] } else { args.roots };
    let roots = paths::Roots::new(roots)?;
    eprintln!("calculix-mcp {}: serving on stdio, root {}", env!("CARGO_PKG_VERSION"), roots.first().display());

    let service = server::CalculixServer::new(roots).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
