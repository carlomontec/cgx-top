# calculix-mcp

A Model Context Protocol (MCP) server for CalculiX. It lets an MCP client (Claude Code, Claude Desktop, or any other) inspect `.inp` decks, run the solver in the background, diagnose convergence from the solver's own log files, and read exact numbers out of `.frd` result files. It speaks JSON-RPC over stdio and is written in Rust on top of `cgx-core`, the parser library of this workspace.

## What it does differently

Most CalculiX wrappers drive a small generated model: write a deck, run it, print a few values. This server is built for decks and results that already exist:

- **Real decks.** `inp_inspect` and `inp_cards` follow the whole `*INCLUDE` tree and report keyword counts, the step plan, missing includes, sets and materials. `inp_consolidate` flattens a multi-file deck into one file on disk.
- **Cyclic symmetry.** Decks with `*CYCLIC SYMMETRY MODEL` are recognised, and result steps carry the mode number and nodal diameter (see the compressor rotor example below).
- **Large and binary result files.** `.frd` files are indexed lazily, in ASCII or binary form, and only the requested dataset is read. A 409 MB result file is summarised without loading it into memory.
- **Convergence diagnosis.** `ccx_diagnose` reads `<job>.sta` and `<job>.cvg` (and `<job>.json` if present) and classifies the run as linear, smooth, slow, contact chattering, cutback loop or divergence, with the evidence, the last increments and suggested next steps.
- **Exact values with node ids.** `frd_query_set` returns von Mises, principal, Tresca, hydrostatic or displacement extrema on a node or element set, per step, with the responsible node ids and coordinates. The model is not asked to estimate these.

## Build

```bash
cargo build --release -p calculix-mcp
```

The binary is `target/release/calculix-mcp`. It needs a Rust toolchain (edition 2021) and no CalculiX installation unless you want `ccx_solve`.

## Run and flags

```bash
calculix-mcp --root /abs/case/dir --ccx mumps=/abs/path/to/CalculiX
```

| Flag | Meaning |
|---|---|
| `--root DIR` | Directory the server may read and write. Repeatable. Default: the current directory. Job run directories are created under the first root. |
| `--ccx NAME=PATH` | Solver binary, run as `CalculiX -j <jobname>`. Repeatable; the first is the default. Without it, `ccx_solve` returns `TOOL_DISABLED`; all other tools still work. |
| `--env-passthrough NAME` | Environment variable to pass to the solver, for example `DYLD_LIBRARY_PATH`. Repeatable. |
| `--max-run-gb GIB` | Size a job's run directory may reach before the job is killed with `DISK_QUOTA`. Default 50. |

stdout carries JSON-RPC only; diagnostics go to stderr. The server stops on stdin EOF, SIGINT or SIGTERM, and kills any running solver first.

## Client setup

Claude Code:

```bash
claude mcp add calculix -- /abs/path/to/calculix-mcp --root /abs/case/dir --ccx mumps=/abs/path/to/CalculiX
```

Claude Desktop, in `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "calculix": {
      "command": "/abs/path/to/calculix-mcp",
      "args": ["--root", "/abs/case/dir", "--ccx", "mumps=/abs/path/to/CalculiX"]
    }
  }
}
```

Use absolute paths. Other MCP clients take the same command and arguments.

## Tools

| Tool | Description |
|---|---|
| `inp_inspect` | Summarise a CalculiX `.inp` deck across its `*INCLUDE` tree: flags, keyword census, step plan, includes (missing ones first), sets, materials. Call first on any deck. |
| `inp_cards` | List keyword cards of a deck (for example `CONTACT PAIR`, `BOUNDARY`, `STATIC`) across its `*INCLUDE` tree, with file:line and the first data lines. |
| `inp_consolidate` | Flatten a multi-file deck into one `.inp` file on disk. Returns statistics, never deck text. |
| `ccx_solve` | Start a solve in the background on a private copy of the deck. The original directory is never written to. Returns a `job_id` immediately. |
| `ccx_job_status` | State, progress, memory, convergence verdict and result files of a job. `wait_s` blocks until the state changes instead of polling. |
| `ccx_job_cancel` | Stop a queued or running job and remove all of its processes. Safe to call on a finished job. |
| `ccx_job_list` | List jobs, newest first, with state, jobname, label, elapsed seconds and run directory size. Includes finished jobs from earlier server runs. Filter with `state`. |
| `ccx_diagnose` | Explain convergence from `.sta`/`.cvg`/`.json`. Give `job_id` for a job started by `ccx_solve`, or `job_path` for any job directory. |
| `frd_summary` | Steps, frequencies or times, available fields and whole-model peak displacement and von Mises stress (with node ids) of an `.frd` file. Call before `frd_query_set`. |
| `frd_sets` | List node and element sets usable in `frd_query_set`, with sizes. Supports a substring filter; pass `inp_path` to add the deck's sets. |
| `frd_query_set` | Exact stress invariants or displacement extrema on one set, per step, with responsible node ids; `top_n` lists the most loaded nodes with coordinates. |

