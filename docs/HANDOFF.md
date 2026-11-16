# Handoff — cgx-top / calculix-mcp

Date: 2026-11-16. Branch: `feat/frd-lazy-index` (not pushed, not merged to `main`).
Each task below is a self-contained brief for one work session. Read "Ground rules" and "What exists" first.

## Ground rules

- **Never commit or push without Carlo's explicit permission.** Propose a plan before editing code. English only (code, comments, docs).
- **Merge the feature branch to `main` before delegating to subagents in worktrees.** Agent worktrees are cut from `main`; work that exists only on a feature branch or uncommitted is invisible to them.
- `.gitignore` ignores `*.sta`, `*.frd`, `*.dat` and `Cargo.lock`. Files under `crates/*/tests/fixtures/` are re-included by a rule; put test data there, nowhere else.
- Verify before reporting: `cargo test --workspace` and `cargo clippy -p <crate> --all-targets` (pre-existing clippy warnings in `cgx-core/src/parser/frd.rs` and `invariants.rs` are not yours to fix unless the task says so).
- cgx-core stays synchronous and free of serde/tokio. Wire shapes live in `calculix-mcp`.
- Do not read `.frd` files into the conversation; run code on them.
- Sample data (absolute paths, tests must skip when absent):
  - `/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/test_NewLib/` (`beam_benchmark.*`)
  - `/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor/` (`lavffcyc.*`, 409 MB `.frd`)
  - ccx binaries: `/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/build_{mumps,accelerate,spooles,default}/CalculiX`, run as `CalculiX -j <jobname>`
  - CGX: `/Users/carlo/code/CalculiX/cgx/CalculiX-CGX-New3D/` (`cgx_2.23/src/cgx.c`, binary `/Users/carlo/code/CalculiX/cgx/bin/cgx_glfw`)

## What exists

| Area | Where | State |
|---|---|---|
| Lazy `.frd` index | `cgx-core/src/parser/frd_index.rs`, dense extrema in `invariants.rs` | done, equivalence-tested against the full parser |
| `.sta`/`.cvg` parsers + diagnosis | `cgx-core/src/telemetry/` | done; thresholds calibrated on synthetic fixtures + 3 real contact runs without cutbacks |
| `.inp` card scanner | `cgx-core/src/parser/inp_cards.rs` | done |
| CLI | `cgx-top diagnose <job>` added; rest unchanged, still on the old full parser | — |
| MCP server | `crates/calculix-mcp` (rmcp 3.10, stdio) | 10 tools, see below |

MCP tools: `inp_inspect`, `inp_cards`, `inp_consolidate`, `ccx_solve`, `ccx_job_status`, `ccx_job_cancel`, `ccx_diagnose`, `frd_summary`, `frd_sets`, `frd_query_set`.

Server flags: `--root DIR` (repeatable, default cwd), `--ccx NAME=PATH` (repeatable, first is default; without it `ccx_solve` returns `TOOL_DISABLED`), `--env-passthrough NAME`.

Module map of `calculix-mcp/src`: `main.rs` (flags, shutdown), `server.rs` (tool declarations only), `paths.rs` (root confinement), `error.rs` (`ToolError`, `nearest`), `frd_tools.rs`, `inp_tools.rs`, `ccx_tools.rs` (sync functions + DTOs, unit-tested without MCP), `jobs.rs` (job manager).

End-to-end check with a real solver (run from the workspace root after building):

```bash
python3 crates/calculix-mcp/tests/smoke/drive.py /Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver/build_mumps/CalculiX
```

### Decisions taken in the job tools (2026-11-16)

| Decision | Reason |
|---|---|
| The run directory `<root>/.calculix-mcp/runs/<job_id>/` gets a **copy** of the deck and of every file it includes or references, mirroring the layout under the deck directory. `consolidate_inp` is not used. | Consolidation has known bugs (task 2). Mirroring preserves whatever relative-path behaviour ccx has. Files outside the deck directory are rejected with a hint to `inp_consolidate`. |
| One job runs at a time; up to 8 wait in a queue. | A solve uses all cores. |
| Each solver is the leader of its own process group. Cancel, timeout and shutdown send SIGTERM to the group, wait 5 s, send SIGKILL, always `wait()`, then SIGKILL the group once more. | No orphans, including grandchildren that ignore SIGTERM (tested). |
| Running solves are killed when the server stops (stdin EOF, SIGINT, SIGTERM). After a server crash the next start marks them `LOST` and kills the old process only if `ps -o lstart=` still matches the recorded start time. | No zombies; PID reuse cannot kill an unrelated process (tested). |
| Environment is cleared; only `PATH`, `HOME`/`TMPDIR` (private), `LANG`/`LC_ALL=C`, thread variables and `--env-passthrough` names are set. | Isolation. The MUMPS build starts fine this way on macOS. |
| No `sysinfo`/`process-wrap` dependencies: `libc::kill` and `ps` only. Process-group handling is Unix-only; on Windows only the direct child is killed. | Smaller dependency surface for a first version. See task 8. |
| No stall detection, no disk quota, no `ccx_job_list`, no MCP resources/prompts/progress notifications. | Out of scope for the first version. See task 6. |
| Run directories are never deleted. | Deleting results automatically is not safe to decide for the user. |

