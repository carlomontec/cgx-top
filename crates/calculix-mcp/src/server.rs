//! MCP server: a thin adapter from tool calls to cgx-core.

use crate::error::ToolError;
use crate::folder::{self, Change, ModelsFolder};
use crate::jobs::JobManager;
use crate::paths::Roots;
use crate::{ccx_tools, cgx_tools, contact_tools, edit_tools, frd_tools, inp_tools, mesh_tools};
use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters};
use rmcp::model::{InitializeRequestParams, InitializeResult, ServerCapabilities, ServerConfig};
use rmcp::service::{NotificationContext, Peer, RequestContext};
use rmcp::{tool, tool_handler, tool_router, ErrorData, Json, RoleServer, ServerHandler};
use std::sync::Arc;
use std::time::Duration;

const INSTRUCTIONS: &str = "Native CalculiX tools. Decks: inp_inspect, then inp_cards to read specific cards; inp_consolidate to flatten, inp_edit to change a deck (new files, diff and re-check; never in place). Mesh: mesh_check before a long solve (parts, free nodes, inverted elements, volume, mass, undefined sets), mesh_select to find and save faces by geometry as a *SURFACE or *NSET include file, mesh_contact_pairs to find faces of different parts that face each other within a gap and save them as slave/master surfaces with a contact-card template. Solving: ccx_solve starts a background job, ccx_job_status (wait_s=30) follows it, ccx_job_cancel stops it, ccx_job_list lists recent jobs (also finished ones on disk), ccx_diagnose explains convergence. Geometry and meshing: cgx_help for command syntax, then cgx_run_batch runs a CalculiX GraphiX script headlessly and summarises the meshes it writes. Results: frd_summary first, \
then frd_sets and frd_query_set for exact per-set values. Paths are relative to the models folder; models_folder shows it and switches it when the user names another project folder. \
Never estimate stresses, displacements or frequencies; query them.";

#[derive(Clone)]
pub struct CalculixServer {
    folder: Arc<ModelsFolder>,
    jobs: JobManager,
    cgx: Option<Arc<cgx_tools::CgxConfig>>,
    tool_router: ToolRouter<Self>,
}

/// Run CPU-bound, file-reading work off the async runtime.
async fn blocking<T: Send + 'static>(
    roots: Arc<Roots>,
    work: impl FnOnce(&Roots) -> Result<T, ToolError> + Send + 'static,
) -> Result<Json<T>, ToolError> {
    tokio::task::spawn_blocking(move || work(&roots))
        .await
        .map_err(ToolError::internal)?
        .map(Json)
}

/// Ask the client for its roots (`file://` URIs). None if it fails or does not answer in time.
#[allow(deprecated)] // roots are deprecated by SEP-2577 but still what current clients send
async fn fetch_client_roots(peer: &Peer<RoleServer>) -> Option<Vec<String>> {
    match tokio::time::timeout(Duration::from_secs(10), peer.list_roots()).await {
        Ok(Ok(list)) => Some(list.roots.into_iter().map(|r| r.uri).collect()),
        Ok(Err(e)) => {
            eprintln!("models folder: roots/list failed: {}", e);
            None
        }
        Err(_) => {
            eprintln!("models folder: the client did not answer roots/list within 10 s");
            None
        }
    }
}

fn client_has_roots(peer: &Peer<RoleServer>) -> bool {
    peer.peer_info().is_some_and(|info| info.capabilities.roots.is_some())
}

impl CalculixServer {
    pub fn new(folder: Arc<ModelsFolder>, jobs: JobManager) -> Self {
        Self { folder, jobs, cgx: None, tool_router: Self::tool_router() }
    }

    /// The active models folder(s), read at the start of each tool call.
    async fn roots(&self) -> Result<Arc<Roots>, ToolError> {
        self.folder.current_settled().await
    }

