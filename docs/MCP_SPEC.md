# calculix-mcp — Architecture Spec (v1)

Status: spec only. Source of facts: `docs/MCP_FACTS.md`. Anything tagged **ASSUMED** is not in the facts file and must be verified by the implementer before coding the dependent module (see §8.3). Field names, defaults, enums and thresholds in this document are normative.

---

## 1. Topology

### 1.1 Crates

| Crate | Change | Contents |
|---|---|---|
| `crates/cgx-core` | extend, stays sync, no serde, no tokio | `parser/inp_cards.rs` (card scanner), `telemetry/{sta.rs,cvg.rs,diagnose.rs}` |
| `crates/calculix-mcp` | new binary `calculix-mcp` | everything below |
| `crates/cgx-top` | untouched | — |

`calculix-mcp/src/`:

| Module | Responsibility |
|---|---|
| `main.rs` | arg parsing (`--config`, `--root` repeatable, `--log-file`), tracing init, runtime, serve stdio |
| `config.rs` | TOML + env (`CALCULIX_MCP_*`) → `Config` (§6.4) |
| `paths.rs` | root confinement, canonicalisation, extension allowlist |
| `error.rs` | `ToolError` (§2.2) and conversion from `FrdError`, io |
| `dto.rs` | all serde/schemars request and response structs (cgx-core types are never serialised directly) |
| `page.rs` | cursor, caps, response-size guard (§5) |
| `inp_tools.rs` | `inp_inspect`, `inp_cards`, `inp_consolidate` |
| `frd_cache.rs` | sidecar summary + in-memory model LRU + background load |
| `frd_tools.rs` | `frd_summary`, `frd_sets`, `frd_query_set` |
| `jobs/{mod,manifest,spawn,supervisor,reaper}.rs` | job table, state machine, process groups, telemetry tailing |
| `ccx_tools.rs` | `ccx_solve`, `ccx_job_status`, `ccx_job_cancel`, `ccx_job_list`, `ccx_diagnose` |
| `cgx_tools.rs` | gated `cgx_run_batch`, `cgx_snapshot` |
| `resources.rs`, `prompts.rs` | §2.5, §2.6 |
| `server.rs` | rmcp `ServerHandler`, tool router, capability declaration |

Dependencies (all versions **ASSUMED**; pin exact versions after checking crates.io): `rmcp` (features `server`, `transport-io`, `macros`), `tokio` (rt-multi-thread, process, fs, time, signal, sync), `serde`, `serde_json`, `schemars`, `toml`, `tracing`, `tracing-subscriber`, `sysinfo`, `process-wrap` (fallback: hand-rolled `setsid`/`killpg` on Unix, Job Object on Windows).

### 1.2 Protocol

- Revision: **`2025-11-25`** (**ASSUMED** latest stable; use the newest revision the pinned `rmcp` supports). The design only needs features present since `2025-06-18`: `structuredContent` + `outputSchema`, resource links, tool annotations. Negotiate down to `2025-06-18`; refuse older.
- MCP Tasks are experimental in that revision and are **not** used (decision D1).
- Transport: stdio only.
- Capabilities: `tools`, `resources {subscribe, listChanged}`, `prompts`, `logging`.

### 1.3 Lifecycle

1. Load config, validate binaries (exists, regular file, executable). A missing binary disables its tools; it is not fatal.
2. Run the reaper (§3.4). 3. Serve `initialize`. 4. If no `--root` was given and the client declares `roots`, call `roots/list` once; else use cwd.
5. Shutdown on stdin EOF, SIGINT or SIGTERM: stop accepting requests, cancel every non-terminal job (§3.2), flush manifests, exit within `kill_grace_s + 5` s.

### 1.4 Logging

- stdout carries JSON-RPC only. Crate root: `#![deny(clippy::print_stdout, clippy::print_stderr)]`.
- `tracing` → stderr (default `info`, `CALCULIX_MCP_LOG`), optional `--log-file`.
- `warn` and above are mirrored as `notifications/message`.
- Child stdout/stderr go to files in the run dir, never to inherited handles.

---

## 2. Tool surface

### 2.1 Critique of the baseline

| Baseline | Problem | Resolution |
|---|---|---|
| `inp_inspect` | only counts exist (facts, discrepancy 4); output unbounded on large decks | add one card scanner in cgx-core; `sections` selector; `inp_cards` for drill-down |
| `ccx_solve` (async) | one tool cannot start, poll and cancel | `ccx_solve` + `ccx_job_status` (long-poll) + `ccx_job_cancel` + `ccx_job_list` |
| `ccx_convergence_status` | mixes process state with numerical diagnosis; unusable on jobs run outside the server | renamed `ccx_diagnose`; accepts `job_id` or a path |
| `frd_summary` | first parse of a 408 MB file is slow and RAM-heavy | sidecar summary cache; `FRD_LOADING` retry contract |
| `frd_query_set` | LLM must guess set names | `frd_sets` with filter; errors list nearest names; `top_n` answers "which nodes" |
| snapshot | headless hardcopy UNVERIFIED | gated, off by default |

### 2.2 Error model

Every failure, including invalid arguments, is a tool result with `isError: true` and this `structuredContent` (mirrored as compact JSON in one text block). JSON-RPC errors are used only for unknown tool names and malformed envelopes.

```json
{"type":"object","required":["code","message","retryable"],"additionalProperties":false,
 "properties":{
  "code":{"enum":["INVALID_ARGUMENT","PATH_OUTSIDE_ROOTS","FILE_NOT_FOUND","BAD_EXTENSION","UNKNOWN_SET","UNKNOWN_STEP","UNKNOWN_DATASET","UNKNOWN_SOLVER","UNKNOWN_JOB","JOB_NOT_ACTIVE","QUEUE_FULL","RESULTS_EXIST","DECK_INVALID","FRD_LOADING","FRD_BUSY","FRD_TOO_LARGE","CURSOR_STALE","TOOL_DISABLED","LIMIT_EXCEEDED","INTERNAL"]},
  "message":{"type":"string","maxLength":300},
  "hint":{"type":"string","maxLength":300,"description":"The next call to make, written imperatively."},
  "valid":{"type":"array","items":{"type":"string"},"maxItems":10,"description":"Valid alternatives for the offending argument."},
  "valid_total":{"type":"integer"},
  "retryable":{"type":"boolean"},
  "retry_after_s":{"type":"integer"}}}
```

| Code | `valid` holds | `hint` |
|---|---|---|
| `FILE_NOT_FOUND` | up to 10 files with an accepted extension in the nearest existing parent | — |
| `UNKNOWN_SET` | 10 nearest names: case-insensitive exact, then substring, then Levenshtein ≤ 3, then alphabetical | "Call frd_sets with filter=…" |
| `UNKNOWN_STEP` | `["0..=<max idx>"]` | "Call frd_summary" |
| `UNKNOWN_DATASET` | dataset names in the file | — |
| `UNKNOWN_SOLVER` | configured solver names | — |
| `UNKNOWN_JOB` | 10 most recent job ids | "Call ccx_job_list" |
| `DECK_INVALID` | missing include targets as `file:line → target` | "Fix or call inp_inspect" |
| `RESULTS_EXIST` | existing result files | "Pass overwrite_results=true or use run_mode=isolated" |
| `FRD_LOADING` | — | "Parse continues in background; repeat the same call"; `retry_after_s` = 15 |
| `PATH_OUTSIDE_ROOTS` | the allowed roots | — |

