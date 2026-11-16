//! MCP server: a thin adapter from tool calls to cgx-core.

use crate::error::ToolError;
use crate::jobs::JobManager;
use crate::paths::Roots;
use crate::{ccx_tools, frd_tools, inp_tools};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, Json, ServerHandler};
use std::sync::Arc;

const INSTRUCTIONS: &str = "Native CalculiX tools. Decks: inp_inspect, then inp_cards to read specific cards; inp_consolidate to flatten. Solving: ccx_solve starts a background job, ccx_job_status (wait_s=30) follows it, ccx_job_cancel stops it, ccx_diagnose explains convergence. Results: frd_summary first, \
then frd_sets and frd_query_set for exact per-set values. Paths are relative to the server root. \
Never estimate stresses, displacements or frequencies; query them.";

#[derive(Clone)]
pub struct CalculixServer {
    roots: Arc<Roots>,
    jobs: JobManager,
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
    pub fn new(roots: Roots, jobs: JobManager) -> Self {
        Self { roots: Arc::new(roots), jobs, tool_router: Self::tool_router() }
    }

    #[tool(
        name = "ccx_solve",
        description = "Start a CalculiX solve in the background on a private copy of the deck (the original directory is never written to). Returns a job_id immediately; follow with ccx_job_status.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn ccx_solve(&self, Parameters(p): Parameters<ccx_tools::CcxSolveIn>) -> Result<Json<ccx_tools::CcxSolveOut>, ToolError> {
        let (roots, jobs) = (self.roots.clone(), self.jobs.clone());
        let (prepared, copied, flags) = tokio::task::spawn_blocking(move || ccx_tools::prepare_solve(&roots, &jobs, p))
            .await
            .map_err(ToolError::internal)??;
        let manifest = self.jobs.launch(prepared);
        Ok(Json(ccx_tools::solve_out(&self.roots, &manifest, copied, flags)))
    }

    #[tool(
        name = "ccx_job_status",
        description = "State, progress, memory, convergence verdict and result files of a solver job. Set wait_s to block until the state changes instead of polling.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ccx_job_status(&self, Parameters(p): Parameters<ccx_tools::CcxJobStatusIn>) -> Result<Json<ccx_tools::CcxJobStatusOut>, ToolError> {
        self.jobs.get(&p.job_id)?;
        self.jobs.wait_change(&p.job_id, std::time::Duration::from_secs(p.wait_s.unwrap_or(0).min(50))).await;
        let manifest = self.jobs.get(&p.job_id)?;
        let tail = p.tail_lines.unwrap_or(0);
        blocking(&self.roots, move |r| Ok(ccx_tools::job_status(r, &manifest, tail))).await
    }

    #[tool(
        name = "ccx_job_cancel",
        description = "Stop a queued or running solver job and remove all of its processes. Safe to call on a finished job.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ccx_job_cancel(&self, Parameters(p): Parameters<ccx_tools::CcxJobCancelIn>) -> Result<Json<ccx_tools::CcxJobCancelOut>, ToolError> {
        let (was, state) = self.jobs.cancel(&p.job_id).await?;
        Ok(Json(ccx_tools::CcxJobCancelOut { job_id: p.job_id, was, state }))
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
        description = "Explain CalculiX convergence from <job>.sta/.cvg (and <job>.json if present): linear, smooth, slow, contact chattering, cutback loop or divergence, with evidence, the last increments and next steps. Give job_id for a job started by ccx_solve, or job_path for any job directory.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ccx_diagnose(&self, Parameters(p): Parameters<ccx_tools::CcxDiagnoseIn>) -> Result<Json<ccx_tools::CcxDiagnoseOut>, ToolError> {
        let job = match &p.job_id {
            Some(id) => Some(self.jobs.get(id)?),
            None => None,
        };
        blocking(&self.roots, move |r| ccx_tools::run_diagnose(r, p, job.as_ref().map(ccx_tools::JobView::of))).await
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