    /// Log a change of models folder and make its run directories known to the job manager
    /// (the first activation of a folder reaps its orphaned jobs).
    async fn apply(&self, change: Option<Change>, why: &str) {
        let Some(change) = change else { return };
        eprintln!("models folder: {} [{}]", change.describe(), why);
        if let Some(roots) = change.roots {
            let (jobs, root) = (self.jobs.clone(), roots.first().to_path_buf());
            let _ = tokio::task::spawn_blocking(move || jobs.activate(&root)).await;
        }
    }

    /// Fetch the client's roots and apply them. `notified`: after roots/list_changed.
    async fn refresh_client_roots(&self, peer: &Peer<RoleServer>, notified: bool) {
        if self.folder.is_fixed() || !client_has_roots(peer) {
            return;
        }
        if let Some(uris) = fetch_client_roots(peer).await {
            let change = self.folder.set_from_client(&uris, notified);
            self.apply(change, if notified { "client roots changed" } else { "client roots" }).await;
        }
    }
}

#[tool_router(router = tool_router)]
impl CalculixServer {
    #[tool(
        name = "models_folder",
        description = "Show or change the folder with the user's models (decks, meshes, results) that all other tools work in. Call it when the user names a different project folder, or when a tool reports that no models folder is set. Paths in other tools are relative to this folder.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn models_folder(&self, Parameters(p): Parameters<folder::ModelsFolderIn>) -> Result<Json<folder::ModelsFolderOut>, ToolError> {
        // Let the client's first roots answer land before reporting or overriding it.
        let _ = self.folder.current_settled().await;
        if let Some(path) = p.path.as_deref() {
            let change = self.folder.set_from_tool(path)?;
            self.apply(change, "models_folder tool").await;
        }
        Ok(Json(self.folder.view()))
    }

    /// Enable cgx_run_batch with this CGX binary.
    pub fn with_cgx(mut self, cgx: cgx_tools::CgxConfig) -> Self {
        self.cgx = Some(Arc::new(cgx));
        self
    }

    #[tool(
        name = "cgx_run_batch",
        description = "Run a CalculiX GraphiX (.fbd) script headlessly to build geometry and meshes, in a private run directory. Returns the files produced and a summary of each mesh. Call cgx_help first for command syntax. The script is screened for shell and path-escaping commands; this is a mitigation, not a sandbox.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn cgx_run_batch(&self, Parameters(p): Parameters<cgx_tools::CgxRunIn>) -> Result<Json<cgx_tools::CgxRunOut>, ToolError> {
        cgx_tools::run_batch(self.roots().await?, self.cgx.clone(), p).await.map(Json)
    }

    #[tool(
        name = "cgx_help",
        description = "CalculiX GraphiX command reference with verified examples for geometry, meshing, sets and export.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn cgx_help(&self, Parameters(p): Parameters<cgx_tools::CgxHelpIn>) -> Result<Json<cgx_tools::CgxHelpOut>, ToolError> {
        Ok(Json(cgx_tools::help(p)))
    }

    #[tool(
        name = "ccx_solve",
        description = "Start a CalculiX solve in the background on a private copy of the deck (the original directory is never written to). Returns a job_id immediately; follow with ccx_job_status.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn ccx_solve(&self, Parameters(p): Parameters<ccx_tools::CcxSolveIn>) -> Result<Json<ccx_tools::CcxSolveOut>, ToolError> {
        let roots = self.roots().await?;
        let (r, jobs) = (roots.clone(), self.jobs.clone());
        let (prepared, copied, flags) = tokio::task::spawn_blocking(move || ccx_tools::prepare_solve(&r, &jobs, p))
            .await
            .map_err(ToolError::internal)??;
        let manifest = self.jobs.launch(prepared);
        Ok(Json(ccx_tools::solve_out(&roots, &manifest, copied, flags)))
    }

    #[tool(
        name = "ccx_job_status",
        description = "State, progress, memory, convergence verdict and result files of a solver job. Set wait_s to block until the state changes instead of polling.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ccx_job_status(&self, Parameters(p): Parameters<ccx_tools::CcxJobStatusIn>) -> Result<Json<ccx_tools::CcxJobStatusOut>, ToolError> {
        let roots = self.roots().await?;
        self.jobs.get(&p.job_id)?;
        self.jobs.wait_change(&p.job_id, std::time::Duration::from_secs(p.wait_s.unwrap_or(0).min(50))).await;
        let manifest = self.jobs.get(&p.job_id)?;
        let tail = p.tail_lines.unwrap_or(0);
        blocking(roots, move |r| Ok(ccx_tools::job_status(r, &manifest, tail))).await
    }

    #[tool(
        name = "ccx_job_cancel",
        description = "Stop a queued or running solver job and remove all of its processes. Safe to call on a finished job. Works even when no models folder is set.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ccx_job_cancel(&self, Parameters(p): Parameters<ccx_tools::CcxJobCancelIn>) -> Result<Json<ccx_tools::CcxJobCancelOut>, ToolError> {
        let (was, state) = self.jobs.cancel(&p.job_id).await?;
        Ok(Json(ccx_tools::CcxJobCancelOut { job_id: p.job_id, was, state }))
    }

    #[tool(
        name = "ccx_job_list",
        description = "List solver jobs, newest first, with state, jobname, label, elapsed seconds and run directory size. Lists the jobs of the models folder (also finished ones from earlier server runs) and every job started in this session. Filter with state.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ccx_job_list(&self, Parameters(p): Parameters<ccx_tools::CcxJobListIn>) -> Result<Json<ccx_tools::CcxJobListOut>, ToolError> {
        let (roots, jobs) = (self.roots().await?, self.jobs.clone());
        tokio::task::spawn_blocking(move || ccx_tools::job_list(&jobs, roots.first(), p)).await.map(Json).map_err(ToolError::internal)
    }

    #[tool(
        name = "inp_inspect",
        description = "Summarise a CalculiX .inp deck across its *INCLUDE tree: flags, keyword census, step plan, includes (missing ones first), sets, materials. Call first on any deck.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inp_inspect(&self, Parameters(p): Parameters<inp_tools::InpInspectIn>) -> Result<Json<inp_tools::InpInspectOut>, ToolError> {
        blocking(self.roots().await?, move |r| inp_tools::inspect(r, p)).await
    }

    #[tool(
        name = "inp_cards",
        description = "List keyword cards of a CalculiX deck (e.g. CONTACT PAIR, BOUNDARY, STATIC) across its *INCLUDE tree, with file:line and the first data lines. Use to read specific parts of a deck without loading it.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inp_cards(&self, Parameters(p): Parameters<inp_tools::InpCardsIn>) -> Result<Json<inp_tools::InpCardsOut>, ToolError> {
        blocking(self.roots().await?, move |r| inp_tools::cards(r, p)).await
    }

    #[tool(
        name = "inp_consolidate",
        description = "Flatten a multi-file CalculiX deck into one .inp file on disk. Returns statistics, never deck text.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn inp_consolidate(&self, Parameters(p): Parameters<inp_tools::InpConsolidateIn>) -> Result<Json<inp_tools::InpConsolidateOut>, ToolError> {
        blocking(self.roots().await?, move |r| inp_tools::consolidate(r, p)).await
    }

    #[tool(
        name = "inp_edit",
        description = "Edit a CalculiX deck by addressing keyword cards (set or remove a parameter, replace data lines, insert or delete a card, append a step). Writes the result to new files and returns a diff and a re-check; the original deck and its includes are never modified. Use dry_run to preview.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn inp_edit(&self, Parameters(p): Parameters<edit_tools::InpEditIn>) -> Result<Json<edit_tools::InpEditOut>, ToolError> {
        blocking(self.roots().await?, move |r| edit_tools::edit(r, p)).await
    }

    #[tool(
        name = "mesh_check",
        description = "Check a CalculiX deck's mesh before solving: disconnected parts, free nodes, inverted elements, worst aspect ratios, volume, mass and centre of gravity per material, undefined or empty sets. Call before a long solve.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn mesh_check(&self, Parameters(p): Parameters<mesh_tools::MeshCheckIn>) -> Result<Json<mesh_tools::MeshCheckOut>, ToolError> {
        blocking(self.roots().await?, move |r| mesh_tools::check(r, p)).await
    }

    #[tool(
        name = "mesh_select",
        description = "Select faces of a CalculiX mesh by geometry (direction, plane, box, cylinder, near a point, connected patch from a seed) and optionally save them as a *SURFACE or *NSET include file. Returns count, area and bounding box so the selection can be checked before use. Never edits the deck.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn mesh_select(&self, Parameters(p): Parameters<mesh_tools::MeshSelectIn>) -> Result<Json<mesh_tools::MeshSelectOut>, ToolError> {
        blocking(self.roots().await?, move |r| mesh_tools::select(r, p)).await
    }

    #[tool(
        name = "mesh_contact_pairs",
        description = "Find faces of different parts of a CalculiX mesh that face each other within a gap (contact candidates), with gaps, areas and a suggested slave/master side; optionally save both surfaces and a contact-card template. Never edits the deck.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn mesh_contact_pairs(&self, Parameters(p): Parameters<contact_tools::MeshContactIn>) -> Result<Json<contact_tools::MeshContactOut>, ToolError> {
        blocking(self.roots().await?, move |r| contact_tools::contact_pairs(r, p)).await
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
        blocking(self.roots().await?, move |r| ccx_tools::run_diagnose(r, p, job.as_ref().map(ccx_tools::JobView::of))).await
    }

    #[tool(
        name = "frd_summary",
        description = "Steps, frequencies or times, available fields and whole-model peak displacement and von Mises stress (with node ids) of a CalculiX .frd result file. Call before frd_query_set.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn frd_summary(&self, Parameters(p): Parameters<frd_tools::FrdSummaryIn>) -> Result<Json<frd_tools::FrdSummaryOut>, ToolError> {
        blocking(self.roots().await?, move |r| frd_tools::summary(r, p)).await
    }

    #[tool(
        name = "frd_sets",
        description = "List node and element sets usable in frd_query_set, with sizes. Supports a substring filter; pass inp_path to add the deck's sets.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn frd_sets(&self, Parameters(p): Parameters<frd_tools::FrdSetsIn>) -> Result<Json<frd_tools::FrdSetsOut>, ToolError> {
        blocking(self.roots().await?, move |r| frd_tools::sets(r, p)).await
    }

    #[tool(
        name = "frd_query_set",
        description = "Exact stress invariants (von Mises, principal, Tresca, hydrostatic) or displacement extrema on one set, per step, with responsible node ids; top_n lists the most loaded nodes with coordinates. Never estimate these values; call this.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn frd_query_set(&self, Parameters(p): Parameters<frd_tools::FrdQueryIn>) -> Result<Json<frd_tools::FrdQueryOut>, ToolError> {
        blocking(self.roots().await?, move |r| frd_tools::query_set(r, p)).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for CalculixServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new("calculix-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }

    async fn initialize(&self, request: InitializeRequestParams, context: RequestContext<RoleServer>) -> Result<InitializeResult, ErrorData> {
        if request.capabilities.roots.is_some() {
            // Tool calls wait (bounded) until the first roots/list answer is applied.
            self.folder.expect_client_roots();
        }
        context.peer.set_peer_info(request.clone());
        self.negotiate_initialize(&request)
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        self.refresh_client_roots(&context.peer, false).await;
        self.folder.settle();
    }

    async fn on_roots_list_changed(&self, context: NotificationContext<RoleServer>) {
        self.refresh_client_roots(&context.peer, true).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::folder::Source;
    use std::path::{Path, PathBuf};

    fn sandbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("calculix-mcp-server-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["a", "b", "home"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        dir.canonicalize().unwrap()
    }

    fn detached_jobs() -> JobManager {
        JobManager::detached(Vec::new(), Vec::new(), Duration::from_millis(100))
    }

    fn server(fixed: Vec<PathBuf>, cwd: &Path, home: &Path, jobs: JobManager) -> CalculixServer {
        let folder = ModelsFolder::new(fixed, Vec::new(), Some(cwd.to_path_buf()), Some(home.to_path_buf())).unwrap();
        CalculixServer::new(Arc::new(folder), jobs)
    }

    fn set(path: &Path) -> Parameters<folder::ModelsFolderIn> {
        Parameters(folder::ModelsFolderIn { path: Some(path.display().to_string()) })
    }

    #[tokio::test]
    async fn no_models_folder_until_one_is_chosen() {
        let dir = sandbox("none");
        // Started in the home folder: never used.
        let s = server(Vec::new(), &dir.join("home"), &dir.join("home"), detached_jobs());
        let check = || Parameters(mesh_tools::MeshCheckIn { path: "deck.inp".into() });
        let e = s.mesh_check(check()).await.err().expect("an error");
        assert_eq!((e.code, e.hint.as_deref()), ("NO_MODELS_FOLDER", Some(folder::NO_FOLDER_HINT)));
        let e = s.ccx_job_list(Parameters(ccx_tools::CcxJobListIn { limit: None, offset: None, state: None })).await.err().expect("an error");
        assert_eq!(e.code, "NO_MODELS_FOLDER");
        assert!(s.cgx_help(Parameters(cgx_tools::CgxHelpIn { topic: Some("swep".into()) })).await.is_ok(), "cgx_help needs no folder");

        let shown = s.models_folder(Parameters(folder::ModelsFolderIn { path: None })).await.unwrap().0;
        assert_eq!((shown.source, shown.folders.len()), (Source::None, 0));
        let chosen = s.models_folder(set(&dir.join("a"))).await.unwrap().0;
        assert_eq!((chosen.source, chosen.folders), (Source::Tool, vec![dir.join("a").display().to_string()]));
        assert_eq!(s.mesh_check(check()).await.err().expect("an error").code, "FILE_NOT_FOUND", "the folder is now in use");

        // Fixed by --root: switching elsewhere is refused.
        let fixed = server(vec![dir.join("a")], &dir, &dir.join("home"), detached_jobs());
        assert_eq!(fixed.models_folder(set(&dir.join("b"))).await.err().expect("an error").code, "ROOT_FIXED");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_job_started_before_a_switch_is_still_reported() {
        use serde_json::json;
        use std::os::unix::fs::PermissionsExt;
        let dir = sandbox("switch");
        let solver = dir.join("fake_ccx");
        std::fs::write(&solver, "#!/bin/sh\nsleep 1\necho '{\"meta\":{\"success\":true},\"steps\":[]}' > \"$2.json\"\n").unwrap();
        std::fs::set_permissions(&solver, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("a/deck.inp"), "*NODE\n1,0.,0.,0.\n*STEP\n*STATIC\n*END STEP\n").unwrap();
        let fake = crate::jobs::Solver { name: "fake".into(), path: solver, json: true };
        let jobs = JobManager::detached(vec![fake], Vec::new(), Duration::from_millis(200));
        let s = server(Vec::new(), &dir.join("a"), &dir.join("home"), jobs);

        let solve = ccx_tools::CcxSolveIn { inp_path: "deck.inp".into(), solver: None, threads: Some(1), timeout_s: Some(30), label: None };
        let job = s.ccx_solve(Parameters(solve)).await.unwrap().0.job_id;
        assert!(dir.join("a/.calculix-mcp/runs").join(&job).is_dir(), "run directory under the folder active at start");
        s.models_folder(set(&dir.join("b"))).await.unwrap();

        let status = |wait_s| Parameters(ccx_tools::CcxJobStatusIn { job_id: job.clone(), wait_s: Some(wait_s), tail_lines: None });
        let mut state = s.ccx_job_status(status(0)).await.unwrap().0.state;
        for _ in 0..20 {
            if state.terminal() {
                break;
            }
            state = s.ccx_job_status(status(5)).await.unwrap().0.state;
        }
        assert_eq!(state, crate::jobs::State::Succeeded);
        let list = s.ccx_job_list(Parameters(ccx_tools::CcxJobListIn { limit: None, offset: None, state: None })).await.unwrap().0;
        assert_eq!(list.jobs.rows.len(), 1, "jobs of this session are listed in the new folder");
        assert_eq!(list.jobs.rows[0][0], json!(job));
        let diag = ccx_tools::CcxDiagnoseIn { job_id: Some(job.clone()), job_path: None, step: None, last_increments: None };
        if let Err(e) = s.ccx_diagnose(Parameters(diag)).await {
            assert_ne!(e.code, "UNKNOWN_JOB");
        }
    }

    // The file:// URIs below are built for Unix paths.
    #[cfg(unix)]
    mod fake_client {
        use super::*;
        use serde_json::{json, Value};
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf};

        type ClientLines = Lines<BufReader<ReadHalf<DuplexStream>>>;
        type ClientWriter = WriteHalf<DuplexStream>;

        async fn write(w: &mut ClientWriter, v: Value) {
            w.write_all(format!("{}\n", v).as_bytes()).await.unwrap();
            w.flush().await.unwrap();
        }

        /// Read until the response to `id`, answering roots/list requests with `roots` on the way.
        async fn until(lines: &mut ClientLines, w: &mut ClientWriter, id: i64, roots: &[String]) -> Value {
            loop {
                let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await.unwrap().unwrap().unwrap();
                let msg: Value = serde_json::from_str(&line).unwrap();
                if msg["method"] == "roots/list" {
                    let list: Vec<Value> = roots.iter().map(|u| json!({ "uri": u })).collect();
                    write(w, json!({"jsonrpc": "2.0", "id": msg["id"], "result": {"roots": list}})).await;
                } else if msg["id"] == json!(id) && msg.get("method").is_none() {
                    return msg;
                }
            }
        }

        /// A fake client over an in-memory pipe speaking raw JSON-RPC: it declares the roots
        /// capability, answers roots/list with folder A, then announces a change to folder B.
        #[tokio::test]
        async fn client_roots_are_followed() {
            let dir = sandbox("roots");
            // Started in the home folder, so only the client's roots can give a folder.
            let s = server(Vec::new(), &dir.join("home"), &dir.join("home"), detached_jobs());
            let (client, server_io) = tokio::io::duplex(1 << 16);
            let running = tokio::spawn(async move {
                let service = rmcp::ServiceExt::serve(s, tokio::io::split(server_io)).await.unwrap();
                let _ = service.waiting().await;
            });
            let (cr, mut cw) = tokio::io::split(client);
            let mut lines = BufReader::new(cr).lines();
            let uri = |p: &Path| format!("file://{}", p.display());
            let (a, b) = (vec![uri(&dir.join("a"))], vec![uri(&dir.join("b"))]);
            let shown = |msg: &Value| msg["result"]["structuredContent"].clone();

            write(&mut cw, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-06-18", "capabilities": {"roots": {"listChanged": true}},
                "clientInfo": {"name": "fake", "version": "0"}}}))
            .await;
            until(&mut lines, &mut cw, 1, &a).await;
            write(&mut cw, json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;

            // The first tool call waits for the roots answer, so it already sees folder A.
            let call = json!({"name": "models_folder", "arguments": {}});
            write(&mut cw, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": call})).await;
            let first = shown(&until(&mut lines, &mut cw, 2, &a).await);
            assert_eq!((&first["source"], &first["folders"]), (&json!("client_roots"), &json!([dir.join("a").display().to_string()])), "{}", first);

            // The user opens another folder: the client notifies, the server asks again.
            write(&mut cw, json!({"jsonrpc": "2.0", "method": "notifications/roots/list_changed"})).await;
            let mut followed = Value::Null;
            for id in 3..40 {
                write(&mut cw, json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": call})).await;
                followed = shown(&until(&mut lines, &mut cw, id, &b).await);
                if followed["folders"] == json!([dir.join("b").display().to_string()]) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert_eq!((&followed["source"], &followed["folders"]), (&json!("client_roots"), &json!([dir.join("b").display().to_string()])), "{}", followed);
            drop(cw);
            let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
        }
    }
}