### 2.3 Tools

Annotations: R = `readOnlyHint`, D = `destructiveHint`, I = `idempotentHint`. `openWorldHint` is false for all.

| Tool | Ann. | Description (verbatim, shown to the model) |
|---|---|---|
| `inp_inspect` | R I | Summarise a CalculiX .inp deck across its *INCLUDE tree: sizes, sets, materials, steps, keyword census. Call first on any deck. |
| `inp_cards` | R I | List keyword cards (e.g. *CONTACT PAIR, *BOUNDARY) with file:line and a few data lines. Use to read specific parts of a deck without loading it. |
| `inp_consolidate` | I | Flatten a multi-file deck into one .inp file. Returns statistics, never deck text. |
| `ccx_solve` | — | Start a CalculiX solve in the background. Returns a job_id immediately; follow with ccx_job_status. |
| `ccx_job_status` | R | State, progress, memory and verdict of a job. Set wait_s to block until the state changes instead of polling. |
| `ccx_job_cancel` | D I | Stop a queued or running job and reap its processes. |
| `ccx_job_list` | R I | List jobs known to this server, newest first. |
| `ccx_diagnose` | R I | Explain convergence from .sta/.cvg: smooth, slow, contact chattering, cutback loop or divergence, with evidence and next steps. Works on any job directory. |
| `frd_summary` | R I | Steps, frequencies or times, available fields and global extrema of a .frd file. Call before frd_query_set. |
| `frd_sets` | R I | List node and element sets usable in frd_query_set, with sizes. Supports a substring filter. |
| `frd_query_set` | R I | Exact stress invariants or displacement extrema on one set, per step, with responsible node ids. Never estimate these values; call this. |
| `cgx_run_batch` | D | (gated) Run a .fbd/.fbl script headlessly with `cgx -bg`. |
| `cgx_snapshot` | — | (gated) Render a contour PNG of one field at one step. |

### 2.4 Schemas

Shared definitions (inlined per tool at build time):

```json
{"$defs":{
 "StepSel":{"type":"object","additionalProperties":false,"properties":{
   "select":{"enum":["all","last","first_last","equidistant","list"],"default":"all"},
   "n":{"type":"integer","minimum":2,"maximum":200,"default":10,"description":"Used by equidistant."},
   "idx":{"type":"array","items":{"type":"integer","minimum":0},"maxItems":200,"description":"Used by list. Zero-based positions in the file's step list."}}},
 "Page":{"type":"object","required":["returned","total","truncated","next_cursor"],"additionalProperties":false,"properties":{
   "returned":{"type":"integer"},"total":{"type":"integer"},"truncated":{"type":"boolean"},
   "next_cursor":{"type":["string","null"]}}},
 "Cursor":{"type":"string","pattern":"^[0-9a-f]{16}:[0-9]+$"}}}
```

**`inp_inspect`**

```json
{"input":{"type":"object","required":["path"],"additionalProperties":false,"properties":{
  "path":{"type":"string"},
  "sections":{"type":"array","items":{"enum":["includes","sets","materials","steps","census","external_refs"]},"default":["includes","materials","steps","census"]},
  "limit":{"type":"integer","minimum":1,"maximum":200,"default":50,"description":"Cap per list section."}}},
 "output":{"type":"object","required":["root_file","totals","page"],"properties":{
  "root_file":{"type":"string"},
  "totals":{"type":"object","properties":{"lines":{"type":"integer"},"files":{"type":"integer"},"steps":{"type":"integer"},"boundary_cards":{"type":"integer"},"node_sets":{"type":"integer"},"element_sets":{"type":"integer"},"missing_includes":{"type":"integer"},"has_nodes":{"type":"boolean"},"has_elements":{"type":"boolean"}}},
  "includes":{"type":"array","items":{"type":"object","properties":{"target":{"type":"string"},"from":{"type":"string"},"line":{"type":"integer"},"exists":{"type":"boolean"},"bytes":{"type":"integer"}}}},
  "sets":{"type":"object","properties":{"columns":{"const":["name","kind","count"]},"rows":{"type":"array"}}},
  "materials":{"type":"array","items":{"type":"string"}},
  "steps":{"type":"array","items":{"type":"object","properties":{"n":{"type":"integer"},"line":{"type":"string","description":"file:line of *STEP"},"params":{"type":"string","description":"Raw *STEP parameters, e.g. NLGEOM,PERTURBATION"},"procedure":{"type":"string","description":"First procedure card, e.g. *STATIC, *FREQUENCY"},"cards":{"type":"object","additionalProperties":{"type":"integer"},"description":"Keyword census inside the step."}}}},
  "census":{"type":"object","additionalProperties":{"type":"integer"},"description":"Model-level keyword → count, outside steps."},
  "external_refs":{"type":"array","items":{"type":"object","properties":{"card":{"type":"string"},"param":{"type":"string"},"target":{"type":"string"},"line":{"type":"string"},"exists":{"type":"boolean"}}}},
  "flags":{"type":"array","items":{"enum":["CYCLIC_SYMMETRY","CONTACT","NLGEOM","PERTURBATION","MISSING_INCLUDES","NO_STEPS","NO_MESH"]}},
  "page":{"$ref":"#/$defs/Page"}}}}
```

Sorting: sets by descending count then name; includes in file order. `page` describes the longest truncated section.

**`inp_cards`**

```json
{"input":{"type":"object","required":["path","keyword"],"additionalProperties":false,"properties":{
  "path":{"type":"string"},
  "keyword":{"type":"string","description":"Case-insensitive, leading * optional, spaces significant: 'CONTACT PAIR'."},
  "step":{"type":"integer","minimum":0,"description":"0 = model level only; n = inside step n; omit = anywhere."},
  "data_lines":{"type":"integer","minimum":0,"maximum":50,"default":5},
  "limit":{"type":"integer","minimum":1,"maximum":50,"default":10},
  "cursor":{"$ref":"#/$defs/Cursor"}}},
 "output":{"type":"object","required":["cards","page"],"properties":{
  "cards":{"type":"array","items":{"type":"object","properties":{"at":{"type":"string","description":"file:line"},"step":{"type":"integer"},"header":{"type":"string"},"data":{"type":"array","items":{"type":"string"}},"data_total":{"type":"integer"}}}},
  "page":{"$ref":"#/$defs/Page"}}}}
```

Unknown keyword returns an empty list plus `hint` naming the 10 most frequent keywords; it is not an error.

**`inp_consolidate`**

```json
{"input":{"type":"object","required":["path"],"additionalProperties":false,"properties":{
  "path":{"type":"string"},
  "output_path":{"type":"string","description":"Default: <stem>_consolidated.inp beside the root deck."},
  "overwrite":{"type":"boolean","default":false}}},
 "output":{"type":"object","required":["output_file","files_inlined","lines_written","bytes"],"properties":{
  "output_file":{"type":"string"},"files_inlined":{"type":"integer"},"lines_written":{"type":"integer"},"bytes":{"type":"integer"},
  "inlined":{"type":"array","items":{"type":"string"},"maxItems":50},"page":{"$ref":"#/$defs/Page"}}}}
```

Result also carries a `resource_link` content block to the written file.

**`ccx_solve`**

