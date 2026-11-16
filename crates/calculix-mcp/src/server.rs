//! MCP server: a thin adapter from tool calls to cgx-core.

use crate::error::ToolError;
use crate::paths::Roots;
use crate::{ccx_tools, frd_tools, inp_tools};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, Json, ServerHandler};
use std::sync::Arc;

const INSTRUCTIONS: &str = "Native CalculiX tools. Decks: inp_inspect, then inp_cards to read specific cards; inp_consolidate to flatten. Solver runs: ccx_diagnose explains convergence from .sta/.cvg. Results: frd_summary first, \
then frd_sets and frd_query_set for exact per-set values. Paths are relative to the server root. \
Never estimate stresses, displacements or frequencies; query them.";

#[derive(Clone)]
pub struct CalculixServer {
    roots: Arc<Roots>,
    tool_router: ToolRouter<Self>,
}

/// Run CPU-bound, file-reading work off the async runtime.
async fn blocking<T: Send + 'static>(
    roots: &Arc<Roots>,
    work: impl FnOnce(&Roots) -> Result<T, ToolError> + Send + 'static,
) -> Result<Json<T>, ToolError> {
    let roots = roots.clone();
    tokio::task::spawn_blocking(move || work(&roots))
        .await
        .map_err(ToolError::internal)?
        .map(Json)
}

#[tool_router(router = tool_router)]
impl CalculixServer {
    pub fn new(roots: Roots) -> Self {
        Self { roots: Arc::new(roots), tool_router: Self::tool_router() }
    }

    #[tool(
        name = "inp_inspect",
        description = "Summarise a CalculiX .inp deck across its *INCLUDE tree: flags, keyword census, step plan, includes (missing ones first), sets, materials. Call first on any deck.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inp_inspect(&self, Parameters(p): Parameters<inp_tools::InpInspectIn>) -> Result<Json<inp_tools::InpInspectOut>, ToolError> {
        blocking(&self.roots, move |r| inp_tools::inspect(r, p)).await
    }

    #[tool(
        name = "inp_cards",
        description = "List keyword cards of a CalculiX deck (e.g. CONTACT PAIR, BOUNDARY, STATIC) across its *INCLUDE tree, with file:line and the first data lines. Use to read specific parts of a deck without loading it.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inp_cards(&self, Parameters(p): Parameters<inp_tools::InpCardsIn>) -> Result<Json<inp_tools::InpCardsOut>, ToolError> {
        blocking(&self.roots, move |r| inp_tools::cards(r, p)).await
    }

    #[tool(
        name = "inp_consolidate",
        description = "Flatten a multi-file CalculiX deck into one .inp file on disk. Returns statistics, never deck text.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inp_consolidate(&self, Parameters(p): Parameters<inp_tools::InpConsolidateIn>) -> Result<Json<inp_tools::InpConsolidateOut>, ToolError> {
        blocking(&self.roots, move |r| inp_tools::consolidate(r, p)).await
    }

    #[tool(
        name = "ccx_diagnose",
        description = "Explain CalculiX convergence from <job>.sta/.cvg (and <job>.json if present): linear, smooth, slow, contact chattering, cutback loop or divergence, with evidence, the last increments and next steps. Works on any job directory, finished or running.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ccx_diagnose(&self, Parameters(p): Parameters<ccx_tools::CcxDiagnoseIn>) -> Result<Json<ccx_tools::CcxDiagnoseOut>, ToolError> {
        blocking(&self.roots, move |r| ccx_tools::run_diagnose(r, p)).await
    }

    #[tool(
        name = "frd_summary",
        description = "Steps, frequencies or times, available fields and whole-model peak displacement and von Mises stress (with node ids) of a CalculiX .frd result file. Call before frd_query_set.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn frd_summary(&self, Parameters(p): Parameters<frd_tools::FrdSummaryIn>) -> Result<Json<frd_tools::FrdSummaryOut>, ToolError> {
        blocking(&self.roots, move |r| frd_tools::summary(r, p)).await
    }

    #[tool(
        name = "frd_sets",
        description = "List node and element sets usable in frd_query_set, with sizes. Supports a substring filter; pass inp_path to add the deck's sets.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn frd_sets(&self, Parameters(p): Parameters<frd_tools::FrdSetsIn>) -> Result<Json<frd_tools::FrdSetsOut>, ToolError> {
        blocking(&self.roots, move |r| frd_tools::sets(r, p)).await
    }

    #[tool(
        name = "frd_query_set",
        description = "Exact stress invariants (von Mises, principal, Tresca, hydrostatic) or displacement extrema on one set, per step, with responsible node ids; top_n lists the most loaded nodes with coordinates. Never estimate these values; call this.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn frd_query_set(&self, Parameters(p): Parameters<frd_tools::FrdQueryIn>) -> Result<Json<frd_tools::FrdQueryOut>, ToolError> {
        blocking(&self.roots, move |r| frd_tools::query_set(r, p)).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CalculixServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new("calculix-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
