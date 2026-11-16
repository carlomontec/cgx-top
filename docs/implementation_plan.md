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