Large listings are paged with `offset` and `limit`. Errors are structured: they carry a `code`, a `retryable` flag and, where it helps, a `valid` list of alternatives (for example the configured solver names).

## Example 1: solve a cube and read the result

This transcript comes from `crates/calculix-mcp/tests/smoke/drive.py`, which solves a one-element cube (`tests/smoke/cube.inp`, 8 nodes, 100 N on the top face) with a MUMPS build of CalculiX. Run from the workspace root:

```bash
cargo build -p calculix-mcp
python3 crates/calculix-mcp/tests/smoke/drive.py /path/to/build_mumps/CalculiX
```

Arguments are shown as the tool call; results are the server's structured output, shortened with `...`.

```text
> ccx_solve  solver="pardiso", inp_path="cube.inp"
{"code": "UNKNOWN_SOLVER", "message": "no solver named 'pardiso'", "retryable": false, "valid": ["mumps"], "valid_total": 1}

> ccx_solve  inp_path="cube.inp", threads=2, label="smoke"
{"files_copied": 2, "flags": [], "job_id": "j1794868841-fb74", "jobname": "cube",
 "next": "Call ccx_job_status with this job_id and wait_s=30.",
 "run_dir": ".calculix-mcp/runs/j1794868841-fb74", "solver": "mumps", "state": "QUEUED", "threads": 2}

> ccx_job_status  job_id="j1794868841-fb74", wait_s=20, tail_lines=6
{"state": "SUCCEEDED", "exit_code": 0, "verdict": "LINEAR", "elapsed_s": 0.0,
 "progress": {"step": 1, "inc": 1, "att": 1, "cutbacks": 0, "increments_done": 1, "tot_time": 1.0, ...},
 "files": {"frd": ".calculix-mcp/runs/j1794868841-fb74/cube.frd",
           "sta": ".../cube.sta", "cvg": ".../cube.cvg", "dat": ".../cube.dat", "json": ".../cube.json",
           "stdout": ".../stdout.log"},
 "telemetry": {"meta": {"ccx_version": "2.21-multisolver", "solver": "MUMPS", "success": true,
                        "mesh_statistics": {"elements": 1, "equations": 12, "nodes": 8}, ...},
               "steps": [{"converged": true, "step_number": 1, "type": "STATIC"}]},
 "next": "Call frd_summary on files.frd", ...}

> ccx_diagnose  job_id="j1794868841-fb74"
{"coverage": {"cvg_rows": 0, "json": true, "sta_rows": 1, "steps": [1]},
 "findings": [{"rule": "L1", "severity": "info", "occurrences": 1, ...}],
 "increments": {"columns": ["step", "inc", "att", "itrs", "tot_time", "inc_time", "contact_el", "resid_force_pct"],
                "rows": [[1, 1, 1, 1, 1.0, 1.0, null, null]]}, ...}

> frd_summary  path=".calculix-mcp/runs/j1794868841-fb74/cube.frd"
{"nodes": 8, "elements": 1, "bytes": 3618,
 "datasets": [{"name": "DISP", "kind": "Displacement", "components": ["D1","D2","D3","ALL"], "steps": 1},
              {"name": "STRESS", "kind": "Stress", "components": ["SXX","SYY","SZZ","SXY","SYZ","SZX"], "steps": 1},
              {"name": "ERROR", "kind": "Other", "components": ["STR(%)"], "steps": 1}],
 "envelope": {"u_max": [0.0018111742, 5, 0], "vm_max": [406.6806, 5, 0]},
 "steps": {"columns": ["idx", "step", "value", "analysis", "u_max", "u_max_node", "vm_max", "vm_max_node"],
           "rows": [[0, 1, 1.0, "", 0.0018111742, 5, 406.6806, 5]]}, ...}

> frd_query_set  path=".../cube.frd", set="NTOP", inp_path="cube.inp", quantity="displacement", top_n=2
{"set": {"kind": "node", "name": "NTOP", "nodes": 4}, "quantity": "displacement",
 "steps": {"columns": ["idx", "step", "value", "u_max", "u_max_node", "u_min", "u_min_node"],
           "rows": [[0, 1, 1.0, 0.0018111742, 5, 0.0018111742, 5]]},
 "top": {"by": "disp", "idx": 0,
         "nodes": {"columns": ["node", "value", "x", "y", "z"],
                   "rows": [[5, 0.0018111742, 0.0, 0.0, 1.0], [6, 0.0018111742, 1.0, 0.0, 1.0]]}}, ...}

> ccx_job_list  limit=5
{"jobs": {"columns": ["job_id", "state", "jobname", "label", "elapsed_s", "run_dir_mb"],
          "rows": [["j1794868841-fb74", "SUCCEEDED", "cube", "smoke", 0.0, 0.0]]}, ...}

> ccx_job_cancel  job_id="j1794868841-fb74"
{"job_id": "j1794868841-fb74", "state": "SUCCEEDED", "was": "SUCCEEDED"}

> ccx_job_status  job_id="j0-dead"
{"code": "UNKNOWN_JOB", "hint": "valid lists the most recent job ids.", "message": "no job 'j0-dead'", "retryable": false, "valid": ["j1794868841-fb74"], "valid_total": 1}
```

