//! Background solver jobs: one supervisor task per job, one job running at a time.
//!
//! Guarantees:
//! - every solver runs as the leader of its own process group, so cancellation, timeout and
//!   server shutdown signal the whole group (SIGTERM, grace period, SIGKILL) and always reap it;
//! - each state change is written atomically to `<run_dir>/manifest.json`;
//! - at startup, jobs left non-terminal by a crashed server are killed (if the recorded process
//!   is provably the same one) and marked LOST.
//!
//! Process-group handling is Unix-only. On other platforms only the direct child is killed.

use crate::error::ToolError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{watch, Semaphore};

pub const MAX_QUEUE: usize = 8;
/// Default ceiling for the size of one run directory.
pub const DEFAULT_MAX_RUN_BYTES: u64 = 50 * 1024 * 1024 * 1024;
/// How often a running job's directory is measured.
const QUOTA_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum State {
    Queued,
    Starting,
    Running,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Lost,
}

impl State {
    pub fn terminal(self) -> bool {
        matches!(self, State::Succeeded | State::Failed | State::Cancelled | State::TimedOut | State::Lost)
    }
}

/// Where a job's telemetry comes from. `Json`: a CalculiX MultiSolver build run with `-j`, which
/// writes `<job>.json`. `StaCvg`: stock ccx, which offers only `<job>.sta` and `<job>.cvg`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TelemetrySource {
    /// Manifests written before stock ccx was supported all come from JSON-capable builds
    #[default]
    Json,
    StaCvg,
}

/// A configured solver binary and what it can do.
#[derive(Debug, Clone)]
pub struct Solver {
    pub name: String,
    pub path: PathBuf,
    /// The binary supports `-j` JSON telemetry (CalculiX MultiSolver)
    pub json: bool,
}

/// Everything known about a job; persisted as `manifest.json` in its run directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub job_id: String,
    pub state: State,
    pub label: Option<String>,
    pub jobname: String,
    pub solver: String,
    pub threads: usize,
    pub argv: Vec<String>,
    #[serde(default)]
    pub telemetry_source: TelemetrySource,
    pub run_dir: PathBuf,
    pub source_inp: PathBuf,
    pub timeout_s: u64,
    pub pid: Option<u32>,
    /// Start time of `pid` as printed by `ps`; guards against PID reuse in the reaper
    pub proc_start: Option<String>,
    pub created: u64,
    pub started: Option<u64>,
    pub ended: Option<u64>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// SPAWN | EXIT_CODE | SIGNAL | SOLVER_REPORTED | TIMEOUT | DISK_QUOTA | LOST, with detail
    pub failure: Option<String>,
}

impl Manifest {
    /// `<run_dir>/<jobname>`: the job path without extension.
    pub fn base(&self) -> PathBuf {
        self.run_dir.join(&self.jobname)
    }