```json
{"input":{"type":"object","required":["inp_path"],"additionalProperties":false,"properties":{
  "inp_path":{"type":"string"},
  "solver":{"type":"string","description":"One of the configured solver names; default from config."},
  "threads":{"type":"integer","minimum":1,"description":"Default from config; capped at logical CPU count."},
  "timeout_s":{"type":"integer","minimum":10,"default":3600},
  "run_mode":{"enum":["isolated","in_place"],"default":"isolated"},
  "overwrite_results":{"type":"boolean","default":false,"description":"in_place only."},
  "label":{"type":"string","maxLength":60}}},
 "output":{"type":"object","required":["job_id","state","run_dir","jobname"],"properties":{
  "job_id":{"type":"string","pattern":"^j[0-9]{14}-[0-9a-f]{4}$"},
  "state":{"enum":["QUEUED","STARTING"]},"queue_position":{"type":"integer"},
  "run_dir":{"type":"string"},"jobname":{"type":"string"},
  "preflight":{"type":"object","properties":{"steps":{"type":"integer"},"flags":{"type":"array","items":{"type":"string"}},"external_refs_linked":{"type":"integer"}}},
  "next":{"const":"Call ccx_job_status with this job_id and wait_s=30."}}}}
```

**`ccx_job_status`**

```json
{"input":{"type":"object","required":["job_id"],"additionalProperties":false,"properties":{
  "job_id":{"type":"string"},
  "wait_s":{"type":"integer","minimum":0,"maximum":50,"default":0,"description":"Return early when the state changes."},
  "tail_lines":{"type":"integer","minimum":0,"maximum":100,"default":0}}},
 "output":{"type":"object","required":["job_id","state","elapsed_s"],"properties":{
  "job_id":{"type":"string"},"label":{"type":"string"},"jobname":{"type":"string"},
  "state":{"enum":["QUEUED","STARTING","RUNNING","CANCELLING","SUCCEEDED","FAILED","CANCELLED","TIMED_OUT","LOST"]},
  "queue_position":{"type":"integer"},"solver":{"type":"string"},"threads":{"type":"integer"},
  "elapsed_s":{"type":"number"},"exit_code":{"type":["integer","null"]},"signal":{"type":["integer","null"]},
  "failure":{"type":"object","properties":{"kind":{"enum":["SPAWN","EXIT_CODE","SIGNAL","SOLVER_REPORTED","STALL","DISK_QUOTA"]},"detail":{"type":"string","maxLength":300}}},
  "resources":{"type":"object","properties":{"rss_mb":{"type":"integer"},"peak_rss_mb":{"type":"integer"},"cpu_pct":{"type":"integer"},"run_dir_mb":{"type":"integer"}}},
  "progress":{"type":"object","properties":{"step":{"type":"integer"},"inc":{"type":"integer"},"att":{"type":"integer"},"iter":{"type":"integer"},"tot_time":{"type":"number"},"inc_time":{"type":"number"},"increments_done":{"type":"integer"},"cutbacks":{"type":"integer"},"resid_force_pct":{"type":["number","null"]},"steps_done":{"type":"integer"}}},
  "verdict":{"type":"string","description":"Same enum as ccx_diagnose.verdict."},
  "telemetry":{"type":"object","description":"<job>.json 'meta' and 'steps' verbatim once present; mass-participation objects dropped."},
  "files":{"type":"object","additionalProperties":{"type":"string"},"description":"Existing files only: frd, dat, sta, cvg, json, stdout, stderr."},
  "tail":{"type":"string"},
  "next":{"type":"string"}}}}
```

`next` is fixed per state: RUNNING → "Call ccx_job_status with wait_s=30."; SUCCEEDED → "Call frd_summary on files.frd."; FAILED/TIMED_OUT → "Call ccx_diagnose with this job_id."

**`ccx_job_cancel`**: input `{job_id}`; output `{job_id, state, was}`. Terminal jobs return their state unchanged (idempotent), not an error.

**`ccx_job_list`**: input `{limit: 1..50 = 10, state?: enum}`; output `{columns:["job_id","state","jobname","label","elapsed_s","run_dir_mb"], rows, page}`.

**`ccx_diagnose`**

```json
{"input":{"type":"object","additionalProperties":false,"oneOf":[{"required":["job_id"]},{"required":["job_path"]}],"properties":{
  "job_id":{"type":"string"},
  "job_path":{"type":"string","description":"Path to <job> with or without .inp/.sta/.cvg."},
  "step":{"type":"integer","minimum":1,"description":"Default: last step present."},
  "last_increments":{"type":"integer","minimum":1,"maximum":50,"default":10},
  "trace":{"type":"boolean","default":false,"description":"Include the residual trace of the last attempt (≤ 20 points)."}}},
 "output":{"type":"object","required":["verdict","findings","coverage"],"properties":{
  "verdict":{"enum":["NO_TELEMETRY","LINEAR","SMOOTH","SLOW","CONTACT_CHATTER","CUTBACK","CUTBACK_LOOP","DIVERGED"]},
  "running":{"type":"boolean"},
  "coverage":{"type":"object","properties":{"sta_rows":{"type":"integer"},"cvg_rows":{"type":"integer"},"steps":{"type":"array","items":{"type":"integer"}},"json":{"type":"boolean"}}},
  "findings":{"type":"array","maxItems":10,"items":{"type":"object","required":["rule","severity","where","evidence"],"properties":{
     "rule":{"type":"string","description":"Rule id from the diagnostics table, e.g. C2."},
     "severity":{"enum":["info","warn","error"]},
     "where":{"type":"object","properties":{"step":{"type":"integer"},"inc":{"type":"integer"},"att":{"type":"integer"}}},
     "evidence":{"type":"object","additionalProperties":{"type":["number","string","null"]}},
     "occurrences":{"type":"integer"},
     "advice":{"type":"string","maxLength":240}}}},
  "increments":{"type":"object","properties":{"columns":{"const":["step","inc","att","itrs","tot_time","inc_time","contact_el","resid_force_pct"]},"rows":{"type":"array"}}},
  "trace":{"type":"object","properties":{"columns":{"const":["iter","contact_el","resid_force_pct","corr_disp_pct"]},"rows":{"type":"array","maxItems":20}}},
  "solver_messages":{"type":"array","maxItems":5,"items":{"type":"string","maxLength":200}}}}}
```

`frd_summary` and `frd_query_set`: see §5.3. **`frd_sets`**: input `{path, inp_path?, filter?: string, kind?: "node"|"element"|"mixed", limit: 1..200 = 50, cursor?}`; output `{columns:["name","kind","nodes","elements"], rows, page}`, sorted by name.

**Gated tools** (registered only when enabled and the startup probe passes; otherwise calls return `TOOL_DISABLED`):

- `cgx_run_batch`: input `{script_path (.fbd|.fbl), timeout_s: 10..3600 = 300}`; output `{exit_code, elapsed_s, new_files:[path], tail, page}`. Runs through the same job machinery, synchronously, cwd = script directory. `new_files` is a directory diff before/after, capped at 50.
- `cgx_snapshot`: input `{frd_path, dataset, component, step_idx, width: 256..2048 = 1024}`; output `{png_path, bytes}` plus a `resource_link`. The server writes a generated `.fbd` into a scratch dir; the cgx command sequence is **ASSUMED** and blocked on Q3.

### 2.5 Resources

Everything is also reachable through tools; resources exist for clients that surface them.