The listing of the deck directory after the run is `['.calculix-mcp', 'cube.inp', 'mesh']`: the solver ran in `.calculix-mcp/runs/<job_id>/`, and the original files were not touched. The script also starts a job with a solver that sleeps, closes the server's stdin, and checks that the solver process is gone and the job manifest says `CANCELLED`.

## Example 2: cyclic-symmetry compressor rotor

The benchmark in `benchmarks/compressor_rotor` of the CalculiX MultiSolver tree has a 5-file deck (`lavffcyc.inp` plus includes) and a 409 MB ASCII result file. The server was started with `--root /path/to/benchmarks/compressor_rotor` and three calls were made over stdio.

`inp_inspect path="lavffcyc.inp"` (shortened):

```text
{"root_file": "lavffcyc.inp", "files": 5, "lines": 74017,
 "flags": ["CYCLIC_SYMMETRY", "PERTURBATION"],
 "census": {"BOUNDARY": 1, "CYCLIC SYMMETRY MODEL": 1, "DENSITY": 1, "ELASTIC": 1, "ELEMENT": 1, "HEADING": 1,
            "INCLUDE": 4, "INITIAL CONDITIONS": 1, "MATERIAL": 1, "NODE": 1, "NSET": 2, "SOLID SECTION": 1,
            "SURFACE": 2, "TIE": 1, "TRANSFORM": 1},
 "includes": [{"target": "all.msh", "from": "lavffcyc.inp:9", "bytes": 4966037, "exists": true},
              {"target": "CYC1.nam", "from": "lavffcyc.inp:13", "bytes": 13324, "exists": true},
              {"target": "CYC2.nam", "from": "lavffcyc.inp:14", "bytes": 13521, "exists": true},
              {"target": "SPC_23.bou", "from": "lavffcyc.inp:29", "bytes": 851, "exists": true}],
 "missing_includes": 0, "materials": ["C355"],
 "sets": {"columns": ["name", "kind", "count"], "rows": [["NCYC1", "node", 1685], ["NCYC2", "node", 1685]]},
 "step_plan": [{"n": 1, "procedure": "STATIC", "params": "", "at": "lavffcyc.inp:47",
                "cards": {"DLOAD": 1, "EL FILE": 1, "END STEP": 1, "NODE FILE": 1, "STATIC": 1, "STEP": 1}},
               {"n": 2, "procedure": "FREQUENCY", "params": "PERTURBATION", "at": "lavffcyc.inp:57",
                "cards": {"EL FILE": 1, "EL PRINT": 1, "END STEP": 1, "FREQUENCY": 1, "NODE FILE": 1,
                          "NODE PRINT": 1, "SELECT CYCLIC SYMMETRY MODES": 1, "STEP": 1}}]}
```

`inp_cards path="lavffcyc.inp", keyword="CYCLIC SYMMETRY MODEL"`:

```text
{"cards": [{"at": "lavffcyc.inp:21", "header": "*CYCLIC SYMMETRY MODEL, N=7, NGRAPH=7",
            "data": ["0.,0.,0., 1.,0.,0."], "data_total": 1, "step": 0}], "hint": null, ...}
```

`frd_summary path="lavffcyc.frd"` (datasets list shortened):

