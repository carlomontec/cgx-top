# Colloquium Feature Roadmap: Extending `cgx-top` & `cgx-core`

## Context & Architectural Alignment

This document captures the next-generation feature concepts developed during the CalculiX Colloquium preparation (in coordination with the `CalculiX-CrunchiX-MultiSolver` benchmark suite and `cgx-glfw`). It is structured so that the `cgx-top` development workflow (`0d0a5a15-bb2a-47c5-bd78-eba13d7278cb`) can integrate these features cleanly on top of what is already built.

### Baseline Status in `cgx-top` (Already Completed)
- **Phase 1 (`cgx-core`)**: Completed. Memory-mapped (`memmap2`) ASCII and binary `.frd` parser (single and double precision), unit-agnostic architecture, `rayon`-accelerated tensor invariants (von Mises, Cardano-Viète Principal stresses $\sigma_1 \ge \sigma_2 \ge \sigma_3$, Tresca, hydrostatic), `.inp` recursive `*INCLUDE` consolidation, `check` / `-c` inspection, companion `.inp`/`*.nam` set auto-discovery, and implicit topology sets (`+C3D20`, `+all`, etc.).
- **Phase 2 (`cgx-top` CLI & TUI)**: Completed. Batch CLI (`--summary`, `--mises`, `--principal`, `--disp`, `-e/--equidistant`, `--csv`, `consolidate`, `check`), Unicode sparklines, $2 \times 4$ Braille Bresenham plotter (`ascii_plot.rs`), and full-screen Ratatui interactive dashboard with WASD/arrow navigation, set rotation, and step summary caching.
- **Note on Original Phase 3 (Terminal 3D ASCII/Braille Wireframe)**: Dropped / Replaced. Rendering dense FEA meshes (e.g., 48k+ nodes in `lavffcyc` or 366k nodes in `hammering`) as monochrome Braille wireframes in a terminal turns into an unreadable dot blob and lacks contour fidelity. Real engineering value comes from **Live `.sta`/`.cvg` Solver Telemetry (`--watch`)**, **Single-File Interactive 3D WebGL HTML Reports (`report`)**, and **Native MCP Integration (`mcp`)** below, plus quick TUI polish (`e` for in-TUI CSV export, `/` for Node ID search).

---

## Strategic Differentiation vs. Existing Third-Party Tools

A third-party project (`superWorldSavior/mcp-calculix` / `@casys/mcp-calculix`) exists on GitHub, which wraps Gmsh and CalculiX in a TypeScript/Docker container for STEP files. However:
1. It completely bypasses CGX (`*.fbd` / `*.fbl`).
2. It blocks raw `.inp` decks and only exposes 5 fixed JSON templates.
3. It cannot inspect existing `.inp` decks, `*INCLUDE` trees, cyclic symmetry (`*CYCLIC SYMMETRY MODEL`), prestressed modal perturbation (`lavffcyc.inp`), contact, or arbitrary binary `.frd` files.

By contrast, our architecture is **100% CalculiX-native**: it operates directly on real `.inp`, `.fbd`/`.fbl`, `.sta`, `.cvg`, `.json`, and ASCII/binary `.frd` files at native Rust (`cgx-core`) and C/OpenGL (`cgx-glfw`) speed.

---

## New Feature Proposals for `cgx-top` / `cgx-core`

### Phase 3: Live Solver & Nonlinear Convergence Monitor (`cgx-top --watch <job>`)

Every CalculiX engineer debugging nonlinear contact, hyperelasticity, plasticity, or turbomachinery preload (`lavffcyc`) currently runs `tail -f job.sta` and `job.cvg` in separate terminals. Adding a live watch mode turns `cgx-top` into a true real-time `htop`/`btop` for running CalculiX jobs.

#### Core Capabilities
1. **Real-Time `.sta` and `.cvg` Parsers in `cgx-core`**:
   - Parse `<job>.sta` (step, increment, attempt, cutback count $U$, step time, increment size $d\theta$).
   - Parse `<job>.cvg` (equilibrium iteration count, residual force % vs. max force, displacement correction % vs. max displacement change, contact elements).
   - Detect active equation solver and thread count from `<job>.json` or stdout/process telemetry (`MUMPS`, `Accelerate`, `PARDISO`, `SPOOLES`) alongside live RSS memory and CPU utilization.
2. **Live Ratatui Telemetry View (`cgx-top --watch <job>`)**:
   - **Top Pane**: Step/Increment progress bar, current $d\theta$, cutback warnings (highlighted when `att > 1`), active solver badge, elapsed wall clock time, and peak RSS RAM.
   - **Middle Pane**: Real-time Braille convergence curve (`ascii_plot.rs`) plotting **Force Residual (%)** and **Increment Size ($d\theta$)** across iterations/increments so divergence or contact chattering is immediately visible.
   - **Bottom Pane**: Live incremental `.frd` tailing — as soon as CCX flushes a completed step/increment to `<job>.frd`, `cgx-core` re-maps the file and updates the live peak displacement and von Mises stress with its responsible Node ID.

---

### Phase 4: Single-File Interactive 3D WebGL Report (`cgx-top report <file.frd> -o <out.html>`)