Known gaps seen while testing: ccx's JSON reported `num_threads: 1` when `threads=2` was requested, so the thread variables (`OMP_NUM_THREADS`, `CCX_NPROC_*`) may not be what this fork reads (task 6). `rss_mb` reads only the group leader.

---

## Task 1 — README and client setup (start here; highest visibility)

**Goal:** a top-level `README.md` section (or `crates/calculix-mcp/README.md`) that lets a CalculiX user install and use the server in ten minutes, and shows the project well to a reader skimming GitHub.

**Content:** what it is and how it differs from STEP/Gmsh wrapper servers (works on real decks, include trees, cyclic symmetry, binary `.frd`, live convergence diagnosis); build (`cargo build --release -p calculix-mcp`); the three flags; a client configuration example for Claude Code (`claude mcp add calculix -- /abs/path/calculix-mcp --root /abs/case/dir --ccx mumps=/abs/path/CalculiX`) and the equivalent JSON block for Claude Desktop (verify both syntaxes against current documentation before writing them); the tool table with one line each (copy descriptions from `server.rs`); one worked transcript on the cube in `crates/calculix-mcp/tests/smoke/` (inspect → solve → status → diagnose → summary → query set), using real output from `drive.py`; measured numbers from `docs/implementation_plan.md` (409 MB result file summarised in 0.6 s); a "Safety" paragraph (path confinement, isolated run directory, argv-only subprocess, cleared environment); limitations (Unix-only process groups, heuristic convergence thresholds, modal displacement stored as PDISP not yet summarised).

**Done when:** every command in the README was actually run and its output pasted from the run; no claim without a measurement.

## Task 2 — fix deck consolidation (`cgx-core/src/parser/inp.rs`)

**Problems (confirmed by reading the code):** `consolidate_inp` reports a file included twice (a diamond) as a circular include because its `visited` set is never popped; its depth limit is hard-coded to 10; it does not inline `INPUT=` files on cards other than `*INCLUDE`. `inspect_inp` silently skips cycles and anything deeper than 10.

**Do:** use a stack (push on enter, pop on leave) for cycle detection in `consolidate_inp`; take limits from `ScanLimits` in `inp_cards.rs`; for a non-INCLUDE card with `INPUT=<file>` emit the card header without the `INPUT` parameter followed by the file's lines; make `inspect_inp` return an error on a cycle instead of skipping. Keep public signatures.

**Tests** (`cgx-core/tests/`, temp-dir decks like `tests/inp_cards.rs`): diamond include consolidates and contains the file twice; real cycle errors; depth 12 works; `*NODE, INPUT=nodes.inc` is inlined; consolidated output of `lavffcyc.inp` has zero `*INCLUDE` lines and the same card census as `scan_cards` on the original, minus `INCLUDE`.

**Then:** in `ccx_tools::prepare_solve`, files outside the deck directory can be handled by consolidating into the run directory instead of rejecting. Add a test.

## Task 3 — `.frd` step metadata and complex results (`cgx-core/src/parser/frd_index.rs`)

**Problems (seen on `lavffcyc.frd`):** the `1PSTEP`, `1PGM`, `1PHID` (nodal diameter) and `1PMODE` records that precede each `100C` block are ignored, so `StepRef` has no increment, mode number or nodal diameter; `PDISP` (components `MAG1..3`, `PHA1..3`) is not treated as displacement, so modal steps have no displacement row; `STRESSI` (imaginary part) contains "STRESS" in its name and is counted as a second real stress dataset by `FrdIndex::stress_history` and the old `compute_stress_history`.

**Do:** parse the `1P*` records in `scan` into new optional fields on `DatasetEntry` (`pstep: Option<(u32,u32,u32)>`, `mode: Option<u32>`, `nodal_diameter: Option<u32>`) and surface them on `StepRef`; in the history functions match stress by exact name `STRESS` (or `field_type == Stress`), never by substring; add `DenseDataset::pdisp_extrema` giving the peak of `sqrt(MAG1² + MAG2² + MAG3²)` and label it as an amplitude bound. Print the raw records first (`grep -a "1P" file | head`) to confirm the column layout; do not guess it.

**MCP:** add `mode` and `nodal_diameter` columns to `frd_summary` when present; use `pdisp_extrema` for `u_max` when a step has `PDISP` and no `DISP`, with a note. Update the tests in `frd_tools.rs`.

**Tests:** synthetic fixtures in `cgx-core/tests/frd_index.rs` (extend the builders there) plus assertions on `lavffcyc.frd` when present: 4 steps, modal steps carry a mode number, one stress row per step.

**Follow-up in the same area, separate change:** switch `cgx-top` (`main.rs`, `tui/`) from `parse_frd_file` to `FrdIndex`, then delete the HashMap-based path if nothing else uses it.

## Task 4 — duplicate set ids (`cgx-core/src/model.rs`, `parser/inp.rs`)