```text
{"file": "lavffcyc.frd", "bytes": 408679809, "nodes": 342139, "elements": 75712,
 "datasets": [{"name": "DISP", "kind": "Displacement", "steps": 1},
              {"name": "STRESS", "kind": "Stress", "steps": 4},
              {"name": "ERROR", "kind": "Other", "steps": 4},
              {"name": "STRESSI", "kind": "Other", "steps": 3},
              {"name": "ERRORI", "kind": "Other", "steps": 3},
              {"name": "PDISP", "kind": "Other", "components": ["MAG1","MAG2","MAG3","PHA1","PHA2","PHA3"], "steps": 3}],
 "envelope": {"u_max": [4729.2866, 48955, 3], "vm_max": [3778647.8, 244549, 3]},
 "steps": {"columns": ["idx", "step", "value", "analysis", "mode", "nodal_diameter",
                       "u_max", "u_max_node", "vm_max", "vm_max_node"],
           "rows": [[0, 1,     1.0, "",      null, null, 0.3730079, 146773,    489.50403, 152204],
                    [1, 2,  8309.992, "MODAL", 1, 5, 3534.0767,  46,     1294410.8, 293571],
                    [2, 2, 11052.029, "MODAL", 3, 5, 2831.5156, 319185,  1776341.5, 221569],
                    [3, 2, 15320.563, "MODAL", 5, 5, 4729.2866,  48955,  3778647.8, 244549]]},
 "notes": ["Values are unit-agnostic (deck units). 'value' is time or frequency depending on the analysis.",
           "Steps with idx 1, 2, 3 have no DISP dataset; their u_max is sqrt(MAG1^2 + MAG2^2 + MAG3^2) of the complex PDISP dataset (magnitude and phase per component). This is an upper bound of the vector amplitude over one period, reached only where the three phases coincide.",
           "Steps with analysis MODAL hold mode shapes: 'value' is the eigenfrequency, and their displacement and stress amplitudes are normalised, not physical magnitudes."],
 "steps_total": 4, ...}
```

Step 0 is the static preload step. Steps 1 to 3 are modes 1, 3 and 5 of nodal diameter 5, with frequencies in the unit of the deck (Hz). The file holds 342,139 nodes because ccx wrote the full cyclic expansion of the sector. The modal peaks are mode-shape amplitudes, not physical stresses (see the notes above).

## Performance

Measured on the development machine (release build, 2026-11-16, `docs/implementation_plan.md`). Each figure is open plus the stress and displacement histories over the whole model:

| File | Full parser | Lazy index | Peak RSS full, lazy |
|---|---|---|---|
| `lavffcyc.frd`, 409 MB ASCII, 342,139 nodes, 18 datasets | 1.97 s | 0.62 s (open alone 0.15 s) | 808 MB, 456 MB |
| hammering, 89 MB binary, 366,987 nodes, 12 datasets | 1.06 s | 0.12 s (open alone 0.06 s) | 365 MB, 191 MB |

The lazy figure for resident memory is mostly memory-mapped file pages, which the operating system can reclaim. The server keeps no cache; every request opens the file again.

## Safety model

- **Root confinement.** Every path argument is resolved against `--root` and must stay inside a root. Paths that escape through `..` or symlinks are rejected.
- **Isolated run directory.** `ccx_solve` copies the deck and every file it includes or references into `<root>/.calculix-mcp/runs/<job_id>/` and runs the solver there. The original directory is not written to. Files outside the deck's directory are rejected with a hint to use `inp_consolidate`. Run directories are never deleted by the server.
- **No shell.** The solver is started with an explicit argument vector (`<binary> -j <jobname>`); no argument is passed through a shell, and the solver binary can only be one of those named with `--ccx`.
- **Cleared environment.** The solver gets `PATH`, a private `HOME` and `TMPDIR`, `LANG` and `LC_ALL` set to `C`, the thread-count variables, and any names given with `--env-passthrough`. Nothing else is inherited.
- **Process-group cleanup.** Each solver leads its own process group. Cancel, timeout and server shutdown send SIGTERM to the group, wait 5 seconds, send SIGKILL, reap the child, and send SIGKILL to the group once more. After a server crash, the next start marks orphaned jobs `LOST` and kills the old process only if its recorded start time still matches.
- **One job at a time.** Up to 8 further jobs wait in a queue.
- **Disk quota.** A job whose run directory exceeds `--max-run-gb` (default 50 GiB) is killed and marked failed with `DISK_QUOTA`.

## Limitations

- Process-group cleanup uses Unix signals. On Windows only the direct child process is killed.
- The convergence thresholds are heuristics. They were calibrated on synthetic cases and on three real contact runs that had no cutbacks or divergence. Treat warnings as leads, not as verdicts.
- Modal displacements and stresses in `.frd` files are normalised mode shapes. Their size is not a physical magnitude.
- For steps that have a complex `PDISP` dataset and no `DISP`, `u_max` is `sqrt(MAG1^2 + MAG2^2 + MAG3^2)` maximised over nodes. It is an upper bound of the amplitude, not the amplitude itself.
- There is no stall detection, no MCP resources or prompts, and no progress notifications yet.
- The `threads` argument is passed to the solver through `OMP_NUM_THREADS` and `CCX_NPROC_*`. The solver's JSON output always reports `num_threads: 1` (a constant in the MultiSolver source), so the thread count in effect cannot be confirmed from the output.
- No rendering: the server returns numbers and tables, not images.

## Licence

MIT, as declared in the workspace `Cargo.toml`. See `LICENSE` in the repository root.
