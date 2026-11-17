# cgx-top 📊

[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> The terminal companion to **CalculiX GraphiX (CGX)**. A fast, zero-dependency Terminal User Interface (TUI) for inspecting FEA results.

Designed for engineers running CalculiX simulations on remote clusters, headless compute nodes, or local environments over SSH. Inspect eigenvalues, mode shapes, load steps, and peak stresses in milliseconds—without slow X11 forwarding or heavy GUIs.

---

## Why cgx-top?

When working remotely over SSH, opening a full OpenGL GUI just to verify whether a simulation converged, what the natural frequencies were, or what the peak von Mises stress was at a critical fillet is slow and clunky.

`cgx-top` brings the essential inspection features of CGX straight to your terminal:

```bash
# Launch interactive terminal inspector
cgx-top results/beam_modal.frd

# Instant command-line summary
cgx-top --summary results/bridge.frd

# Query peak von Mises stress history for a specific set across all time steps
cgx-top results/turbine.frd --set END_TIP_BLADE --mises
```

---

## Key Features

- ⚡ **Instant Startup**: Memory-mapped (`memmap2`) zero-copy parsing—opens multi-gigabyte `.frd` files in milliseconds.
- 📋 **Step & Mode Navigator**: Quick overview of natural frequencies, time increments, and load steps.
- 🎯 **Set-Based Invariant Queries**: Query peak stresses (von Mises, Max Principal, Tresca) or temperatures across any node/element set (e.g. `END_TIP_BLADE`) across all time increments.
- 🔍 **Dataset Extrema**: Instant min/max identification (values and node IDs) for displacements, stresses, temperatures, and strains.
- 📈 **ASCII Trend Sparklines**: Quick in-terminal visual curves for stress histories and frequency spectrums.
- 📁 **Dual ASCII & Binary FRD Support**: Seamlessly reads both text-mode and binary CalculiX results files.
- 🌐 **SSH & Terminal Native**: Runs anywhere your shell runs. Zero graphic or display server dependencies.
- 🔭 **Roadmap (Phase 3)**: 3D ASCII/Braille deformed wireframe viewport for instant geometry sanity checks over SSH.

---

## Interactive TUI Layout

```text
┌─ CalculiX FRD Inspector ─── beam_modal.frd ──────────────────────────────────────────────┐
│ Nodes: 12,450  │  Elements: 8,200 (C3D20: 8,200)  │  Steps: 20  │  File Size: 45.2 MB   │
├────────────────────────────────┬─────────────────────────────────────────────────────────┤
│ [Steps / Modes]                │ [Dataset Details: Mode 3 (1,245.8 Hz)]                  │
│                                │                                                         │
│ > Mode 1:    124.5 Hz          │  Field: DISP (Nodal Displacement)                       │
│   Mode 2:    482.1 Hz          │  Components: D1, D2, D3, ALL                            │
│   Mode 3:  1,245.8 Hz  ◄       │  Extrema:                                               │
│   Mode 4:  1,890.3 Hz          │    D1:   min: -0.0124 mm    │ max:  0.0124 mm           │
│   Mode 5:  2,410.0 Hz          │    D2:   min: -0.0002 mm    │ max:  0.0001 mm           │
│   Mode 6:  3,105.2 Hz          │    D3:   min: -0.0541 mm    │ max:  0.0541 mm           │
│   Mode 7:  4,210.9 Hz          │    ALL:  max:  0.0555 mm (Node #1042)                   │
│   Mode 8:  5,640.1 Hz          │                                                         │
│                                │  Field: STRESS (Cauchy Stress)                          │
│                                │    von Mises: max: 184.2 MPa (Node #312)                │
├────────────────────────────────┴─────────────────────────────────────────────────────────┤
│ [Frequency Spectrum / Trend]                                                             │
│   5.6k ┤                                     ╭─                                          │
│   3.0k ┤                            ╭───────╯                                            │
│   1.2k ┤               ╭───────────╯                                                     │
│      0 ┴───┴───┴───┴───┴───┴───┴───┴───┴───┴───                                          │
│            1   2   3   4   5   6   7   8   9   10  (Mode Index)                          │
├──────────────────────────────────────────────────────────────────────────────────────────┤
│ [q] Quit  │  [j/k] Navigate Modes  │  [s] Filter Set  │  [Tab] Switch Pane  │  [/] Search│
└──────────────────────────────────────────────────────────────────────────────────────────┘
```

---

## Keybindings

| Key | Action |
|:---|:---|
| `j` / `↓` | Next Step / Mode |
| `k` / `↑` | Previous Step / Mode |
| `s` | Open Set Filter selector (`[ALL]`, `END_TIP_BLADE`, etc.) |
| `Tab` | Switch between Steps and Field details |
| `g` / `G` | Jump to first / last step |
| `/` | Filter fields or search node IDs |
| `e` | Export current view to CSV |
| `q` / `Esc` | Quit |

---

## MCP server (calculix-mcp)

The workspace also contains `calculix-mcp`, a Model Context Protocol server that lets an MCP client (such as Antigravity, Claude Code, or Claude Desktop) inspect CalculiX decks, check a mesh and select faces by geometry (saved as `*SURFACE` or `*NSET` include files), run the solver in the background, diagnose convergence from `.sta`/`.cvg` files, and query `.frd` results (including binary files and a 409 MB cyclic-symmetry result) with exact values and node ids.

```bash
cargo build --release -p calculix-mcp
claude mcp add calculix -- /abs/path/to/calculix-mcp --root /abs/case/dir --ccx ccx=/abs/path/to/CalculiX
```

Pass `--ccx name=path` multiple times to register several MultiSolver builds (e.g. `accelerate`, `spooles`, `mumps`); the first one is the default. Every build must support `-j` JSON telemetry (the CalculiX MultiSolver fork); stock `ccx` is not supported.

See [`crates/calculix-mcp/README.md`](crates/calculix-mcp/README.md) for the tool list, worked examples, safety model and limitations.

---

## Architecture

`cgx-top` is built in Rust as a modular Cargo workspace:

```text
cgx-top/
├── crates/
│   ├── cgx-core/            # Pure FEA data model, memory-mapped parser, invariants (von Mises)
│   └── cgx-top/             # Interactive TUI application (ratatui + crossterm)
```

- **`cgx-core`**: Fast, zero-copy parser for ASCII and binary `.frd` files. Uses `rayon` for multi-threaded invariant calculations and memory-mapped virtual I/O (`memmap2`).
- **`cgx-top`**: Terminal user interface following the Model-View-Update pattern.

---

## Building from Source

### Prerequisites
- [Rust & Cargo](https://rustup.rs/) (edition 2021 or later)

```bash
git clone https://github.com/carlomontec/cgx-top.git
cd cgx-top
cargo build --release
```

The compiled binary will be placed at `target/release/cgx-top`.

---

## Roadmap

- [x] Workspace scaffolding & core FEA architecture
- [ ] Phase 1: High-performance memory-mapped ASCII & binary `.frd` parser
- [ ] Phase 1: Set parsing and invariant calculation engine (von Mises, Principal Stresses)
- [ ] Phase 2: Full interactive Ratatui TUI dashboard with live sparklines
- [ ] Phase 3: ASCII/Braille 3D deformed wireframe viewport for remote geometry sanity checks

---

## License

Distributed under the MIT License. See `LICENSE` for more information.