| URI | MIME | Notes |
|---|---|---|
| `calculix://jobs` | application/json | same payload as `ccx_job_list` (limit 50) |
| `calculix://jobs/{job_id}` | application/json | `ccx_job_status` payload; subscribable; `notifications/resources/updated` on each state transition |
| `calculix://jobs/{job_id}/{sta\|cvg\|stdout\|stderr\|telemetry}` | text/plain, application/json | last 64 KiB with the truncation marker (§5.2) |
| `calculix://guide/diagnostics` | text/markdown | the rule table of §4, generated from `Thresholds::default()` |

`notifications/resources/list_changed` fires on job creation.

### 2.6 Prompts

| Name | Arguments | Script the prompt instructs |
|---|---|---|
| `review_deck` | `inp_path` | `inp_inspect` → `inp_cards` on flagged keywords → report missing includes, unconstrained risk, step plan |
| `solve_and_report` | `inp_path`, `set?` | `ccx_solve` → `ccx_job_status(wait_s=30)` loop → `frd_summary` → `frd_query_set` |
| `debug_nonconvergence` | `job` (id or path) | `ccx_diagnose(trace=true)` → `inp_cards` on *CONTACT PAIR, *SURFACE BEHAVIOR, *STATIC, *CONTROLS → ranked deck changes |

### 2.7 Progress and cancellation

- `notifications/progress` is sent only when the request carries a `progressToken`, at most 1 Hz: `ccx_job_status` with `wait_s > 0` (`progress` = increments done, no `total`, `message` = `"step 2 inc 14 att 1 resid 0.31%"`), and frd tools while parsing (`progress` = seconds elapsed).
- `notifications/cancelled` aborts only the pending request (the wait or the parse wait). It never cancels a job; only `ccx_job_cancel` does.

---

## 3. Async solver

### 3.1 State machine

| From | Event | To |
|---|---|---|
| — | `ccx_solve` preflight passes | QUEUED |
| QUEUED | slot free (`max_concurrent_jobs`, default 1) | STARTING |
| STARTING | spawn ok | RUNNING |
| STARTING | spawn error | FAILED (`SPAWN`) |
| RUNNING | exit 0 and (`<job>.json` absent or `meta.success == true`) | SUCCEEDED |
| RUNNING | exit ≠ 0, killed by foreign signal, or `meta.success == false` | FAILED (`EXIT_CODE` / `SIGNAL` / `SOLVER_REPORTED`) |
| RUNNING | wall time > `timeout_s` | CANCELLING → TIMED_OUT |
| RUNNING | stall rule or disk quota (§3.3) | CANCELLING → FAILED (`STALL` / `DISK_QUOTA`) |
| QUEUED | `ccx_job_cancel` | CANCELLED |
| STARTING, RUNNING | `ccx_job_cancel` or server shutdown | CANCELLING → CANCELLED |
| any non-terminal | found by reaper after a server crash | LOST |

Terminal states never change. Every transition rewrites `manifest.json` atomically (write temp, fsync, rename).

Preflight (synchronous, in order; first failure is returned): path confinement → `inspect_inp` succeeds → no missing includes and none outside the roots (`DECK_INVALID`) → `step_count ≥ 1` → solver configured (`UNKNOWN_SOLVER`) → queue length < `max_queue` (`QUEUE_FULL`) → `in_place` only: no existing `<job>.{frd,dat,sta,cvg,json}` unless `overwrite_results` (`RESULTS_EXIST`).

### 3.2 Spawn and cancellation

- Run dir, `isolated`: `<first root>/.calculix-mcp/runs/<job_id>/`. The deck is written there as `<jobname>.inp` via `consolidate_inp`. Each `external_refs` entry (non-`*INCLUDE` file parameters such as `INPUT=`) is symlinked (copied on Windows) under its relative path; a ref outside the roots is `DECK_INVALID`. `in_place`: cwd is the deck directory; the manifest, stdout and stderr still live in the run dir.
- `jobname` = deck stem if it matches `^[A-Za-z0-9_][A-Za-z0-9_.-]{0,63}$`, else `job` (isolated) or `INVALID_ARGUMENT` (in_place).
- argv: `[<solver binary>, "-j", <jobname>]`. stdin null. stdout and stderr are files `stdout.log`, `stderr.log`.
- Each job is the leader of a new process group (Unix `setsid`) or a Job Object with `KILL_ON_JOB_CLOSE` (Windows).
- Cancel sequence: SIGTERM to the group → wait `kill_grace_s` (5) → SIGKILL to the group → `wait()` on the child (always, so no zombie) → verify the group is empty with `killpg(pgid, 0)`; if not, SIGKILL again and log `warn`.
- The supervisor task owns the `Child`; no other code calls `wait` or `kill`. Tools send messages over an mpsc channel.

### 3.3 Supervision loop (every 1 s while RUNNING)

1. `try_wait`. 2. Tail `.sta`/`.cvg` from the stored byte offsets (complete lines only). 3. Sample RSS and CPU time summed over the process group via `sysinfo`. 4. Check limits.

| Limit | Default | Action |
|---|---|---|
| wall `timeout_s` | 3600, max 86400 | TIMED_OUT |
| stall: no growth of `.sta`, `.cvg`, `stdout.log`, `.frd` **and** CPU time delta < 1 s over `stall_timeout_s` | 900 | FAILED (`STALL`) |
| run dir size > `max_run_bytes` (checked every 10 s) | 50 GiB | FAILED (`DISK_QUOTA`) |

High CPU with no output is a factorisation, not a stall.

### 3.4 Crash cleanup

- Manifest fields: `job_id, state, argv, cwd, run_dir, pid, pgid, proc_start_time, created, started, ended, exit_code, signal, solver, threads, label, source_inp, run_mode`.
- A panic hook and the shutdown path run the cancel sequence for all live jobs.
- Reaper at startup: for each manifest in a non-terminal state, if `pid` is alive **and** its start time equals `proc_start_time` (PID-reuse guard), run the cancel sequence on `pgid`; then mark LOST. Linux additionally sets `PR_SET_PDEATHSIG(SIGKILL)` in `pre_exec`.
- Run dirs are never deleted automatically (Q2).

### 3.5 Environment isolation

`env_clear()`, then exactly:

| Variable | Value |
|---|---|
| `PATH` | `/usr/bin:/bin` (Windows: `%SystemRoot%\System32`) |
| `HOME`, `TMPDIR`/`TEMP` | `<run_dir>/tmp` |
| `LANG`, `LC_ALL` | `C` |
| `OMP_NUM_THREADS`, `CCX_NPROC_EQUATION_SOLVER`, `CCX_NPROC_STIFFNESS`, `CCX_NPROC_RESULTS` | `threads` (**ASSUMED** honoured by the MultiSolver fork) |
| names in config `env_passthrough` | copied from the server environment (for `DYLD_LIBRARY_PATH` and similar) |

Solver choice = choice of binary from `[ccx.binaries]` (selection mechanism is UNVERIFIED in the facts file; a per-build binary map works whichever mechanism exists).

---

## 4. Convergence diagnostics

### 4.1 Parsers (cgx-core, sync)

