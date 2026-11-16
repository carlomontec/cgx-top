# MCP_FACTS.md — verified facts for the calculix-mcp spec

Gathered by reading code and sample files (2026-11-16). Everything here was observed; items marked UNVERIFIED were not.

## Workspace (/Users/carlo/code/CalculiX/cgx-top)
- Cargo workspace, members `crates/cgx-core`, `crates/cgx-top`. Edition 2021. Deps: memmap2 0.9, rayon 1.10, byteorder, clap 4.5, thiserror 2, anyhow, ratatui 0.29. No async runtime, no serde, no rmcp yet.
- cgx-top CLI subcommands today: `consolidate <inp> [-o out]`, `check <inp>`; flags `--summary --mises --principal --disp -e --csv -c`.

## cgx-core public API (crates/cgx-core/src)
- `parser::parse_frd_file<P: AsRef<Path>>(P) -> Result<FrdModel>` (mmap, then `FrdParserEngine::from_bytes(..).parse_all()`).
- `FrdModel { title, nodes: Vec<Node>, node_index: HashMap<u32,usize>, elements: Vec<Element>, sets: HashMap<String,Set>, steps: Vec<StepInfo> }`.
  - methods: `get_node(id)`, `get_set(name)`, `resolve_set_node_ids(name) -> Option<HashSet<u32>>`, `populate_implicit_sets()`, `load_companion_sets(frd_path)`, `load_inp_sets(inp_path) -> Result<usize>`.
- `StepInfo { step_number, increment_number, value: f32 (time or Hz), description, datasets: Vec<Dataset> }`.
- `Dataset { name, field_type: FieldType{Displacement,Stress,Strain,Temperature,Force,Other}, step_number, value, components: Vec<ComponentData> }`.
- `ComponentData { name, min_val, min_node, max_val, max_node, values: HashMap<u32,f32> }`.
- `Set { name, set_type: SetType{Node,Element,Mixed}, node_ids: Vec<u32>, element_ids: Vec<u32> }`.
- Invariants (`invariants.rs`): `StressTensor::{von_mises, hydrostatic, principal_stresses -> (s1,s2,s3), max_shear}`, `displacement_magnitude`.
  - `compute_dataset_stress_invariants(&Dataset, Option<&HashSet<u32>>) -> Option<StressExtremaSummary>`; `compute_dataset_disp_invariants` likewise.
  - `FrdModel::compute_stress_history(Option<&HashSet<u32>>) -> Vec<StressExtremaSummary>`; `compute_disp_history` likewise.
  - `StressExtremaSummary { step_number, increment_number, value, max_von_mises:(f32,u32), min_von_mises, max_principal_s1, min_principal_s3, max_tresca, max_hydrostatic, node_count }` (tuple = (value, node_id)).
  - `DispExtremaSummary { step_number, increment_number, value, max_disp:(f32,u32), min_disp, node_count }`.
- INP (`parser/inp.rs`): `inspect_inp(path) -> Result<InpSummary>`; `consolidate_inp(path) -> Result<(String, InpConsolidateStats)>`; `parse_sets_from_inp(path) -> Result<HashMap<String,Set>>`; `parse_sets_from_str`; `load_companion_sets(frd_path)`.
  - `InpSummary { root_file, includes: Vec<InpInclude>, node_sets: HashMap<String,usize>, element_sets: HashMap<String,usize>, materials: Vec<String>, step_count, boundary_count, has_nodes, has_elements, total_lines, total_files_count }`.
  - `InpInclude { raw_target, resolved_path, exists, file_size_bytes, line_number, included_from }`.
  - `InpConsolidateStats { root_file, output_file, total_files_inlined, total_lines_written, inlined_files }`.
- Error type: `FrdError` (thiserror) in lib.rs; `Result` alias.
- NO serde derives on any of these types. NO `.sta`/`.cvg`/`.json` parsers exist yet.

### Discrepancies vs. the earlier summary (important for the design)
1. The parser is mmap-based, but `FrdModel` is NOT zero-copy: every dataset component is materialised as `HashMap<u32,f32>` of all nodal values. A full parse of lavffcyc.frd (408 MB, 48,909 nodes, many modes) is therefore heavy in RAM and time. The spec must decide: parse-per-call vs. cached model with LRU/size cap vs. a lighter lazy/summary path. Do not assume cheap repeated parses.
2. `frd_summary` extrema can come from `ComponentData.{min,max}_{val,node}` without recomputing invariants; set-filtered invariants need `values` and `resolve_set_node_ids`.
3. Values are `f32`; units are not interpreted (unit-agnostic).
4. There is no `inp_inspect` data on BCs/loads beyond `boundary_count`, nor per-step definitions beyond `step_count`. Richer inspection is new work in cgx-core.

