# calculix-mcp

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org/)
[![MCP](https://img.shields.io/badge/Model_Context_Protocol-server-6E56CF.svg)](https://modelcontextprotocol.io/)
[![CalculiX](https://img.shields.io/badge/CalculiX-native-2E7D32.svg)](http://www.calculix.de/)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

**Let an AI agent work on real CalculiX models.** `calculix-mcp` is a Model Context Protocol server that gives Claude, Antigravity or any MCP client direct access to your finite-element work: it reads decks with their whole `*INCLUDE` tree, checks the mesh, selects faces by geometry, runs the solver in the background, explains convergence problems, and quotes exact results with node ids from ASCII or binary `.frd` files.

It is written in Rust, works on the decks and result files you already have, and never asks the model to estimate a number it can compute.

```mermaid
flowchart LR
    C["AI client<br/>Claude Code · Antigravity"] -- "MCP (stdio)" --> S["calculix-mcp<br/>this repository"]
    S --> F["Your models<br/>.inp decks · meshes · .frd results"]
    S --> M["CalculiX MultiSolver<br/>solve · JSON telemetry"]
    S -.-> G["CalculiX GraphiX GLFW<br/>meshing · contour images (planned)"]
```

## Part of a CalculiX toolchain

`calculix-mcp` is one of three projects that together give CalculiX a modern, scriptable workflow:

| Project | Role |
|---|---|
| **calculix-mcp-rs** (this repository) | The MCP server and the Rust library behind it: deck inspection, mesh checks and face selection, solver jobs, convergence diagnosis, result queries |
| [**CalculiX MultiSolver**](https://github.com/carlomontec/CalculiX-CrunchiX-MultiSolver) | A multi-platform edition of the CalculiX solver (CrunchiX) with selectable sparse solvers (MUMPS, Apple Accelerate, PARDISO, SPOOLES) and JSON run telemetry, which `calculix-mcp` reads to report each solve |
| [**CalculiX GraphiX GLFW**](https://github.com/carlomontec/CalculiX-GraphiX-GLFW) | A multi-platform edition of the CalculiX pre- and post-processor (GraphiX) on GLFW, with a command channel that will let `calculix-mcp` mesh geometry and render contour plots |

## What a session looks like

Condensed from a real Claude Code session on the 7-sector compressor rotor benchmark (a 74,000-line deck and a 409 MB result file). The numbers are the server's output.

> **You:** Is the rotor deck sound, and what did the last run give?
>
> **Agent** calls `inp_inspect`, `mesh_check` and `frd_summary`, then answers:
>
> The deck is a cyclic-symmetry model (`*CYCLIC SYMMETRY MODEL, N=7`) in 5 files with no missing includes: a static preload step at line 47, then a perturbation frequency step at line 57.
> The mesh is one connected part of 10,816 C3D20R elements with no free nodes and no inverted elements; volume 8,130.8 and mass 2.15e-5 in deck units. The worst aspect ratio is 26.7, at elements 7888 and 7904.
> The result file holds the full 342,139-node expansion. Modes 1, 3 and 5 at nodal diameter 5 are at 8,310, 11,052 and 15,321. Their stress peaks are normalised mode-shape amplitudes, not physical stresses.

Other things you can ask:

| You say | The agent uses |
|---|---|
| "Show me the frequency step and the boundary conditions of this deck." | `inp_cards` |
| "Any free nodes or disconnected parts? What is the mass per material?" | `mesh_check` |
| "Save the upward-facing faces above z = 40 as a surface called TOP." | `mesh_select` |
| "Solve it with MUMPS on 8 threads and tell me when it is done." | `ccx_solve`, `ccx_job_status` |
| "My contact job keeps cutting back. Why?" | `ccx_diagnose` |
| "Peak von Mises on set BLADE_ROOT in every step, and the five worst nodes with coordinates." | `frd_query_set` |

Full transcripts with the raw tool output are in [`crates/calculix-mcp/README.md`](crates/calculix-mcp/README.md).

## Highlights

- Works on existing decks: the whole `*INCLUDE` tree, cyclic symmetry, modal results with mode number and nodal diameter.
- Reads large ASCII and binary `.frd` files lazily; a 409 MB file is summarised in well under a second.
- Solves on a private copy of the deck and always cleans up its processes.
- Returns exact values with node ids, plus notes when a number needs context.

## Install and connect

### 1. Build

```bash
git clone https://github.com/carlomontec/calculix-mcp-rs.git
cd calculix-mcp-rs
cargo build --release -p calculix-mcp
```

The server is `target/release/calculix-mcp`. You need a Rust toolchain, and for solving, a build of [CalculiX MultiSolver](https://github.com/carlomontec/CalculiX-CrunchiX-MultiSolver). Every other tool works without a solver.

### 2. Connect your AI client

Two flags matter: `--root` is the folder the server may read and write (your cases), and `--ccx NAME=PATH` registers a solver build. Repeat `--ccx` to offer several (for example `accelerate`, `spooles`, `mumps`); the first is the default. Use absolute paths.

**Claude Code**: register once from a terminal:

```bash
claude mcp add calculix -- /abs/path/to/calculix-mcp --root /abs/path/to/cases --ccx mumps=/abs/path/to/CalculiX
```

or put this in a `.mcp.json` file at the root of your project, to share the setup with the project:

```json
{
  "mcpServers": {
    "calculix": {
      "type": "stdio",
      "command": "/abs/path/to/calculix-mcp",
      "args": ["--root", "/abs/path/to/cases", "--ccx", "mumps=/abs/path/to/CalculiX"]
    }
  }
}
```

Check that it is connected with `/mcp` inside Claude Code.

**Antigravity**: add the same `mcpServers` block to `~/.gemini/config/mcp_config.json`, then reload the MCP servers in Antigravity.

Claude Desktop reads the same block from its `claude_desktop_config.json`; this setup has not been tested yet.

### 3. Talk to it

Ask in plain language; the agent picks the tools. Paths are relative to `--root`.

```text
What does run/bracket.inp do? Any missing includes?
Check the mesh of run/bracket.inp before I solve it.
Save the bolt hole along z through (40, 0), radius 6, as a surface called BORE.
Solve run/bracket.inp with mumps and tell me when it is done.
What is the peak von Mises stress on set FILLET, and at which nodes?
```

The server never edits your deck: selections go to new include files, and solves run on a copy in `<root>/.calculix-mcp/runs/`.

### Platforms

The configuration is the same JSON on every operating system; only file locations and path syntax differ (on Windows, write paths in JSON as `C:/cases` or `C:\\cases`). The server is developed and tested on macOS. Linux uses the same mechanisms and is expected to work but has not been tested yet. On Windows it is untested, and stopping a solver ends only its direct process, not the processes it started.

All flags are documented in the [server README](crates/calculix-mcp/README.md#run-and-flags).

## Tools

| Stage | Tool | What it does |
|---|---|---|
| Decks | `inp_inspect` | Summary of a deck across its `*INCLUDE` tree: flags, keyword census, step plan, includes, sets, materials |
| | `inp_cards` | Specific keyword cards with file, line and data lines, without loading the whole deck |
| | `inp_consolidate` | Flatten a multi-file deck into one file |
| Mesh | `mesh_check` | Parts, free nodes, inverted elements, aspect ratios, volume, mass and centre of gravity, undefined or empty sets |
| | `mesh_select` | Select exterior faces by geometry and save them as a `*SURFACE` or `*NSET` include file |
| Solve | `ccx_solve` | Start a background solve on a private copy of the deck |
| | `ccx_job_status` | State, progress, memory, convergence verdict and result files; waits for a change instead of polling |
| | `ccx_job_cancel` | Stop a job and every process it started |
| | `ccx_job_list` | Recent and past jobs with state and run-directory size |
| | `ccx_diagnose` | Explain convergence from `.sta`, `.cvg` and `.json` |
| Results | `frd_summary` | Steps, frequencies, fields and whole-model peaks with node ids |
| | `frd_sets` | Node and element sets available for queries |
| | `frd_query_set` | Exact stress invariants or displacement extrema on one set, per step, with the most loaded nodes |

## Built to be trusted

- **Confined.** Every path must resolve inside `--root`; `..` and symlink escapes are rejected.
- **Read-only by default.** Decks are never edited. Selections are written to new include files, and solves run in `<root>/.calculix-mcp/runs/<job_id>/`.
- **No shell.** The solver is started with an explicit argument vector, from a binary named at startup, with a cleared environment.
- **No orphans.** Cancel, timeout and client disconnect end with the solver's whole process group gone, including processes that ignore SIGTERM. After a server crash, the next start finds and stops orphaned solvers.
- **Honest output.** Results carry notes when a number needs context: deck units, normalised mode shapes, upper-bound amplitudes, heuristic thresholds.

## Performance

Release build, open plus stress and displacement histories over the whole model:

| Result file | Full parse | Lazy index (`calculix-mcp`) | Peak memory, full to lazy |
|---|---|---|---|
| Compressor rotor, 409 MB ASCII, 342,139 nodes | 1.97 s | 0.62 s | 808 MB to 456 MB |
| Hammering, 89 MB binary, 366,987 nodes | 1.06 s | 0.12 s | 365 MB to 191 MB |

## Also in this repository

The server is built on a Rust workspace that can be used on its own:

| Crate | Purpose |
|---|---|
| [`calculix-mcp`](crates/calculix-mcp) | The MCP server |
| `cgx-core` | Library: lazy `.frd` index (ASCII and binary), `.inp` card scanner and mesh reader, exterior faces and selections, mesh checks, `.sta`/`.cvg` parsers and convergence rules, stress invariants |
| `cgx-top` | Terminal companion to CalculiX GraphiX: result summaries, set queries with CSV output, deck checks, convergence diagnosis and an interactive TUI |

```bash
cgx-top --summary results.frd                      # steps, fields, extrema
cgx-top results.frd deck.inp -S BLADE_ROOT --mises # von Mises history on a set
cgx-top diagnose run/job                           # convergence verdict from .sta/.cvg
```

## Roadmap

- [x] Deck inspection, mesh checks and face selection
- [x] Background solver jobs with cleanup, quota and crash recovery
- [x] Convergence diagnosis from `.sta` and `.cvg`
- [x] Exact result queries on large ASCII and binary `.frd` files, including cyclic-symmetry modes
- [ ] Contact pair detection between parts
- [ ] Located, reviewable deck edits written to a new file with a diff
- [ ] Geometry and meshing through CalculiX GraphiX batch scripts
- [ ] Contour images rendered by CalculiX GraphiX
- [ ] Solving with stock CalculiX, Windows process handling, CI

Limitations and design notes: [server README](crates/calculix-mcp/README.md#limitations), [`docs/`](docs).

## License

MIT. See [LICENSE](LICENSE).