```rust
pub struct StaRow { pub step: u32, pub inc: u32, pub att: u32, pub itrs: u32, pub tot_time: f64, pub step_time: f64, pub inc_time: f64 }
pub struct CvgRow { pub step: u32, pub inc: u32, pub att: u32, pub iter: u32, pub contact_el: u64,
                    pub resid_force: Option<f64>, pub corr_disp: Option<f64>, pub resid_flux: Option<f64>, pub corr_temp: Option<f64> }
pub fn parse_sta(text: &str) -> Vec<StaRow>;
pub fn parse_cvg(text: &str) -> Vec<CvgRow>;
pub struct Tail { /* path, offset */ }
impl Tail { pub fn new(path: PathBuf) -> Self; pub fn poll(&mut self) -> std::io::Result<String>; }
pub fn diagnose(sta: &[StaRow], cvg: &[CvgRow], ctx: &DiagCtx, th: &Thresholds) -> Diagnosis;
```

Parsing rules: a data row is a line whose first whitespace token parses as an integer and which has exactly 7 (`.sta`) or 9 (`.cvg`) tokens; every other line is skipped (covers repeated headers and the `C0NVERGENCE` spelling). A float token that fails to parse, or is `NaN`, `Infinity` or contains `*`, becomes `None`. `Tail::poll` returns only text up to the last `\n`; a shrinking file resets the offset to 0. The `.cvg` data-row layout is **ASSUMED** (no real rows exist in the samples).

`DiagCtx { running: bool, exit_failed: bool, json_steps: Vec<(u32 /*step*/, bool /*converged*/)>, stdout_tail: String }`.

### 4.2 Definitions

An *attempt* is the run of `.cvg` rows sharing `(step, inc, att)`, with residuals `r_k` (`resid_force`, %), corrections `c_k`, contact counts `n_k`, `k = 1..K`. An attempt is *converged* if a `.sta` row exists with the same `(step, inc, att)`; *failed* if a later attempt of the same increment exists, or the job ended in failure and it is the last attempt; otherwise *open*. Contraction `ρ = (r_K / r_1)^(1/(K−1))` for `K ≥ 3`, both values present and positive.

### 4.3 Rules

All thresholds live in `Thresholds` with these defaults. Only the meaning of ATT > 1 comes from the facts file; **every numeric threshold is ASSUMED** and must be recalibrated on real nonlinear telemetry.

| Id | Verdict | Condition | Severity |
|---|---|---|---|
| N1 | NO_TELEMETRY | no `.sta` and no `.cvg` file, or both empty and no json | info |
| L1 | LINEAR | zero `.cvg` rows, and (json present with all steps converged, or `.sta` rows all `att = 1`, `itrs ≤ 1`) | info |
| S1 | SMOOTH | all attempts in the window have `att = 1`, `K ≤ 8`, `ρ ≤ 0.5` where defined, and `inc_time` never decreases across the window | info |
| W1 | SLOW | converged, `att = 1`, but `K > 8` in ≥ 3 of the last 10 increments | warn |
| W2 | SLOW | converged attempt with `0.5 < ρ < 1.0` and `K ≥ 6`, in ≥ 3 of the last 10 increments | warn |
| C1 | CONTACT_CHATTER | attempt with `K ≥ 6`; over its last 6 iterations the sign of `n_k − n_{k−1}` flips ≥ 3 times, `max n − min n ≥ max(2, 0.01·mean n)`, and `min r / max r > 0.1` (residual stagnates) | warn if converged, error if failed |
| C2 | CONTACT_CHATTER | attempt with `K ≥ 10`, `n_k > 0` throughout, last 6 residuals all within a factor 3 of each other and above 0.5 % | same as C1 |
| B1 | CUTBACK | any `.sta` row with `att > 1`, or any `.cvg` attempt index > 1; report cutbacks = Σ(att − 1) | warn |
| B2 | CUTBACK_LOOP | ≥ 3 consecutive increments with `att > 1` | error |
| B3 | CUTBACK_LOOP | any increment reaching `att ≥ 4` | error |
| B4 | CUTBACK_LOOP | latest `inc_time` ≤ 0.01 × the maximum `inc_time` seen in the same step, with ≥ 2 cutbacks in the last 5 increments | error |
| D1 | DIVERGED | in one attempt, for two consecutive `k ≥ 4`: `r_k > r_{k−1}` and `r_k > r_{k−2}` | error if failed/open, warn if a later attempt converged |
| D2 | DIVERGED | `r_k ≥ 1000 × min_j r_j` within an attempt, or `r_k > 1e6` | error |
| D3 | DIVERGED | a residual or correction is `None` after iteration 1 (NaN/overflow) | error |
| D4 | DIVERGED | `exit_failed`, or a json step with `converged == false`, and the last attempt is failed | error |
| D5 | DIVERGED | stdout matches a fatal pattern (§4.4) | error |

Window = the last `last_increments` increments of the selected step. Final `verdict` = highest-ranked rule that fired, rank `DIVERGED > CUTBACK_LOOP > CONTACT_CHATTER > CUTBACK > SLOW > SMOOTH > LINEAR > NO_TELEMETRY`. All fired rules are returned as findings, identical rule ids merged with `occurrences` and the worst instance as `where`. A D1 finding whose increment later converged does not raise the verdict to DIVERGED; it supports B1.

Evidence keys (exact): `K, rho, r_first, r_last, r_min, n_min, n_max, flips, att, cutbacks, inc_time, inc_time_max, consecutive`.

### 4.4 Stdout patterns and advice

Case-insensitive substring match on the last 200 stdout lines (**ASSUMED** strings; confirm against ccx source):

| Pattern | Reported as |
|---|---|
| `increment size smaller than minimum` | D5, "time increment fell below the minimum" |
| `too many attempts` | D5 |
| `*ERROR` | `solver_messages` (first 5 distinct lines); D5 only if the job failed |
| `best solution and residuals are in the frd file` | `solver_messages`; hint to inspect the last frd step |

Fixed advice strings per verdict:

| Verdict | Advice |
|---|---|
| CONTACT_CHATTER | "Inspect *SURFACE BEHAVIOR and *CONTACT PAIR with inp_cards. Consider a softer pressure-overclosure, a smaller initial increment, or contact stabilisation." |
| CUTBACK | "Cutbacks recovered. Reduce the initial increment in the procedure card if they recur." |
| CUTBACK_LOOP | "The increment keeps shrinking. Check the procedure card increments and *CONTROLS; look for abrupt load or contact changes at the reported increment." |
| DIVERGED | "Check for rigid-body motion (missing *BOUNDARY), material instability or excessive load. Run inp_inspect and review the step's cards." |
| SLOW | "Converging but expensive. A smaller increment or a tighter contact definition usually reduces iterations." |

### 4.5 Fixtures

Real (copy into `crates/cgx-core/tests/fixtures/telemetry/`): `beam_benchmark.{sta,cvg}`, `lavffcyc.{sta,cvg,json}` → expect L1.

Synthetic, hand-written `.sta` + `.cvg` pairs, each asserting one verdict and rule set:

| Fixture | Content | Expect |
|---|---|---|
| `smooth` | 10 incs, att 1, K = 4–6, residual ÷10 per iteration, growing inc_time | S1 |
| `slow_iters` | K = 12 in 4 of 10 incs | W1 |
| `slow_rho` | K = 8, ρ ≈ 0.8 | W2 |
| `chatter_converged` | n alternates 120/131, residual 2–4 %, converges at K = 14 | C1 warn |
| `chatter_failed` | same, attempt abandoned, att 2 follows | C1 error + B1 |
| `chatter_plateau` | n constant > 0, residual flat at 1.5 % for 12 iterations | C2 |
| `cutback_single` | one increment with att 2, then clean | B1 |
| `cutback_loop_consecutive` | 4 increments with att 2–3 | B2 |
| `cutback_loop_att4` | one increment with att 5 | B3 |
| `cutback_loop_shrink` | inc_time from 1e-1 to 1e-4 | B4 |
| `diverge_growth` | residual grows from iteration 4, final attempt | D1 error |
| `diverge_recovered` | D1 pattern in att 1, att 2 converges | B1, verdict CUTBACK |
| `diverge_blowup` | residual 0.5 → 9e7 | D2 |
| `diverge_nan` | tokens `NaN` and `********` | D3 |
| `partial_last_line` | file ends mid-row | row ignored, no panic |
| `multi_step_headers` | header repeated per step, 3 steps | step filter works |
| `header_only` | headers, no rows, no json | N1 |

Also required: one real nonlinear contact run with cutbacks from the official test suite, to replace the ASSUMED `.cvg` layout and recalibrate (§8.3).

---

## 5. Token economy

### 5.1 Principles

1. Aggregate in Rust, serialise extrema. No tool ever returns a per-node field. The largest per-node output is `top_n ≤ 50` rows.
2. Tabular data is columnar: `{"columns":[…],"rows":[[…]]}`.
3. An extremum is always value plus node id, in adjacent columns.
4. Floats are serialised from `f32` directly (shortest round-trip form). Never widen to `f64` before serialising (`0.1f32 as f64` prints 17 digits). Non-finite values become `null`.
5. `structuredContent` is the payload; the text block is the same object as compact JSON, no pretty-printing, no prose.
6. Paths are returned relative to the first root when inside it.

### 5.2 Caps, pagination, truncation

| Item | Default | Max |
|---|---|---|
| step rows (`frd_summary`, `frd_query_set`) | 20 | 200 |
| set rows | 50 | 200 |
| `top_n` | 0 | 50 |
| inp list sections | 50 | 200 |
| cards / data lines per card | 10 / 5 | 50 / 50 |
| log tail lines / bytes | 0 / — | 100 / 4096 |
| any string field | — | 300 chars |
| serialised response | — | 24 KiB |

- Every list-bearing response has `page` (`$defs/Page`). `truncated` is true whenever `returned < total`.
- Cursor = `<16 hex>:<offset>`; the hex is FNV-1a 64 over canonical path, file size, mtime (ns) and the canonical JSON of all arguments except `cursor` and `limit`. Mismatch → `CURSOR_STALE` with hint "Repeat the call without cursor."
- Size guard (`page.rs`): after serialising, if > 24 KiB, halve the row count of the largest table and retry until it fits; set `truncated` and `next_cursor` accordingly.
- Truncated text (tails, card data) ends with the literal final line `…[truncated: N more lines]`. Tails keep the end of the file and put the marker first.

### 5.3 FRD cache and the two exact response schemas

Cache (`frd_cache.rs`), key = (canonical path, size, mtime ns):

| Tier | Contents | Location | Serves |
|---|---|---|---|
| Sidecar | `FrdDigest`: title, counts, dataset catalogue, per-step whole-model extrema, set catalogue | `<first root>/.calculix-mcp/cache/<fnv64 of path>.digest.json` (holds the key; stale → rebuilt) | `frd_summary`, `frd_sets` without parsing |
| Model LRU | full `FrdModel` behind `Arc` | memory, `frd_cache_models` = 1 entry | `frd_query_set` |

Load procedure (in `spawn_blocking`, one load per key at a time, concurrent callers share it): `parse_frd_file` → `populate_implicit_sets` → `load_companion_sets(frd_path)` → build digest → write sidecar → insert in LRU. `inp_path`, when given, additionally calls `load_inp_sets` once per (model, inp) pair and refreshes the digest's set catalogue.

Per-step digest extrema: for each `StepInfo`, the first dataset with `field_type == Displacement` → `compute_dataset_disp_invariants(ds, None)`; the first with `field_type == Stress` (prefer `name == "STRESS"`) → `compute_dataset_stress_invariants(ds, None)`.

Guards: file size > `frd_max_file_bytes` (2 GiB) → `FRD_TOO_LARGE`. The file belongs to a non-terminal job → `FRD_BUSY`. If a load does not finish within `frd_sync_budget_s` (20), the tool returns `FRD_LOADING` (`retryable`, `retry_after_s` 15) and the load continues.

**`frd_summary`**

```json
{"input":{"type":"object","required":["path"],"additionalProperties":false,"properties":{
  "path":{"type":"string"},
  "steps":{"$ref":"#/$defs/StepSel"},
  "limit":{"type":"integer","minimum":1,"maximum":200,"default":20},
  "cursor":{"$ref":"#/$defs/Cursor"}}},
 "output":{"type":"object","required":["file","counts","datasets","steps","envelope","page"],"additionalProperties":false,"properties":{
  "file":{"type":"object","required":["path","bytes"],"properties":{"path":{"type":"string"},"bytes":{"type":"integer"},"title":{"type":"string"}}},
  "counts":{"type":"object","required":["nodes","elements","sets","steps"],"properties":{"nodes":{"type":"integer"},"elements":{"type":"integer"},"sets":{"type":"integer"},"steps":{"type":"integer"}}},
  "datasets":{"type":"array","items":{"type":"object","required":["name","kind","components","steps"],"properties":{
     "name":{"type":"string"},"kind":{"enum":["Displacement","Stress","Strain","Temperature","Force","Other"]},
     "components":{"type":"array","items":{"type":"string"}},"steps":{"type":"integer","description":"Number of steps containing it."}}}},
  "steps":{"type":"object","required":["columns","rows"],"properties":{
     "columns":{"const":["idx","step","inc","value","desc","u_max","u_max_node","vm_max","vm_max_node"]},
     "rows":{"type":"array","items":{"type":"array","minItems":9,"maxItems":9}}}},
  "envelope":{"type":"object","description":"Over ALL selected steps, not only the returned page.","properties":{
     "u_max":{"type":["array","null"],"prefixItems":[{"type":"number"},{"type":"integer"},{"type":"integer"}],"description":"[value, node, idx]"},
     "vm_max":{"type":["array","null"],"prefixItems":[{"type":"number"},{"type":"integer"},{"type":"integer"}]},
     "value_range":{"type":"array","prefixItems":[{"type":"number"},{"type":"number"}]}}},
  "notes":{"type":"array","items":{"type":"string"},"description":"Always includes: 'Values are unit-agnostic (deck units). value = time or frequency (Hz) depending on the step.'"},
  "page":{"$ref":"#/$defs/Page"}}}}
```

`desc` is `StepInfo.description` trimmed to 40 characters. Missing datasets give `null` cells.

**`frd_query_set`**