    pub fn elapsed_s(&self) -> f64 {
        match self.started {
            Some(s) => self.ended.unwrap_or_else(now).saturating_sub(s) as f64,
            None => 0.0,
        }
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn write_manifest(m: &Manifest) {
    let path = m.run_dir.join("manifest.json");
    let tmp = m.run_dir.join("manifest.json.tmp");
    if let Ok(text) = serde_json::to_string_pretty(m) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn read_manifest(run_dir: &Path) -> Option<Manifest> {
    serde_json::from_str(&std::fs::read_to_string(run_dir.join("manifest.json")).ok()?).ok()
}

// ---------- OS helpers ----------

#[cfg(unix)]
fn signal_group(pid: Option<u32>, sig: i32) {
    if let Some(pid) = pid.filter(|&p| p > 1) {
        // The job is its own group leader, so the group id equals its pid.
        unsafe {
            libc::kill(-(pid as i32), sig);
        }
    }
}

#[cfg(not(unix))]
fn signal_group(_pid: Option<u32>, _sig: i32) {}

#[cfg(unix)]
const TERM: i32 = libc::SIGTERM;
#[cfg(unix)]
const KILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const TERM: i32 = 15;
#[cfg(not(unix))]
const KILL: i32 = 9;

#[cfg(unix)]
pub fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub fn alive(_pid: u32) -> bool {
    false
}

/// Remove whatever is left in the process group of a leader that has already exited.
pub fn kill_group(pid: Option<u32>) {
    signal_group(pid, KILL);
}

/// A command whose child runs isolated: cleared environment with only `PATH`, a private
/// `HOME`/`TMPDIR`/`TEMP` (`private_tmp`), the C locale, then `extra_env`, then the `passthrough`
/// variables of the server's own environment; stdin from the null device; leader of its own process
/// group on Unix; killed if the handle is dropped. The caller adds arguments, stdout and stderr.
pub fn isolated_command(
    binary: &Path,
    cwd: &Path,
    private_tmp: &Path,
    extra_env: &[(&str, &str)],
    passthrough: &[String],
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.current_dir(cwd)
        .env_clear()
        .env("PATH", if cfg!(windows) { r"C:\Windows\System32" } else { "/usr/bin:/bin" })
        .env("HOME", private_tmp)
        .env("TMPDIR", private_tmp)
        .env("TEMP", private_tmp)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (name, value) in extra_env {
        cmd.env(name, value);
    }
    for name in passthrough {
        if let Ok(value) = std::env::var(name) {
            cmd.env(name, value);
        }
    }
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// Stop the process group led by `child` (pid `pid`): SIGTERM to the group, wait up to `grace`,
/// then SIGKILL; always reap the leader (no zombie), then SIGKILL the group once more because
/// members that ignored SIGTERM may outlive the leader. Returns the leader's exit status if known.
pub async fn terminate_group(child: &mut tokio::process::Child, pid: Option<u32>, grace: Duration) -> Option<std::process::ExitStatus> {
    signal_group(pid, TERM);
    #[cfg(not(unix))]
    let _ = child.start_kill();
    let status = match tokio::time::timeout(grace, child.wait()).await {
        Ok(status) => status.ok(),
        Err(_) => {
            signal_group(pid, KILL);
            let _ = child.start_kill();
            child.wait().await.ok()
        }
    };
    signal_group(pid, KILL);
    status
}

/// Total size in bytes of the regular files under `dir` (symlinks are not followed).
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(md) = std::fs::symlink_metadata(entry.path()) else { continue };
            if md.is_dir() {
                pending.push(entry.path());
            } else if md.is_file() {
                total += md.len();
            }
        }
    }
    total
}

/// Start time of a process as an opaque string (`ps -o lstart=`); None if it does not exist.
pub fn proc_start(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps").args(["-o", "lstart=", "-p", &pid.to_string()]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Resident memory of one process in MiB (Unix, via `ps`).
pub fn rss_mb(pid: u32) -> Option<u64> {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid.to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok().map(|kb| kb / 1024)
}

// ---------- job ----------

pub struct Job {
    manifest: Mutex<Manifest>,
    state: watch::Sender<State>,
    cancel: watch::Sender<bool>,
}

impl Job {
    pub fn snapshot(&self) -> Manifest {
        self.manifest.lock().unwrap().clone()
    }

    fn update(&self, change: impl FnOnce(&mut Manifest)) {
        let state = {
            let mut m = self.manifest.lock().unwrap();
            change(&mut m);
            write_manifest(&m);
            m.state
        };
        self.state.send_replace(state);
    }

    fn finish(&self, state: State, exit_code: Option<i32>, signal: Option<i32>, failure: Option<String>) {
        self.update(|m| {
            m.state = state;
            m.ended = Some(now());
            m.exit_code = exit_code;
            m.signal = signal;
            m.failure = failure;
        });
    }
}

/// A deck already copied into its run directory, ready to start.
#[derive(Debug)]
pub struct Prepared {
    pub job_id: String,
    pub run_dir: PathBuf,
    pub source_inp: PathBuf,
    pub jobname: String,
    pub solver: String,
    pub binary: PathBuf,
    /// The solver supports `-j` JSON telemetry
    pub json: bool,
    pub threads: usize,
    pub timeout_s: u64,
    pub label: Option<String>,
}

struct Inner {
    runs_dir: PathBuf,
    solvers: Vec<Solver>,
    env_passthrough: Vec<String>,
    grace: Duration,
    max_run_bytes: u64,
    quota_interval: Duration,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    slot: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct JobManager(Arc<Inner>);

impl JobManager {
    /// `root` hosts `.calculix-mcp/runs/`. Runs the reaper before returning.
    pub fn new(root: &Path, solvers: Vec<Solver>, env_passthrough: Vec<String>, grace: Duration) -> Self {
        let manager = Self(Arc::new(Inner {
            runs_dir: root.join(".calculix-mcp").join("runs"),
            solvers,
            env_passthrough,
            grace,
            max_run_bytes: DEFAULT_MAX_RUN_BYTES,
            quota_interval: QUOTA_INTERVAL,
            jobs: Mutex::new(HashMap::new()),
            slot: Arc::new(Semaphore::new(1)),
        }));
        manager.reap();
        manager
    }

    /// Set the run-directory size limit and how often it is checked. Call right after `new`.
    pub fn with_disk_limit(mut self, max_run_bytes: u64, interval: Duration) -> Self {
        let inner = Arc::get_mut(&mut self.0).expect("with_disk_limit is called before the manager is shared");
        inner.max_run_bytes = max_run_bytes;
        inner.quota_interval = interval;
        self
    }

    pub fn solver_names(&self) -> Vec<String> {
        self.0.solvers.iter().map(|s| s.name.clone()).collect()
    }

    /// Resolve a solver name (None = first configured) to its binary.
    pub fn solver(&self, name: Option<&str>) -> Result<Solver, ToolError> {
        if self.0.solvers.is_empty() {
            return Err(ToolError::new("TOOL_DISABLED", "no solver binary is configured")
                .hint("Start the server with --ccx <name>=<absolute path to the ccx binary>."));
        }
        match name {
            None => Ok(self.0.solvers[0].clone()),
            Some(n) => self.0.solvers.iter().find(|s| s.name.eq_ignore_ascii_case(n)).cloned().ok_or_else(|| {
                ToolError::new("UNKNOWN_SOLVER", format!("no solver named '{}'", n)).valid(self.solver_names())
            }),
        }
    }

    /// Reserve a fresh job id and create its run directory (with a private `tmp/`).
    pub fn new_run_dir(&self) -> Result<(String, PathBuf), ToolError> {
        let waiting = self.0.jobs.lock().unwrap().values().filter(|j| j.snapshot().state == State::Queued).count();
        if waiting >= MAX_QUEUE {
            return Err(ToolError::new("QUEUE_FULL", format!("{} jobs are already queued", waiting))
                .hint("Wait for a job to finish (ccx_job_status with wait_s) or cancel one."));
        }
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        for salt in 0..100u32 {
            let id = format!("j{}-{:04x}", now(), (nanos ^ std::process::id()).wrapping_add(salt) & 0xffff);
            let dir = self.0.runs_dir.join(&id);
            if !dir.exists() {
                std::fs::create_dir_all(dir.join("tmp")).map_err(ToolError::internal)?;
                return Ok((id, dir));
            }
        }
        Err(ToolError::internal("could not allocate a job id"))
    }

    /// Register the job and start its supervisor. Must be called inside the tokio runtime.
    pub fn launch(&self, p: Prepared) -> Manifest {
        let manifest = Manifest {
            job_id: p.job_id.clone(),
            state: State::Queued,
            label: p.label,
            // `-j <job>` for MultiSolver (JSON telemetry; the job name is a bare argument), the
            // classic `-i <job>` for stock ccx. MultiSolver accepts `-i` too but would not write JSON.
            argv: vec![p.binary.display().to_string(), if p.json { "-j" } else { "-i" }.into(), p.jobname.clone()],
            telemetry_source: if p.json { TelemetrySource::Json } else { TelemetrySource::StaCvg },
            jobname: p.jobname,
            solver: p.solver,
            threads: p.threads,
            run_dir: p.run_dir,
            source_inp: p.source_inp,
            timeout_s: p.timeout_s,
            pid: None,
            proc_start: None,
            created: now(),
            started: None,
            ended: None,
            exit_code: None,
            signal: None,
            failure: None,
        };
        write_manifest(&manifest);
        let job = Arc::new(Job {
            manifest: Mutex::new(manifest.clone()),
            state: watch::channel(State::Queued).0,
            cancel: watch::channel(false).0,
        });
        self.0.jobs.lock().unwrap().insert(p.job_id, job.clone());
        tokio::spawn(supervise(self.0.clone(), job, p.binary));
        manifest
    }

    fn live(&self, id: &str) -> Option<Arc<Job>> {
        self.0.jobs.lock().unwrap().get(id).cloned()
    }

    /// Current manifest of a job from this server run, or of a finished one found on disk.
    pub fn get(&self, id: &str) -> Result<Manifest, ToolError> {
        if let Some(job) = self.live(id) {
            return Ok(job.snapshot());
        }
        let safe = !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        safe.then(|| read_manifest(&self.0.runs_dir.join(id))).flatten().ok_or_else(|| {
            let mut ids: Vec<String> = std::fs::read_dir(&self.0.runs_dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            ids.sort_by(|a, b| b.cmp(a)); // ids start with a timestamp: newest first
            ToolError::new("UNKNOWN_JOB", format!("no job '{}'", id)).valid(ids).hint("valid lists the most recent job ids.")
        })
    }

    /// Every job with a manifest on disk (live ones from memory), newest first, optionally only
    /// those in `state`. Returns the page of `offset..offset+limit` and the total that matched.
    pub fn list(&self, state: Option<State>, offset: usize, limit: usize) -> (Vec<Manifest>, usize) {
        let mut all: Vec<Manifest> = std::fs::read_dir(&self.0.runs_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let id = e.file_name().into_string().ok()?;
                match self.live(&id) {
                    Some(job) => Some(job.snapshot()),
                    None => read_manifest(&e.path()),
                }
            })
            .filter(|m| state.is_none_or(|s| m.state == s))
            .collect();
        all.sort_by(|a, b| (b.created, &b.job_id).cmp(&(a.created, &a.job_id)));
        let total = all.len();
        (all.into_iter().skip(offset).take(limit).collect(), total)
    }

    /// Wait until the job leaves the state it is in now, or `wait` elapses.
    pub async fn wait_change(&self, id: &str, wait: Duration) {
        if let Some(job) = self.live(id) {
            let mut rx = job.state.subscribe();
            let seen = *rx.borrow_and_update();
            if !seen.terminal() && !wait.is_zero() {
                let _ = tokio::time::timeout(wait, rx.wait_for(|s| *s != seen)).await;
            }
        }
    }

    /// Request cancellation and wait (bounded) for the job to become terminal.
    /// Returns the state before and after. Idempotent on terminal jobs.
    pub async fn cancel(&self, id: &str) -> Result<(State, State), ToolError> {
        let was = self.get(id)?.state;
        if let Some(job) = self.live(id).filter(|_| !was.terminal()) {
            job.cancel.send_replace(true);
            let mut rx = job.state.subscribe();
            let _ = tokio::time::timeout(self.0.grace + Duration::from_secs(3), rx.wait_for(|s| s.terminal())).await;
        }
        Ok((was, self.get(id)?.state))
    }

    /// Cancel every live job and wait for the supervisors to reap them.
    pub async fn shutdown(&self) {
        let jobs: Vec<Arc<Job>> = self.0.jobs.lock().unwrap().values().cloned().collect();
        for job in &jobs {
            job.cancel.send_replace(true);
        }
        for job in jobs {
            let mut rx = job.state.subscribe();
            let _ = tokio::time::timeout(self.0.grace + Duration::from_secs(3), rx.wait_for(|s| s.terminal())).await;
        }
    }

    /// Mark jobs orphaned by a previous server as LOST, killing their process group when the
    /// recorded pid is alive and has the recorded start time.
    fn reap(&self) {
        for entry in std::fs::read_dir(&self.0.runs_dir).into_iter().flatten().flatten() {
            let Some(mut m) = read_manifest(&entry.path()) else { continue };
            if m.state.terminal() {
                continue;
            }
            if let Some(pid) = m.pid {
                if alive(pid) && m.proc_start.is_some() && proc_start(pid) == m.proc_start {
                    signal_group(Some(pid), TERM);
                    std::thread::sleep(self.0.grace.min(Duration::from_secs(2)));
                    signal_group(Some(pid), KILL);
                }
            }
            m.state = State::Lost;
            m.ended = Some(now());
            m.failure = Some("LOST: the server that started this job exited before it finished".into());
            write_manifest(&m);
        }
    }
}

enum Outcome {
    Exited(std::io::Result<std::process::ExitStatus>),
    Cancelled,
    TimedOut,
    /// The run directory grew past the limit: (measured bytes, limit bytes)
    DiskQuota(u64, u64),
}

/// Resolves when `dir` is larger than `limit` bytes, checking every `interval`.
pub async fn over_quota(dir: PathBuf, limit: u64, interval: Duration) -> (u64, u64) {
    loop {
        tokio::time::sleep(interval).await;
        let d = dir.clone();
        let size = tokio::task::spawn_blocking(move || dir_size(&d)).await.unwrap_or(0);
        if size > limit {
            return (size, limit);
        }
    }
}

async fn supervise(inner: Arc<Inner>, job: Arc<Job>, binary: PathBuf) {
    let mut cancel = job.cancel.subscribe();

    // Queue: wait for the single run slot, unless cancelled first.
    let permit = tokio::select! {
        p = inner.slot.clone().acquire_owned() => p.ok(),
        _ = cancel.wait_for(|c| *c) => None,
    };
    let Some(_permit) = permit else {
        job.finish(State::Cancelled, None, None, None);
        return;
    };

    job.update(|m| m.state = State::Starting);
    let m = job.snapshot();
    let open = |name: &str| std::fs::File::create(m.run_dir.join(name));
    let (stdout, stderr) = match (open("stdout.log"), open("stderr.log")) {
        (Ok(o), Ok(e)) => (o, e),
        _ => {
            job.finish(State::Failed, None, None, Some("SPAWN: cannot create log files in the run directory".into()));
            return;
        }
    };

    let tmp = m.run_dir.join("tmp");
    let threads = m.threads.to_string();
    // MultiSolver reads the thread count from the environment only (src/nonlingeo.c,
    // mafillsmmain.c, mumps.c, ...): a CCX_NPROC_* variable wins for its stage, otherwise
    // OMP_NUM_THREADS applies, always capped at the detected CPU count. The linear solver
    // itself is fixed by the binary and the deck's `SOLVER=` parameter, not by the environment.
    let thread_env = [
        ("OMP_NUM_THREADS", threads.as_str()),
        ("CCX_NPROC_EQUATION_SOLVER", threads.as_str()),
        ("CCX_NPROC_STIFFNESS", threads.as_str()),
        ("CCX_NPROC_RESULTS", threads.as_str()),
    ];
    let mut cmd = isolated_command(&binary, &m.run_dir, &tmp, &thread_env, &inner.env_passthrough);
    cmd.args(&m.argv[1..]).stdout(stdout).stderr(stderr);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            job.finish(State::Failed, None, None, Some(format!("SPAWN: {}: {}", binary.display(), e)));
            return;
        }
    };
    let pid = child.id();
    job.update(|m| {
        m.state = State::Running;
        m.pid = pid;
        m.proc_start = pid.and_then(proc_start);
        m.started = Some(now());
    });

    let outcome = tokio::select! {
        status = child.wait() => Outcome::Exited(status),
        _ = cancel.wait_for(|c| *c) => Outcome::Cancelled,
        _ = tokio::time::sleep(Duration::from_secs(m.timeout_s)) => Outcome::TimedOut,
        (size, limit) = over_quota(m.run_dir.clone(), inner.max_run_bytes, inner.quota_interval) => Outcome::DiskQuota(size, limit),
    };

    match outcome {
        Outcome::Exited(status) => {
            // The leader is gone; remove anything it left behind in its group.
            kill_group(pid);
            let (state, code, signal, failure) = classify(status, &m);
            job.finish(state, code, signal, failure);
        }
        Outcome::Cancelled | Outcome::TimedOut | Outcome::DiskQuota(..) => {
            job.update(|m| m.state = State::Cancelling);
            let status = terminate_group(&mut child, pid, inner.grace).await;
            let (code, signal) = status.map(exit_parts).unwrap_or((None, None));
            match outcome {
                Outcome::TimedOut => job.finish(State::TimedOut, code, signal, Some(format!("TIMEOUT: exceeded {} s", m.timeout_s))),
                Outcome::DiskQuota(size, limit) => job.finish(
                    State::Failed,
                    code,
                    signal,
                    Some(format!("DISK_QUOTA: run directory reached {} MiB, limit {} MiB", size >> 20, limit >> 20)),
                ),
                _ => job.finish(State::Cancelled, code, signal, None),
            }
        }
    }
}

pub fn exit_parts(status: std::process::ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    let signal = std::os::unix::process::ExitStatusExt::signal(&status);
    #[cfg(not(unix))]
    let signal = None;
    (status.code(), signal)
}

/// Decide the terminal state of a process that exited on its own.
fn classify(status: std::io::Result<std::process::ExitStatus>, m: &Manifest) -> (State, Option<i32>, Option<i32>, Option<String>) {
    let status = match status {
        Ok(s) => s,
        Err(e) => return (State::Failed, None, None, Some(format!("SPAWN: wait failed: {}", e))),
    };
    let (code, signal) = exit_parts(status);
    if let Some(sig) = signal {
        return (State::Failed, code, signal, Some(format!("SIGNAL: killed by signal {}", sig)));
    }
    if code != Some(0) {
        return (State::Failed, code, signal, Some(format!("EXIT_CODE: solver exited with {}", code.unwrap_or(-1))));
    }
    if m.telemetry_source == TelemetrySource::StaCvg {
        // Stock ccx has no machine-readable verdict: success is exit 0 and no `*ERROR` line.
        return match first_error_line(&m.run_dir) {
            Some(line) => (State::Failed, code, signal, Some(format!("SOLVER_REPORTED: {}", line))),
            None => (State::Succeeded, code, signal, None),
        };
    }
    // Exit 0 from a JSON-capable build: it must have written <job>.json (`-j`). Without it the
    // server cannot tell success from failure, so this is suspicious.
    let json_path = m.run_dir.join(format!("{}.json", m.jobname));
    let Ok(text) = std::fs::read_to_string(&json_path) else {
        return (State::Failed, code, signal, Some(
            "SOLVER_NO_JSON: the solver exited 0 but wrote no <job>.json although it was started with -j. It was classified as a CalculiX MultiSolver build; if it is a wrapper around stock ccx, point --ccx at the stock binary directly.".into()));
    };
    let reported_failure = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .is_some_and(|v| v["meta"]["success"].as_bool() == Some(false));
    if reported_failure {
        return (State::Failed, code, signal, Some("SOLVER_REPORTED: <job>.json has meta.success = false".into()));
    }
    (State::Succeeded, code, signal, None)
}

/// First line containing `*ERROR` in stdout.log or stderr.log of a run, trimmed.
fn first_error_line(run_dir: &Path) -> Option<String> {
    use std::io::BufRead;
    for name in ["stdout.log", "stderr.log"] {
        let Ok(file) = std::fs::File::open(run_dir.join(name)) else { continue };
        let mut reader = std::io::BufReader::new(file);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let line = String::from_utf8_lossy(&buf);
                    if line.contains("*ERROR") {
                        return Some(line.trim().chars().take(300).collect());
                    }
                }
            }
        }
    }
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const FAKE_SOLVER: &str = r#"#!/bin/sh
# Fake ccx: argv is "-j <jobname>" (JSON-capable) or "-i <jobname>" (stock); behaviour is chosen by a "** MODE=<mode>" line in the deck.
job="$2"
mode=$(sed -n 's/^\*\* MODE=//p' "$job.inp" | head -1)
printf '  STEP      INC     ATT  ITRS     TOT TIME     STEP TIME      INC TIME\n     1          1     1     1  0.100000E+01  0.100000E+01  0.100000E+01\n' > "$job.sta"
echo "args=$*"
echo "fake solver mode=$mode threads=$OMP_NUM_THREADS home=$HOME"
case "$mode" in
  ok)   echo '{"meta":{"success":true},"steps":[{"step_number":1,"converged":true}]}' > "$job.json"; exit 0 ;;
  bad)  echo '{"meta":{"success":false},"steps":[{"step_number":1,"converged":false}]}' > "$job.json"; exit 0 ;;
  nojson) exit 0 ;;
  stockerr) echo " *ERROR in fake: element type not known"; echo " *ERROR in fake: second line"; exit 0 ;;
  fail) echo " *ERROR in fake: boom"; exit 201 ;;
  hang) sleep 300 ;;
  flood) while :; do head -c 65536 /dev/zero >> flood.bin; sleep 0.02; done ;;
  stubborn) (trap '' TERM; exec sleep 300) & echo $! > child.pid; trap '' TERM; wait ;;
