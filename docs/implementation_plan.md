# Implementation plan — CLI-first CalculiX tooling, MCP as adapter

Date: 2026-11-16. Supersedes the scope (not the detailed schemas) of `docs/MCP_SPEC.md`.
Priority: a durable community tool, not a demo.

## Direction

1. **Capabilities live in `cgx-core`**, exposed first through the `cgx-top` CLI with `--json`. Any agent with a shell can use them without MCP.
2. **`calculix-mcp` is a thin adapter** over the same functions: job management, path confinement, schemas. Reduced to about 7 tools; no resources, prompts or subscriptions in v1.
3. **CGX gets a command channel, not a protocol**: a running CGX accepts its ordinary commands from a pipe or socket, and the Rust server translates. Needs a feasibility check in `cgx.c` first.
4. **Fix the `.frd` memory model in cgx-core** instead of caching around it.

## Roadmap (one step = one reviewable change; do not start a step without approval)

| Step | Deliverable | Status |
|---|---|---|
| 1 | cgx-core: lazy `.frd` access (`FrdIndex`, on-demand dataset load, streaming histories) | done, see result below |
| 2 | cgx-core: `.sta`/`.cvg` parsers + `diagnose`; `cgx-top diagnose <job>` (MCP_SPEC §4) | pending |
| 3 | `--json` output for `--summary`, set queries, `check`, `diagnose` (shapes from MCP_SPEC §5.3) | pending |
| 4 | cgx-core: `.inp` card scanner; richer `check`; `cgx-top cards` (MCP_SPEC §7) | pending |
| 5 | `crates/calculix-mcp`: `inp_inspect`, `inp_cards`, `ccx_solve`, `ccx_job_status`, `ccx_job_cancel`, `ccx_diagnose`, `frd_summary`, `frd_query_set` | pending |
| 6 | CGX command channel: feasibility read of `cgx.c`, then design | pending |

The digest-sidecar cache of MCP_SPEC §5.3 is dropped if step 1 makes summaries fast enough; decide after measuring.

## Step 1 — lazy FRD access

### Findings in the current parser (`crates/cgx-core/src/parser/frd.rs`)

- `parse_all` materialises every dataset; each component is a `HashMap<u32, f32>` (6 maps per stress dataset, one insert per value).
- Binary payloads have a known length (`nod_data × record size`), so they can be skipped without reading.
- `StepInfo.increment_number` is hard-coded to 1 and `description` is just the first dataset name. Datasets are grouped by the header's step counter only.
- ASCII continuation records (`-2`) are ignored, so only the first 6 components of a dataset are read.

### Design

New module `parser/frd_index.rs`, additive. Existing `parse_frd_file` and `FrdModel` stay untouched in this step, so the CLI and TUI keep working.

```rust
pub struct FrdIndex { /* mmap, title, nodes, node_index, elements, entries */ }
pub struct DatasetEntry {
    pub name: String, pub field_type: FieldType,
    pub step_number: u32, pub value: f32, pub analysis: String,   // header cols 63..73
    pub components: Vec<(String, i32)>,                            // (name, iexist)
    pub node_count: usize, pub format: u8,                         // 0/1 ASCII, 2 f32, 3 f64
    pub payload: std::ops::Range<usize>,                           // byte range in the file
}
pub struct DenseDataset {                 // one loaded dataset, no hashing
    pub node_ids: Vec<u32>,               // file order
    pub columns: Vec<Vec<f32>>,           // one per component, aligned with node_ids
}
impl FrdIndex {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self>;          // mesh + headers only
    pub fn entries(&self) -> &[DatasetEntry];
    pub fn steps(&self) -> Vec<StepRef>;                            // entries grouped by step_number
    pub fn load(&self, entry: usize) -> Result<DenseDataset>;
    pub fn stress_history(&self, filter: Option<&HashSet<u32>>) -> Vec<StressExtremaSummary>;
    pub fn disp_history(&self, filter: Option<&HashSet<u32>>) -> Vec<DispExtremaSummary>;
}
```

- `open`: parses nodes and elements as today; for each `100C` block reads the `-4`/`-5` header lines, then skips the payload (binary: arithmetic; ASCII: scan to the `-3` line without parsing floats).
- `load`: parses one payload into dense columns.
- Histories: rayon over steps; each task loads one dataset, computes the extrema, drops it. Peak memory is one dataset per thread instead of the whole file.
- Invariant maths is reused from `invariants.rs` by adding slice-based helpers next to the HashMap-based ones; the existing functions are not changed.

### Tests

- For `beam_benchmark.frd`: `FrdIndex` entry count, names and step numbers equal those of `parse_frd_file`.
- `load(i)` values equal the HashMap values of the full parse for every node, every component.
- `stress_history`/`disp_history` equal `FrdModel::compute_*_history`, with and without a set filter.
- Small hand-written ASCII and binary (f32, f64) fixtures: truncated payload returns an error, not a panic.
- Benchmark (ignored test, printed): `open` and full history time and peak RSS versus `parse_frd_file`.

### Result (2026-11-16, done, not committed)

Files: `parser/frd_index.rs` (new), `invariants.rs` (dense extrema), `parser/reader.rs` (`pos`, `seek`), visibility of helpers in `parser/frd.rs`, `tests/frd_index.rs` (7 tests + 2 ignored benchmarks). All workspace tests pass.

Release build, open + stress and displacement histories, whole model:

| File | Full parser | `FrdIndex` | Peak RSS full → lazy |
|---|---|---|---|
| `lavffcyc.frd` (409 MB ASCII, 342,139 nodes, 18 datasets) | 1.97 s | 0.62 s (open alone 0.15 s) | 808 MB → 456 MB |
| hammering (89 MB binary, 366,987 nodes, 12 datasets) | 1.06 s | 0.12 s (open alone 0.06 s) | 365 MB → 191 MB |

The lazy RSS is mostly mapped file pages, which the OS can reclaim.

Consequences for `MCP_SPEC.md`: the digest sidecar, the model LRU and the `FRD_LOADING` retry contract (§5.3) are unnecessary and are dropped. `frd_summary` and `frd_query_set` call `FrdIndex` directly on every request.

### Follow-ups found while testing on `lavffcyc.frd`

- The `.frd` holds 342,139 nodes, not 48,909: ccx wrote the cyclic-symmetry expansion.
- `1PSTEP`, `1PGM`, `1PHID` (nodal diameter) and `1PMODE` records precede each block and are ignored. They carry the real step/increment/mode numbering.
- `PDISP` (magnitude/phase) is not reported as displacement, so modal steps have no displacement row. `STRESSI` (imaginary part) is reported as if it were a second real stress dataset.
- The `-4`/`-5` name fields are read one column early (4..12 instead of 5..13). Harmless for names up to 7 characters; kept identical to the full parser for now.

### Out of scope for step 1

Migrating the CLI/TUI to `FrdIndex`, fixing `increment_number`/`description`, reading `-2` continuation records. Each becomes its own follow-up once step 1 is verified.

## Status log

### 2026-11-16 — MCP server first slice (uncommitted, branch `feat/frd-lazy-index`)

- Order changed: the MCP adapter is built first; steps 2 (telemetry + diagnose) and 4 (card scanner) are delegated to Sonnet subagents in separate worktrees and integrated when they report.
- Verified facts: `rmcp` is at **3.10.0** (features `server`, `macros`, `transport-io`, `schemars`; schemars 1.x). The latest MCP revision is **2026-07-28**; rmcp negotiates down, and the server was exercised with a `2025-06-18` handshake. `MCP_SPEC.md` §1 is outdated on both points.
- `crates/calculix-mcp` exists and serves over stdio: `inp_inspect`, `inp_consolidate`, `frd_summary`, `frd_sets`, `frd_query_set`. Structured output with output schemas, annotations, structured errors with `valid` alternatives, path confinement (`--root`, repeatable).
- Simplifications against `MCP_SPEC.md`: plain `offset`/`limit` paging instead of hashed cursors; no cache (a fresh `FrdIndex` per call); no resources, prompts or config file yet.
- Still to do in the adapter: `ccx_solve`, `ccx_job_status`, `ccx_job_cancel` (job supervisor, MCP_SPEC §3), then `ccx_diagnose` and `inp_cards` once the subagent work is merged.
- Found: sets defined in both a `.nam` file and the deck get their ids listed twice in `FrdModel.sets` (counts doubled; the MCP tools count distinct ids). `beam_benchmark.inp` in `test_NewLib` references `all.msh` and `TOP.dlo`, which are not in that folder.

### 2026-11-16 — card scanner integrated (uncommitted)

- Step 4 delivered by a Sonnet subagent and copied in: `parser/inp_cards.rs` + `tests/inp_cards.rs` (22 tests). `inp_inspect` now returns flags, keyword census, step plan and external refs; new tool `inp_cards`. Checked live on `lavffcyc.inp`.
- Learned about `inp.rs`: `consolidate_inp` does not inline `INPUT=` on non-INCLUDE cards; its cycle check never pops, so a file included twice (diamond) is wrongly reported as circular; depth limit hard-coded to 10; a missing include is an error there. `inspect_inp` silently skips cycles and depth > 10. To fix before `ccx_solve` relies on consolidation.
- Subagent worktrees branch from `main`, not from the feature branch, so their briefs must not rely on uncommitted or branch-only files.
- Pending: telemetry/diagnose subagent (step 2) still running; then `ccx_diagnose`; then the job tools.

### 2026-11-16 — telemetry + diagnose integrated (uncommitted)

- Step 2 delivered by a Sonnet subagent and copied in: `cgx-core/src/telemetry/{sta,cvg,diagnose}.rs`, `tests/telemetry.rs` (29 tests), 43 fixture files, `cgx-top diagnose <job>`. New MCP tool `ccx_diagnose` (path-based). Server now has 7 tools; 10 unit tests in `calculix-mcp`.
- Real `.cvg` rows obtained from ccx runs of `contact8/9/13`: the 9-column layout is confirmed. None of the real runs had a cutback or a divergence, so the C/B/D thresholds remain calibrated on synthetic fixtures only.
- Deviations from MCP_SPEC §4 made by the subagent, accepted: relative-growth branch of D2 requires the residual above 1 % (a healthy real run tripped the literal rule); the last increment of a step is excluded from the S1 "increment never shrinks" check; D2/D3 on an attempt that later recovered still read as DIVERGED (to revisit with real cutback data).
- Facts correction: `lavffcyc.sta` and `beam_benchmark.sta` are not header-only; each has one linear row.
- Remaining for the adapter: `ccx_solve`, `ccx_job_status`, `ccx_job_cancel` (MCP_SPEC §3), feeding `running`, exit status and stdout into `ccx_diagnose`. Before that: fix the `consolidate_inp` issues listed above.
- Remaining elsewhere: step 3 (`--json` on the CLI), step 6 (CGX command channel), FRD follow-ups (PSTEP/PHID/PMODE metadata, PDISP, STRESSI), duplicate set ids.