```json
{"input":{"type":"object","required":["path","set","quantity"],"additionalProperties":false,"properties":{
  "path":{"type":"string"},
  "set":{"type":"string","description":"Exact set name from frd_sets, including implicit sets such as +all."},
  "inp_path":{"type":"string","description":"Deck whose *NSET/*ELSET definitions should be added."},
  "quantity":{"enum":["stress","displacement"]},
  "steps":{"$ref":"#/$defs/StepSel"},
  "limit":{"type":"integer","minimum":1,"maximum":200,"default":20},
  "cursor":{"$ref":"#/$defs/Cursor"},
  "top_n":{"type":"integer","minimum":0,"maximum":50,"default":0},
  "top_by":{"enum":["von_mises","s1","s3_min","tresca","hydrostatic","disp"],"description":"Default von_mises for stress, disp for displacement."},
  "top_step":{"type":"integer","minimum":0,"description":"Step idx for the ranking. Default: the envelope step of top_by."}}},
 "output":{"type":"object","required":["set","quantity","steps","envelope","page"],"additionalProperties":false,"properties":{
  "set":{"type":"object","required":["name","kind","nodes"],"properties":{"name":{"type":"string"},"kind":{"enum":["node","element","mixed"]},"nodes":{"type":"integer","description":"Resolved node count."}}},
  "quantity":{"enum":["stress","displacement"]},
  "dataset":{"type":"string"},
  "steps":{"type":"object","required":["columns","rows"],"properties":{
     "columns":{"oneOf":[
        {"const":["idx","step","inc","value","vm_max","vm_max_node","vm_min","vm_min_node","s1_max","s1_max_node","s3_min","s3_min_node","tresca_max","tresca_max_node","hydro_max","hydro_max_node"]},
        {"const":["idx","step","inc","value","u_max","u_max_node","u_min","u_min_node"]}]},
     "rows":{"type":"array","items":{"type":"array"}}}},
  "envelope":{"type":"object","description":"Extreme over ALL selected steps as [value, node, idx]. Keys: vm_max, s1_max, s3_min, tresca_max, hydro_max (stress) or u_max (displacement).","additionalProperties":{"type":"array","prefixItems":[{"type":"number"},{"type":"integer"},{"type":"integer"}]}},
  "top":{"type":"object","properties":{"by":{"type":"string"},"idx":{"type":"integer"},
     "columns":{"const":["node","value","x","y","z"]},"rows":{"type":"array","maxItems":50}}},
  "notes":{"type":"array","items":{"type":"string"}},
  "page":{"$ref":"#/$defs/Page"}}}}
```

Rows come from `compute_dataset_{stress,disp}_invariants(ds, Some(&ids))` with `ids = resolve_set_node_ids(set)`, steps processed with rayon. A step without the dataset yields `null` cells and one note. An empty resolved set is `UNKNOWN_SET` with message "set resolves to 0 nodes". `top` is computed per node from the dataset's component values through `StressTensor` (constructor and component names **ASSUMED**), sorted descending (ascending for `s3_min`), ties by node id.

---

## 6. Security

### 6.1 Paths

- Roots: `--root` flags, else config `roots`, else client `roots/list`, else cwd. Fixed after initialisation.
- Every path argument: reject NUL bytes; resolve relative paths against the first root; `canonicalize` (for files to be created, canonicalise the parent and re-append the file name); require a root as component-wise prefix. Symlinks are therefore judged by their target.
- Extension allowlist (case-insensitive): `inp_*` → `.inp`; `frd_*` → `.frd`; `cgx_run_batch` → `.fbd`, `.fbl`; `ccx_diagnose.job_path` → `.inp`, `.sta`, `.cvg` or none; written files → `.inp`, `.png`.
- The server writes only under `<root>/.calculix-mcp/`, to `inp_consolidate.output_path`, and (in_place) wherever ccx writes beside the deck. Existing files are never overwritten without the explicit flag.
- Include targets and external refs resolved by cgx-core get the same confinement check before any solve.

### 6.2 Subprocesses

- `tokio::process::Command` with argv arrays only. No shell, no string interpolation into a command line.
- Binaries come only from the config file or environment, as absolute paths, validated at startup. No tool argument can name or alter a binary or add argv entries.
- The only caller-influenced argv element is `jobname`, constrained by the regex of §3.2 (cannot start with `-`).
- Environment is cleared (§3.5).
- `cgx_run_batch` executes user scripts and cgx can shell out (`sys` command, **ASSUMED**). Therefore: disabled unless `enable_cgx_batch = true`; before running, the script and every file it `read`s (depth ≤ 8) are scanned and rejected with `INVALID_ARGUMENT` if any line's first token is `sys` (case-insensitive). This is a mitigation, not a sandbox; the tool description says so.

### 6.3 Limits

| Limit | Default |
|---|---|
| concurrent jobs / queue | 1 / 8 |
| threads | ≤ logical CPUs |
| timeout | 3600 s, max 86400 s |
| deck size (sum over include tree) | 2 GiB |
| include count / depth | 1000 / 16 (enforced in the card scanner) |
| `.frd` size | 2 GiB |
| models in RAM | 1 |
| run dir size | 50 GiB |
| concurrent tool calls | 8 (semaphore); frd loads 1 |
| request string argument length | 4096 |

### 6.4 Config

`calculix-mcp.toml` (`--config`, else `<cwd>/calculix-mcp.toml`, else defaults). Scalar keys are overridable by `CALCULIX_MCP_<UPPER_SNAKE_KEY>`.

```toml
roots = ["/abs/path"]
max_concurrent_jobs = 1
max_queue = 8
default_threads = 4
default_timeout_s = 3600
stall_timeout_s = 900
kill_grace_s = 5
frd_max_file_bytes = 2147483648
frd_cache_models = 1
frd_sync_budget_s = 20
max_run_bytes = 53687091200
env_passthrough = []
enable_cgx_batch = false
enable_snapshot = false

[ccx]
default_solver = "mumps"
[ccx.binaries]            # name -> absolute path; names become the `solver` enum
mumps = "/abs/build_mumps/CalculiX"
accelerate = "/abs/build_accelerate/CalculiX"
spooles = "/abs/build_spooles/CalculiX"

[cgx]
binary = "/abs/cgx/bin/cgx_glfw"
```

---

## 7. New cgx-core API (signatures only)

```rust
// parser/inp_cards.rs
pub struct InpCard { pub keyword: String /* upper-case, no '*' */, pub params: Vec<(String, Option<String>)>,
                     pub file: PathBuf, pub line: usize, pub step: u32 /* 0 = model level */,
                     pub data_lines: usize, pub data_offset: u64 /* byte offset of first data line in `file` */ }
pub struct InpScan { pub cards: Vec<InpCard>, pub includes: Vec<InpInclude>, pub total_lines: usize }
pub fn scan_cards(root: &Path, limits: &ScanLimits) -> Result<InpScan>;
pub fn read_card_data(card: &InpCard, max_lines: usize) -> Result<Vec<String>>;
pub struct ScanLimits { pub max_files: usize, pub max_depth: usize, pub max_total_bytes: u64 }
```

Scanner rules: a card starts at a line beginning with `*` but not `**`; keyword = text up to the first comma, trimmed, upper-cased, inner whitespace collapsed to one space; parameters split on commas, `KEY=VALUE` or bare `KEY`, keys upper-cased, values verbatim; header continuation when a header line ends with a comma; `*INCLUDE` is followed in place and tracked for cycles (cycle → error); `step` increments at `*STEP` and returns to 0 after `*END STEP`. External ref = any card other than `*INCLUDE` having parameter `INPUT` or `FILE` (**ASSUMED** complete list).

Telemetry signatures: §4.1.

---

## 8. Decisions, questions, checklist

### 8.1 Decision log