## ccx (/Users/carlo/code/CalculiX/ccx/CalculiX-CrunchiX-MultiSolver)
- Docs: JSON_EXPORT.md, BENCHMARK_ACCELERATE.md, CHANGES_VS_CCX.md. Build dirs per solver: build_mumps, build_accelerate, build_spooles, build_default, ...
- Invocation: `ccx -j <jobname>` / `--json`, or env `CCX_JSON=1`. Built binary example: `./build_mumps/CalculiX -j beam`. Jobname has no `.inp` extension. Solver selection mechanism (flag/env/per-build) is UNVERIFIED: read the source before specifying (spec should list it as a Facts-needed item if not resolved).
- `<job>.json` is written atomically at step completion (not per iteration), NaN/Inf encoded as null. Real example (lavffcyc.json, 922 bytes):
```json
{"meta":{"jobname":"lavffcyc","ccx_version":"2.21-multisolver","solver":"SPOOLES","num_threads":1,"success":true,"exit_code":0,
  "timings":{"total_wall_time_s":540.599330},"mesh_statistics":{"nodes":48909,"elements":10816,"equations":283024}},
 "steps":[{"step_number":1,"type":"STATIC","converged":true},
          {"step_number":2,"type":"FREQUENCY","converged":true,"total_effective_mass":{"x":..,"rx":..},"fraction_of_totals":{"x":..,"rz":..}}]}
```
  Note: no per-iteration data, no live progress, no RSS/CPU; the file appears only after a step completes.
- `.sta` format (real, whitespace columns, header repeated per step block):
```
SUMMARY OF JOB INFORMATION
  STEP      INC     ATT  ITRS     TOT TIME     STEP TIME      INC TIME
     1          1     1     1  0.100000E+01  0.100000E+01  0.100000E+01
```
  Columns: STEP, INC, ATT (attempt; >1 means cutback), ITRS (iterations), TOT TIME, STEP TIME, INC TIME (Fortran E-format).
- `.cvg` format: header only in all available samples:
```
SUMMARY OF C0NVERGENCE INFORMATION      <- note the zero in "C0NVERGENCE"
  STEP   INC  ATT   ITER     CONT.   RESID.        CORR.      RESID.      CORR.
                              EL.    FORCE         DISP       FLUX        TEMP.
                              (#)     (%)           (%)        (%)         (%)
```
  Columns: STEP, INC, ATT, ITER, CONT.EL (#), RESID.FORCE (%), CORR.DISP (%), RESID.FLUX (%), CORR.TEMP (%). Data rows exist only for nonlinear steps. NO real data rows were available in any sample.
- Sample data: `test_NewLib/beam_benchmark.{inp,frd(10MB),sta,cvg,dat}`, `beam_modal.inp`, `beam_verydense.inp`, `*.fbl`, `NFIX.nam NTIP.nam`, `originaltest_json/` (621 entries, reference JSONs), `originaltest_json_report.md`, `testsuite_results/`, `run_official_testsuite.py`, `json_checker.py`.
- Demo case lives in `benchmarks/compressor_rotor/`: `lavffcyc.{inp,frd (408 MB, DO NOT READ),sta,cvg,json,dat}`, `CYC1.nam CYC2.nam SPC_23.bou all.msh lavff.fbd mesh_{coarse,medium,fine}.fbl`. Its .sta/.cvg are header-only (linear static + frequency, no cutbacks), so contact/cutback/divergence rules have NO real data. Other copies: `/Users/carlo/code/CalculiX/CalculixExamples/examples/cgx/compressor/`.
- Only linear cases are available as real telemetry; nonlinear convergence fixtures (cutbacks, contact chatter, divergence) must be synthesized or generated by running a nonlinear ccx example (UNVERIFIED availability in `originaltest_json` / official test suite).

## cgx-glfw (/Users/carlo/code/CalculiX/cgx/CalculiX-CGX-New3D)
- Source `cgx_2.23/src/cgx.c`; binaries in `/Users/carlo/code/CalculiX/cgx/bin/{cgx, cgx_glfw}`. Project docs: AGENTS.md, README.md, GEMINI.md. Rules from AGENTS.md: no commits without permission, plan first, keep core portable (GLFW only).
- CLI (from cgx.c usage text): `cgx <file>.frd [<file>.inp]` post-processing; `-b <file>[.fbd]` interactive build; `-bg <file>.fbd` headless batch (executes all commands, no GUI; line ~7435, parsed at ~7839); `-a` auto build; `-c <file>.inp` solver-deck check; `-step`, `-stl`, `-ng`, `-tg`, `-foam`, `-vtk` importers.
- Note: `-bg` takes `.fbd`; `.fbl` macros exist in test_NewLib. Whether `-bg` can load an `.frd`, render a contour and write a PNG headlessly (hardcopy without an OpenGL window) is UNVERIFIED. A snapshot tool depends on this; the spec should list it as an open question or a required cgx-glfw change.

## Gaps the spec must handle
- No async/JSON-RPC/serde infrastructure exists in the workspace; `rmcp` version and API are UNVERIFIED (spec must say "pin after checking crates.io").
- No live-progress source: `.json` only at step end; `.sta`/`.cvg` are appended during the run (tail-able), which is the only mid-run signal. RSS/CPU come from the OS process, not ccx.
- Memory model of `FrdModel` (see Discrepancy 1) is the main token/RAM-economy risk.