While a full Emscripten/WASM port of `cgx-glfw` is high-risk, `cgx-core` already parses the 3D mesh topology, nodal coordinates, displacement vectors, eigenfrequencies, and stress tensors in memory.

#### Core Capabilities
1. **Exterior Surface Face Extraction (`cgx-core`)**:
   - Extract boundary faces from 3D solid elements (`C3D4`, `C3D8`, `C3D10`, `C3D20`/`C3D20R`) and shell elements (`S3`, `S4`, `S6`, `S8`) so only visible exterior triangles are serialized.
2. **Self-Contained Single-File HTML Generator**:
   - Emit a single standalone `.html` file (zero external server required; opens directly via `file://` in any browser or slide presentation).
   - Embed compact binary/base64 buffers for vertex coordinates, surface triangle indices, displacement/mode vectors, and scalar invariants (von Mises, $\sigma_1$, $\sigma_3$, Tresca, $|u|$).
   - Interactive Three.js / WebGL viewport featuring:
     - **Cubehelix perceptual colormap** matching `cgx-glfw`.
     - **Step / Eigenmode selector** and **real-time deformation scale slider** (with harmonic sine animation for `*FREQUENCY` modes like the 7-blade compressor rotor `lavffcyc`).
     - **Set filter dropdown** (highlighting or isolating `.inp`/`*.nam` sets parsed by `cgx-core`).
     - **Metadata & Solver Summary Card** (if `<job>.json` from `ccx -j` is present, display solver name, DOF count, and wall-clock runtime).

---

### Phase 5: Native CalculiX Model Context Protocol Server (`calculix-mcp` / `cgx-top mcp`)

Instead of a heavy Python or Docker wrapper, expose `cgx-core`, `cgx-glfw`, and `ccx` MultiSolver through a lightweight Rust MCP server over stdio JSON-RPC 2.0.

#### Workspace Packaging Options
- **Option 1 (Subcommand or Binary inside `cgx-top` workspace)**: Add `crates/calculix-mcp` (or `cgx-top mcp` subcommand) inside the `cgx-top` Cargo workspace so both `cgx-top` and `calculix-mcp` share `crates/cgx-core` directly.
- **Option 2 (Standalone Repo)**: A dedicated `calculix-mcp` repository depending on `cgx-core = { git = "https://github.com/carlomontec/cgx-top" }`.

#### MCP Tool Surface (Pre + Solve + Post)
1. **Pre-Processing & Deck Diagnostics (`cgx-core` + `cgx-glfw -bg`)**:
   - `inp_inspect`: Calls `cgx-core`'s `.inp` inspector (`check` logic) to return node/element counts, `*INCLUDE` dependency trees, defined sets, materials, boundary conditions, and step definitions.
   - `inp_consolidate`: Calls `cgx-core::consolidate_inp` to flatten multi-file include decks into a single inspectable `.inp`.
   - `cgx_run_batch`: Executes an `.fbd`/`.fbl` script headlessly via `cgx -bg` (e.g., parametric meshing or set generation) and returns newly generated sets/mesh statistics.
2. **Solving & Convergence Diagnostics (`ccx` MultiSolver + `cgx-core`)**:
   - `ccx_solve`: Runs `ccx -j <job>` with selectable solver (`MUMPS`, `ACCELERATE`, `PARDISO`, `SPOOLES`) and thread count, returning structured JSON telemetry.
   - `ccx_convergence_status`: Reads `<job>.sta` and `<job>.cvg` via `cgx-core` to explain cutbacks, non-convergent equilibrium iterations, or large residual forces.
3. **Post-Processing & Visual Verification (`cgx-core` + `cgx-glfw -bg`)**:
   - `frd_summary`: Returns step list, modal frequencies, and global extrema via `cgx-core` zero-copy `memmap2`.
   - `frd_query_set`: Queries exact von Mises, Principal ($\sigma_1, \sigma_3$), Tresca, hydrostatic stress, or displacement magnitude on any `.inp`/`*.nam` or implicit topology set across steps — zero hallucination.
   - `cgx_render_snapshot`: Invokes `cgx-glfw -bg` (or `cgx-top report --html`) to produce a high-DPI Cubehelix contour PNG, animated mode-shape GIF, or interactive HTML report.

---

## Colloquium Live Demo Synergy

Combining the three repositories creates a cohesive two-act story for Dr. Dhondt's CalculiX Colloquium:

1. **Act 1 (Solver Performance)**: Show the 7-Blade Turbocharger Compressor Rotor (`lavffcyc.inp`, 283,024 DOFs, centrifugal preload + prestressed cyclic symmetry `*FREQUENCY`) achieving **6.81x speedup with Apple Accelerate** and **6.44x speedup with MUMPS 5.x** over SPOOLES 2.2.
2. **Act 2 (`cgx-top` + `cgx-glfw` + MCP Live Finale)**:
   - Show `cgx-top --watch` tracking live `.sta`/`.cvg` convergence and `.frd` invariants in the terminal.
   - Generate a single-file interactive 3D HTML report (`cgx-top report lavffcyc.frd -o rotor.html`) and spin the vibrating rotor mode shape right inside the presentation slide.
   - Demonstrate an AI agent querying exact blade-root von Mises stress and modal frequencies via `calculix-mcp` (`cgx-core`) and rendering a Cubehelix snapshot via `cgx-glfw`.