| # | Decision | Rejected | Why |
|---|---|---|---|
| D1 | Jobs are plain tools returning a `job_id`, with long-poll `wait_s` | MCP Tasks as the only async mechanism | Tasks are experimental and absent from most clients; job tools work everywhere and survive reconnects. Tasks can wrap the same supervisor later. |
| D2 | Default `isolated` run dir with a consolidated deck | Always solve beside the deck | Solving in place would overwrite existing results such as a 540 s, 408 MB `lavffcyc.frd`. Cost: external refs must be linked. |
| D3 | Digest sidecar + one-model LRU + `FRD_LOADING` retry | Parse per call; or rewrite cgx-core as lazy/zero-copy now | `FrdModel` materialises every value; per-call parsing is unusable, a parser rewrite is out of scope before the colloquium. The sidecar makes repeat summaries instant. |
| D4 | DTOs in `calculix-mcp/dto.rs` | serde derives on cgx-core types | Keeps cgx-core dependency-free and lets wire shapes (columnar, capped) differ from memory shapes. |
| D5 | `.sta`/`.cvg` parsers and the rule engine live in cgx-core | In the MCP crate | `cgx-top --watch` (Phase 3) needs the same code; one threshold table. |
| D6 | cgx via subprocess, gated | FFI into `cgx.c` | FFI needs cgx's globals, `main`, GL context and `exit()` calls refactored into a library (estimated weeks), breaks crash isolation, and conflicts with the "preserve the core engine" rule. Subprocess cost: ~process start per call, no shared state. |
| D7 | Columnar aggregates only, `top_n ≤ 50` | Per-node field export or CSV in the response | A 48,909-node field is ~500k tokens; extrema with node ids answer the engineering question. Bulk export stays in the `cgx-top --csv` CLI. |
| D8 | Solver = configured binary map | `solver` flag passed to ccx | The fork's selection mechanism is UNVERIFIED; a binary map is correct under either mechanism and keeps binaries allowlisted. |

### 8.2 Open questions for Carlo

1. Should running solves survive a server or client restart? v1 kills them (no orphans). Surviving needs detached processes and re-adoption by pid.
2. Run-dir retention: never delete (v1), or delete terminal jobs older than N days?
3. Can `cgx_glfw -bg` load a `.frd`, plot a dataset and write a PNG without a window? If not, is a cgx-glfw offscreen change acceptable, or should the snapshot become `cgx-top report` HTML (Phase 4)?
4. How does the MultiSolver fork select the solver and thread count (per-build binary, flag, env)? Confirms §3.5 and D8.
5. Is `ccx_diagnose` an acceptable rename of `ccx_convergence_status`?
6. `default_threads`: fixed 4, or physical core count?
7. For the demo, is a first-call wait on `lavffcyc.frd` acceptable if the digest is pre-warmed by calling `frd_summary` once beforehand?
8. Should `in_place` be allowed at all, given the overwrite risk?

### 8.3 Facts needed (resolve before the dependent module)

| Fact | Blocks | Used assumption |
|---|---|---|
| Latest stable MCP revision; `rmcp` version, feature names, macro API, structured-output support | M9 | `2025-11-25`; features in §1.1 |
| Real `.cvg` data rows (run one nonlinear contact case from the official suite) | M2, M3 | 9 numeric tokens per row |
| When `.sta` rows are written; whether failed attempts appear in `.sta` | M3 | only converged increments, ATT = attempt that converged |
| ccx fatal message strings; exit codes on failure | M3, M7 | §4.4 |
| ccx thread environment variables in the fork | M7 | §3.5 |
| Whether ccx resolves `*INCLUDE`/`INPUT=` relative to cwd | M7 | yes |
| Does `consolidate_inp` inline `INPUT=` files and detect include cycles? | M1, M7 | no / unknown |
| `StressTensor` constructor and FRD stress component names | M6 | `SXX,SYY,SZZ,SXY,SYZ,SZX` |
| Parse time and peak RSS for `lavffcyc.frd` | M5 limits | sync budget 20 s, cap 2 GiB |
| cgx `sys` command name; `-bg` exit codes; headless PNG | M11 | §6.2, Q3 |
| `process-wrap` and `sysinfo` current APIs | M7 | §1.1 |

### 8.4 Implementation checklist

Each module is one PR-sized unit; do them in order; `cargo test --workspace` and `cargo clippy -- -D warnings` must pass before the next.

| # | Module | Work | Tests |
|---|---|---|---|
| M0 | workspace | Add `crates/calculix-mcp` to members; resolve §8.3 row 1; pin all versions | `cargo build` |
| M1 | cgx-core `inp_cards` | §7 scanner, `read_card_data`, limits | unit: continuation lines, comments, nested include, cycle, missing include, step numbering; integration: `beam_benchmark.inp`, `lavffcyc.inp` census has `CYCLIC SYMMETRY MODEL` |
| M2 | cgx-core `telemetry::{sta,cvg}` | parsers, `Tail` | real fixtures parse to expected row counts; `partial_last_line`, `multi_step_headers`, `diverge_nan`; `Tail` across appends and truncation |
| M3 | cgx-core `telemetry::diagnose` | `Thresholds`, rules §4.3, verdict ranking, merging | one test per fixture in §4.5 asserting verdict, rule ids and evidence keys |
| M4 | mcp `config`, `paths`, `error`, `page`, `dto` | §6.1, §6.4, §2.2, §5.2 | traversal (`..`, symlink out, absolute outside), extension checks, cursor round-trip and staleness, size guard halving, f32 serialisation (`0.1f32` → `0.1`), nearest-name ranking |
| M5 | mcp `frd_cache` | key, sidecar, LRU, shared in-flight load, budget → `FRD_LOADING` | `beam_benchmark.frd`: digest equals direct computation; second call does not parse (counter); mtime change invalidates; concurrent callers trigger one parse |
| M6 | mcp `frd_tools` | three tools per §5.3 | golden JSON on `beam_benchmark.frd` with `NFIX`/`NTIP` sets; `UNKNOWN_SET` lists `valid`; pagination covers all steps exactly once; `top_n` ordering; output validates against `outputSchema` |
| M7 | mcp `jobs` | manifest, spawn, supervisor, limits, cancel, reaper | fake solver script (`tests/bin/fake_ccx`: modes ok, fail, hang, spawn-grandchild, write-sta-slowly): each transition of §3.1; after cancel no process of the group remains; timeout; stall; reaper marks LOST and kills only on matching start time; manifest atomicity |
| M8 | mcp `inp_tools`, `ccx_tools` | wiring, preflight, long-poll, `next` strings | preflight failure order; `wait_s` returns early on transition; `ccx_diagnose` by path on fixtures; one `#[ignore]` end-to-end test with real ccx on `beam_benchmark.inp` |
| M9 | mcp `server` | rmcp handler, annotations, output schemas, progress, cancellation semantics | in-process client: `initialize`, `tools/list` snapshot test of names, descriptions and schemas; cancelling a waiting request leaves the job RUNNING; nothing but JSON-RPC on stdout (spawn the binary, parse every stdout line) |
| M10 | mcp `resources`, `prompts` | §2.5, §2.6 | list/read/subscribe; update notification on transition; prompt argument validation |
| M11 | mcp `cgx_tools` | only after Q3; gating, `sys` scan, snapshot | disabled → `TOOL_DISABLED`; script with `sys` rejected, including via nested `read` |
| M12 | docs | README section: config, client registration, demo script on `lavffcyc` | manual run of the three prompts |