A set defined in both a `.nam` file and the deck ends up with every id twice in `Set.node_ids` (seen on `beam_benchmark`: `NNFIX` 322 ids for 161 nodes). `calculix-mcp` works around it by counting distinct ids. Fix at the source: when a set of the same name is loaded again, merge and de-duplicate while preserving first-seen order. Test with a `.nam` + `.inp` pair in a temp dir, then remove the workaround in `frd_tools::sets` and adjust its test.

## Task 5 — `--json` for the CLI (`crates/cgx-top/src/main.rs`)

Agents with a shell should be able to use `cgx-top` without MCP. Add `--json` to `--summary`, the set queries (`--mises`, `--principal`, `--disp` with `-S`), `check` and `diagnose`. Emit the same shapes the MCP tools return. To avoid two copies of the DTOs, move the sync functions and DTOs (`frd_tools.rs`, `inp_tools.rs`, the diagnose part of `ccx_tools.rs`, `paths.rs`, `error.rs`) from `calculix-mcp` into a new crate `crates/cgx-report` that depends on serde and schemars but not on rmcp or tokio; `ToolError`'s `IntoCallToolResult` impl stays in `calculix-mcp` behind a newtype. Both binaries then call `cgx-report`. Tests move with the code. Done when `cgx-top --summary --json file.frd` and the `frd_summary` tool give byte-identical JSON for the same file.

## Task 6 — harden the job tools (`crates/calculix-mcp/src/jobs.rs`, `ccx_tools.rs`)

In priority order, each with a test using the fake-solver pattern already in `jobs.rs`:

1. **Thread variables.** Read the MultiSolver source (`ccx_2.2x/src/*.c`, grep for `getenv`) and its docs to find how solver and thread count are selected; fix `supervise` accordingly; verify with `telemetry.meta.num_threads` on a real run.
2. **`ccx_job_list`** tool: newest first, columns `job_id, state, jobname, label, elapsed_s, run_dir_mb`, `limit` 1..50; includes finished jobs found on disk.
3. **Stall detection:** no growth of `.sta`, `.cvg`, `stdout.log`, `.frd` and less than 1 s of CPU time over 900 s → FAILED with `STALL`. High CPU with no output is a factorisation, not a stall.
4. **Disk quota** for the run directory (default 50 GiB) → FAILED with `DISK_QUOTA`.
5. **Progress notifications** on `ccx_job_status` with `wait_s` when the request carries a progress token (see `tests/test_progress_subscriber.rs` in the rmcp source under `~/.cargo/registry/src/*/rmcp-3.10.0/`).
6. **Calibration:** run nonlinear cases from the official ccx test suite until one produces cutbacks and one diverges; save their `.sta`/`.cvg` under `cgx-core/tests/fixtures/telemetry/`; revisit thresholds in `Thresholds::default()` and the open point that a blow-up recovered by a cutback currently reads as `DIVERGED`.

## Task 7 — CGX command channel: feasibility study only

**Question:** can a running `cgx_glfw` accept its ordinary commands from another process, so the MCP server can drive a visible CGX session (and render snapshots)?

**Read, do not edit:** `cgx.c` around the `-bg` handling (near lines 7435 and 7839), the command dispatcher (`commandoInterpreter` or equivalent), the GLFW idle/event loop in `cgx_glut_glfw.c`, and the `read`, `sys` and `hcpy` commands. Follow `/Users/carlo/code/CalculiX/AGENTS.md` (portable code, GLFW only, no commits).

**Deliver** `docs/CGX_CHANNEL.md`: where a non-blocking poll of a pipe or local socket could be added to the event loop; which function would execute a received line; thread-safety; whether `-bg` can load a `.frd`, plot a dataset and write a PNG without a window (try it with a small `.fbd` on `test/beam_modal.frd` and report the exact commands and result); security of the `sys` command; a recommended design with an estimate, and the alternative of generating an `.fbd` and running `cgx -bg`. No code changes in CGX.

## Task 8 — portability and packaging

- Windows: put each solver in a Job Object with `KILL_ON_JOB_CLOSE` (crate `windows-sys`, or evaluate `process-wrap`) so cancellation kills the whole tree; replace `ps` in `proc_start`/`rss_mb` with a portable source (evaluate `sysinfo`). Gate tests per platform.
- Port `tests/smoke/drive.py` to a Rust integration test that spawns the built binary with the fake solver (no real ccx needed), asserts that every stdout line is valid JSON-RPC, and runs in `cargo test`.
- CI workflow: build + test on Linux and macOS.
- Decide with Carlo whether to commit `Cargo.lock` (currently ignored; usual practice for binaries is to commit it).

## Task 9 — bring `docs/MCP_SPEC.md` in line with the code

The spec predates the implementation. Rewrite it as reference documentation of what exists: protocol facts (rmcp 3.10.0, latest MCP revision 2026-07-28, tested with a 2025-06-18 handshake), the real tool schemas (dump them with a `tools/list` call), `offset`/`limit` paging instead of cursors, no cache, the job model as implemented, and the diagnosis rules as implemented including the subagent's deviations listed in `docs/implementation_plan.md`. Keep section 4's rule table and section 6's security model; delete what was dropped (digest sidecar, `FRD_LOADING`, resources, prompts) or move it to a short "Not implemented" list.