esac
"#;

    struct Bench {
        dir: PathBuf,
        manager: JobManager,
    }

    /// The same fake script registered as a JSON-capable solver and as a stock one.
    fn solvers(path: &Path) -> Vec<Solver> {
        vec![
            Solver { name: "fake".into(), path: path.to_path_buf(), json: true },
            Solver { name: "stock".into(), path: path.to_path_buf(), json: false },
        ]
    }

    fn bench(tag: &str) -> Bench {
        let dir = std::env::temp_dir().join(format!("calculix-mcp-jobs-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let solver = dir.join("fake_ccx");
        std::fs::write(&solver, FAKE_SOLVER).unwrap();
        std::fs::set_permissions(&solver, std::fs::Permissions::from_mode(0o755)).unwrap();
        let manager = JobManager::new(&dir, solvers(&solver), Vec::new(), Duration::from_millis(400));
        Bench { dir, manager }
    }

    impl Bench {
        fn start(&self, mode: &str, timeout_s: u64) -> String {
            self.start_with(None, mode, timeout_s)
        }

        fn start_with(&self, solver: Option<&str>, mode: &str, timeout_s: u64) -> String {
            let (job_id, run_dir) = self.manager.new_run_dir().unwrap();
            std::fs::write(run_dir.join("deck.inp"), format!("** MODE={}\n*STEP\n*STATIC\n*END STEP\n", mode)).unwrap();
            let s = self.manager.solver(solver).unwrap();
            let p = Prepared {
                job_id: job_id.clone(), source_inp: run_dir.join("deck.inp"), run_dir, jobname: "deck".into(),
                solver: s.name, binary: s.path, json: s.json, threads: 3, timeout_s, label: None,
            };
            self.manager.launch(p);
            job_id
        }

        async fn until(&self, id: &str, wanted: impl Fn(State) -> bool) -> Manifest {
            for _ in 0..200 {
                let m = self.manager.get(id).unwrap();
                if wanted(m.state) {
                    return m;
                }
                self.manager.wait_change(id, Duration::from_millis(100)).await;
            }
            panic!("job {} never reached the wanted state: {:?}", id, self.manager.get(id).unwrap().state);
        }
    }

    async fn gone(pid: u32) -> bool {
        for _ in 0..40 {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn success_failure_and_solver_reported_failure() {
        let b = bench("exit");
        let ok = b.until(&b.start("ok", 30), State::terminal).await;
        assert_eq!((ok.state, ok.exit_code, ok.failure.as_deref()), (State::Succeeded, Some(0), None));
        let log = std::fs::read_to_string(ok.run_dir.join("stdout.log")).unwrap();
        assert!(log.contains("threads=3"), "thread count reaches the solver: {}", log);
        assert!(log.contains(&format!("home={}", ok.run_dir.join("tmp").display())), "environment is isolated: {}", log);
        assert!(ok.base().with_extension("sta").exists());

        let fail = b.until(&b.start("fail", 30), State::terminal).await;
        assert_eq!((fail.state, fail.exit_code), (State::Failed, Some(201)));
        assert!(fail.failure.unwrap().starts_with("EXIT_CODE"));

        let bad = b.until(&b.start("bad", 30), State::terminal).await;
        assert_eq!(bad.state, State::Failed);
        assert!(bad.failure.unwrap().starts_with("SOLVER_REPORTED"));

        let nojson = b.until(&b.start("nojson", 30), State::terminal).await;
        assert_eq!(nojson.state, State::Failed);
        assert!(nojson.failure.unwrap().starts_with("SOLVER_NO_JSON"));

        // The manifest on disk matches, and a new manager can still answer for the job.
        let again = JobManager::new(&b.dir, Vec::new(), Vec::new(), Duration::from_millis(100));
        assert_eq!(again.get(&ok.job_id).unwrap().state, State::Succeeded);
        assert_eq!(again.get("nope").unwrap_err().code, "UNKNOWN_JOB");
        assert_eq!(again.get("../x").unwrap_err().code, "UNKNOWN_JOB");
    }

    #[tokio::test]
    async fn stock_solver_is_judged_by_exit_code_and_error_lines() {
        let b = bench("stock");
        let ok = b.until(&b.start_with(Some("stock"), "nojson", 30), State::terminal).await;
        assert_eq!((ok.state, ok.exit_code, ok.failure.as_deref()), (State::Succeeded, Some(0), None));
        assert_eq!(ok.telemetry_source, TelemetrySource::StaCvg);
        assert!(!ok.base().with_extension("json").exists());

        let err = b.until(&b.start_with(Some("stock"), "stockerr", 30), State::terminal).await;
        assert_eq!((err.state, err.exit_code), (State::Failed, Some(0)));
        assert_eq!(err.failure.as_deref(), Some("SOLVER_REPORTED: *ERROR in fake: element type not known"));

        let fail = b.until(&b.start_with(Some("stock"), "fail", 30), State::terminal).await;
        assert_eq!((fail.state, fail.exit_code), (State::Failed, Some(201)));
        assert!(fail.failure.unwrap().starts_with("EXIT_CODE"));

        // The JSON-capable solver keeps its stricter rule for the very same behaviour.
        let json = b.until(&b.start_with(Some("fake"), "nojson", 30), State::terminal).await;
        assert_eq!(json.telemetry_source, TelemetrySource::Json);
        assert!(json.failure.unwrap().starts_with("SOLVER_NO_JSON"));
    }

    #[tokio::test]
    async fn argv_is_dash_i_for_stock_and_dash_j_for_json() {
        let b = bench("argv");
        let stock = b.until(&b.start_with(Some("stock"), "nojson", 30), State::terminal).await;
        let json = b.until(&b.start_with(Some("fake"), "ok", 30), State::terminal).await;
        assert_eq!(&stock.argv[1..], ["-i", "deck"]);
        assert_eq!(&json.argv[1..], ["-j", "deck"]);
        // What the process really received, as echoed by the fake solver.
        let seen = |m: &Manifest| std::fs::read_to_string(m.run_dir.join("stdout.log")).unwrap();
        assert!(seen(&stock).contains("args=-i deck\n"), "{}", seen(&stock));
        assert!(seen(&json).contains("args=-j deck\n"), "{}", seen(&json));
    }

    #[tokio::test]
    async fn cancel_kills_the_whole_group_even_if_it_ignores_sigterm() {
        let b = bench("cancel");
        let id = b.start("stubborn", 30);
        let running = b.until(&id, |s| s == State::Running).await;
        let leader = running.pid.unwrap();
        let pid_file = running.run_dir.join("child.pid");
        for _ in 0..40 {
            if pid_file.exists() { break; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let grandchild: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert!(alive(leader) && alive(grandchild));

        let (was, after) = b.manager.cancel(&id).await.unwrap();
        assert_eq!((was, after), (State::Running, State::Cancelled));
        assert!(gone(leader).await, "leader still alive");
        assert!(gone(grandchild).await, "grandchild that ignored SIGTERM still alive");
        // Idempotent on a terminal job.
        assert_eq!(b.manager.cancel(&id).await.unwrap(), (State::Cancelled, State::Cancelled));
    }

    #[tokio::test]
    async fn timeout_queue_and_shutdown() {
        let b = bench("timeout");
        let slow = b.start("hang", 1);
        let queued = b.start("hang", 30);
        b.until(&slow, |s| s == State::Running).await;
        assert_eq!(b.manager.get(&queued).unwrap().state, State::Queued, "only one job runs at a time");

        let timed_out = b.until(&slow, State::terminal).await;
        assert_eq!(timed_out.state, State::TimedOut);
        assert!(gone(timed_out.pid.unwrap()).await);

        // The queued job now gets the slot; shutdown must stop it and leave nothing behind.
        let second = b.until(&queued, |s| s == State::Running).await;
        let third = b.start("hang", 30);
        b.manager.shutdown().await;
        assert_eq!(b.manager.get(&queued).unwrap().state, State::Cancelled);
        assert_eq!(b.manager.get(&third).unwrap().state, State::Cancelled, "queued job cancelled without starting");
        assert!(b.manager.get(&third).unwrap().pid.is_none());
        assert!(gone(second.pid.unwrap()).await);
    }

    #[tokio::test]
    async fn disk_quota_kills_the_job_and_fails_it() {
        let mut b = bench("quota");
        let solver = b.dir.join("fake_ccx");
        b.manager = JobManager::new(&b.dir, solvers(&solver), Vec::new(), Duration::from_millis(400))
            .with_disk_limit(200 * 1024, Duration::from_millis(100));
        // A job that stays small is left alone.
        let small = b.until(&b.start("ok", 30), State::terminal).await;
        assert_eq!(small.state, State::Succeeded);

        let id = b.start("flood", 60);
        let m = b.until(&id, State::terminal).await;
        assert_eq!(m.state, State::Failed);
        assert!(m.failure.as_deref().unwrap().starts_with("DISK_QUOTA:"), "{:?}", m.failure);
        assert!(m.signal.is_some(), "the group was terminated");
        assert!(gone(m.pid.unwrap()).await);
        assert!(dir_size(&m.run_dir) > 200 * 1024);
    }

    #[tokio::test]
    async fn list_is_newest_first_filtered_and_paged() {
        let b = bench("list");
        let ok = b.start("ok", 30);
        b.until(&ok, State::terminal).await;
        let fail = b.start("fail", 30);
        b.until(&fail, State::terminal).await;
        // Backdate the first job, as if a previous server run had produced it.
        let mut old = b.manager.get(&ok).unwrap();
        old.created -= 100;
        write_manifest(&old);

        let fresh = JobManager::new(&b.dir, Vec::new(), Vec::new(), Duration::from_millis(100));
        let (all, total) = fresh.list(None, 0, 10);
        assert_eq!((total, all.iter().map(|m| m.job_id.as_str()).collect::<Vec<_>>()), (2, vec![fail.as_str(), ok.as_str()]));
        let (failed, n) = fresh.list(Some(State::Failed), 0, 10);
        assert_eq!((n, failed[0].job_id.as_str()), (1, fail.as_str()));
        let (page, n) = fresh.list(None, 1, 1);
        assert_eq!((n, page.len(), page[0].job_id.as_str()), (2, 1, ok.as_str()));
        assert_eq!(fresh.list(Some(State::Running), 0, 10).1, 0);
    }

    #[tokio::test]
    async fn reaper_kills_matching_orphans_and_spares_reused_pids() {
        let b = bench("reaper");
        let orphan = |name: &str, pid: u32, start: Option<String>| {
            let run_dir = b.dir.join(".calculix-mcp/runs").join(name);
            std::fs::create_dir_all(&run_dir).unwrap();
            write_manifest(&Manifest {
                job_id: name.into(), state: State::Running, label: None, jobname: "deck".into(), solver: "fake".into(),
                threads: 1, argv: Vec::new(), telemetry_source: TelemetrySource::Json, run_dir, source_inp: PathBuf::new(), timeout_s: 1, pid: Some(pid),
                proc_start: start, created: 0, started: Some(0), ended: None, exit_code: None, signal: None, failure: None,
            });
        };
        let spawn = || {
            let mut c = std::process::Command::new("sleep");
            std::os::unix::process::CommandExt::process_group(&mut c, 0);
            c.arg("300").spawn().unwrap()
        };
        let (mut real, mut reused) = (spawn(), spawn());
        orphan("j1-real", real.id(), proc_start(real.id()));
        orphan("j2-reused", reused.id(), Some("Thu Jan  1 00:00:00 1970".into()));
        orphan("j3-dead", 999_999_999, None);

        let manager = JobManager::new(&b.dir, Vec::new(), Vec::new(), Duration::from_millis(200));
        for id in ["j1-real", "j2-reused", "j3-dead"] {
            assert_eq!(manager.get(id).unwrap().state, State::Lost);
        }
        let status = real.wait().unwrap();
        assert!(std::os::unix::process::ExitStatusExt::signal(&status).is_some(), "orphan was killed");
        assert!(reused.try_wait().unwrap().is_none(), "a pid with a different start time is left alone");
        reused.kill().unwrap();
        reused.wait().unwrap();
    }
}
