# The 17 tools of calculix-mcp

These tools let an AI assistant read, check, edit and solve CalculiX models for you. You talk to the assistant in plain language and it picks the tools; you never call them by hand. Paths are relative to `--root`, the folder that holds your models. The examples below were run for real; numbers are shortened and are in deck units.

## Overview

| Group | Tool | In one line |
|---|---|---|
| Decks | `inp_inspect` | Summary of a deck and its includes: flags, steps, sets, materials |
| | `inp_cards` | Show specific keyword cards with file and line |
| | `inp_consolidate` | Flatten a multi-file deck into one file |
| | `inp_edit` | Change cards, written to a new deck with a diff |
| Geometry and meshing | `cgx_help` | CalculiX GraphiX command reference with tested examples |
| | `cgx_run_batch` | Run a GraphiX script headlessly to build and mesh geometry |
| Mesh checks and selections | `mesh_check` | Parts, free nodes, inverted elements, volume, mass, bad sets |
| | `mesh_select` | Pick faces by geometry; save as a surface or node set |
| | `mesh_contact_pairs` | Find faces that face each other; save contact surfaces |
| Solving | `ccx_solve` | Start a solve in the background |
| | `ccx_job_status` | Follow a job: state, progress, verdict, result files |
| | `ccx_job_cancel` | Stop a job and everything it started |
| | `ccx_job_list` | List recent and past jobs |
| | `ccx_diagnose` | Explain why a run converges well or badly |
| Results | `frd_summary` | Steps, fields and whole-model peaks of a result file |
| | `frd_sets` | List the sets you can query |
| | `frd_query_set` | Exact stress or displacement values on one set |

## Decks

### `inp_inspect`
**What it's for.** First look at any deck. It follows every `*INCLUDE` and reports flags, a keyword count, the step plan, missing files, sets and materials.

**Try:** "What does `ccx/CalculiX-CrunchiX-MultiSolver/benchmarks/compressor_rotor/lavffcyc.inp` do? Any missing includes?"

**You get:** 74,017 lines in 5 files, 4 includes and none missing. Flags: cyclic symmetry and perturbation. Two steps: a static step, then a frequency step. One material, C355.

**Good to know:**
- Never edits the deck.
- Large lists (includes, sets) are capped; use `limit` or `set_filter` to see more.

### `inp_cards`
**What it's for.** Read just the cards you care about, with file, line and first data lines, without loading the deck.

**Try:** "Show me the cyclic symmetry card of the rotor deck."

**You get:** One card, `*CYCLIC SYMMETRY MODEL, N=7, NGRAPH=7`, at `lavffcyc.inp:21`, model level, with its data line `0.,0.,0., 1.,0.,0.`.

**Good to know:**
- If nothing matches, the answer lists the most frequent keywords in the deck.

### `inp_consolidate`
**What it's for.** Flatten a deck that uses `*INCLUDE` into a single `.inp` file.

**Try:** "Flatten `cube.inp` into one file."

**You get:** `cube_consolidated.inp`, 37 lines, 1 file inlined (`mesh/cube.msh`).

**Good to know:**
- Writes a new file beside the deck; refuses to replace an existing one unless you ask.
- Returns statistics only, not the deck text.

### `inp_edit`
**What it's for.** Change a deck by naming cards: set or remove a parameter, replace data lines, insert or delete a card, append a step. You get a diff and a re-check.

**Try:** "In `beam_modal.inp`, ask for 10 modes instead of 5."

**You get:** `beam_modal_edit.inp` is written. The diff is one line: `5` becomes `10` under `*FREQUENCY` (line 356). The re-check says the deck still scans fine.

**Good to know:**
- The original deck and its includes are never modified. Say "dry run" to see the diff without writing.
- If several cards match, the assistant must say which one (by index); an unknown card returns the list of valid keywords.

## Geometry and meshing

`cgx_help` works without `--cgx`; `cgx_run_batch` needs it.

### `cgx_help`
**What it's for.** The CalculiX GraphiX command reference, split into topics (points, lines, surfaces, bodies, sweeps, meshing, sets, `send`, and so on). Every example was run with cgx.

**Try:** "How do I extrude a surface into a solid mesh with GraphiX?"

**You get:** The "Sweeps and extrusions" section with syntax and two scripts, for example a 100 x 20 x 10 block that produces 321 nodes and 40 C3D20 elements.

**Good to know:**
- Ask for a topic (`swep`, `send`, `sets`) to get only that section.

### `cgx_run_batch`
**What it's for.** Run a GraphiX script without a window to build geometry and mesh it. It returns the files produced and a summary of each mesh.

**Try:** "Run the block example from the sweep help: a 100 x 20 x 10 block with 20-node bricks."

**You get:** `all.msh` with 321 nodes and 40 C3D20 elements, box (0,0,0) to (100,20,10), sets `EALL` (40) and `NALL` (321), written under `.calculix-mcp/cgx/<run>/`.

**Good to know:**
- cgx exits with success even when a command fails, so the result carries an `errors` list; check counts before using the mesh.
- The script is screened for shell and path-escaping commands. That is a mitigation, not a sandbox.

## Mesh checks and selections

### `mesh_check`
**What it's for.** Check the mesh before a long solve: separate parts, free nodes, inverted elements, worst aspect ratio, volume, mass and centre of gravity per material, undefined or empty sets.

**Try:** "Check the mesh of `test/beam_modal.inp` before I solve it."

**You get:** Verdict OK. 261 nodes, 32 C3D20 elements, 1 part, no free nodes, none inverted, worst aspect ratio 2.0, volume 8.0, mass 6.24e-08.

**Good to know:**
- Volume and mass use corner nodes only, so curved quadratic edges come out slightly short.
- Mass is empty (null) when a material has no `*DENSITY`.

### `mesh_select`
**What it's for.** Select faces by geometry (direction, plane, box, cylinder, near a point, connected patch) and optionally save them as a `*SURFACE` or `*NSET` include file. You get count, area and bounding box to check the selection.

**Try:** "Save the faces of `beam_modal.inp` that point along +z as a surface called TOPFACE."

**You get:** 4 faces, area 1.0, 21 nodes, all at z = 8. Saved as `TOPFACE.sur`; the line to add is `*INCLUDE, INPUT=TOPFACE.sur`.

**Good to know:**
- Never edits the deck: you add the include line yourself, or ask `inp_edit`.
- A saved surface is not an `.frd` set, so `frd_query_set` cannot query it; save an `*NSET` for that.

### `mesh_contact_pairs`
**What it's for.** Find faces of different parts that face each other within a gap, with gaps, areas and a suggested slave and master side. It can save both surfaces and a contact-card template.

**Try:** "Find contact candidates in `contact2.inp` up to a gap of 0.6 and save them with prefix c2."

**You get:** One pair, PART_1 and PART_2: 16 faces each side, area 8.0, gap 0.5 everywhere. Suggested slave PART_1, master PART_2. Writes `c2_PART_1_PART_2_slave.sur` and `_master.sur`.

**Good to know:**
- The default gap is small. With defaults on this deck nothing is found, and the answer says the closest faces are 0.5 apart and to retry with 0.6.
- The card template is text for you to review; it is not written into the deck. The slave suggestion is a suggestion.

## Solving

Needs `--ccx`. Solves run on a private copy of the deck in `<root>/.calculix-mcp/runs/<job_id>/`.

### `ccx_solve`
**What it's for.** Start a solve in the background and get a job id at once.

**Try:** "Solve `beam_modal_edit.inp` with mumps."

**You get:** A job id like `j1795254402-c6ac`, solver mumps, 4 threads, run directory `.calculix-mcp/runs/j1795254402-c6ac`, JSON telemetry on.

**Good to know:**
- The original folder is never written to.
- Only one job runs at a time; others wait with state QUEUED. Default time limit is 3600 s.

### `ccx_job_status`
**What it's for.** State, progress, memory, convergence verdict and result files of a job. It can wait for a change instead of making you poll.

**Try:** "Tell me when that job is done and how it went."

**You get:** SUCCEEDED, verdict LINEAR, 10 modes, first frequency 13102 Hz, result files (`.frd`, `.dat`, `.sta`, `.cvg`, `.json`). While a nonlinear job runs it shows step, increment and memory, for example step 1, increment 1, 44 MB.

**Good to know:**
- Waits up to 50 s per call.
- With stock CalculiX there is no JSON telemetry; progress comes from `.sta` and `.cvg` only.

### `ccx_job_cancel`
**What it's for.** Stop a queued or running job and every process it started.

**Try:** "Cancel that job, it is going nowhere."

**You get:** `RUNNING` became `CANCELLED`.

**Good to know:**
- Safe on a finished job: it is returned unchanged (`SUCCEEDED` stays `SUCCEEDED`).

### `ccx_job_list`
**What it's for.** Recent jobs, newest first, with state, jobname, label, elapsed time and run-directory size. It includes jobs from earlier server runs.

**Try:** "What jobs have I run?"

**You get:** A table such as: `bad1`, CANCELLED, 3 s, 0.2 MB; `bad1_fix`, SUCCEEDED, 2 s, 0.4 MB; `bad1`, FAILED, 13 s, 0.5 MB.

**Good to know:**
- Filter by state (for example FAILED) and page with `limit` and `offset`.

### `ccx_diagnose`
**What it's for.** Explain convergence from `.sta`, `.cvg` and `.json`: linear, smooth, slow, contact chattering, cutback loop or divergence, with evidence, the last increments and next steps.

**Try:** "Why did that contact run fail?"

**You get:** Verdict DIVERGED. Findings: residual not falling (D1, D2), contact chatter (C1), five cutbacks then a shrinking increment (B1, B3). Solver message: too many cutbacks. Advice: check contact stiffness and use a smaller first increment.

**Good to know:**
- Thresholds are heuristics. Treat warnings as leads, not proof.
- Works on a job id, or on any job folder by path.

## Results

### `frd_summary`
**What it's for.** What is in a `.frd` file: steps, frequencies or times, fields, and whole-model peak displacement and von Mises stress with node ids. Call it before querying.

**Try:** "Summarise the rotor result file."

**You get:** 342,139 nodes, 75,712 elements, 4 steps, 409 MB. Static step: peak displacement 0.373 (node 146773), peak von Mises 489.5 (node 152204). Modal steps at 8310 and 11052, nodal diameter 5.

**Good to know:**
- Large files are read lazily; this one takes well under a second.
- Modal amplitudes are normalised, not physical. For cyclic-symmetry files with complex data, displacement is an upper bound.

### `frd_sets`
**What it's for.** List the node and element sets you can pass to `frd_query_set`, with sizes. Add the deck to include its sets.

**Try:** "Which sets does the rotor result have?"

**You get:** 5 sets: `NCYC1` and `NCYC2` (1,685 nodes each) plus the implicit `+all`, `+C3D20`, `+mat1`.

**Good to know:**
- Filter by name part with `filter`. Only node and element sets count; surfaces do not.

### `frd_query_set`
**What it's for.** Exact stress invariants (von Mises, principal, Tresca, hydrostatic) or displacement extrema on one set, per step, with the responsible node ids. `top_n` lists the most loaded nodes with coordinates.

**Try:** "Peak stress on NCYC1 in each step, and the three worst nodes."

**You get:** Step 1: von Mises peak 370.3 at node 12241. Step 2 (8310): 173,653 at node 12241. Worst node over all steps: 12241 at (0, 1.91, 2.93).

**Good to know:**
- The assistant should call this, never estimate.
- Modal stresses are normalised mode-shape values, not physical stresses.

## Workflows

**(a) Check an unknown deck before solving**
1. "What is in `model.inp`?" Answered by `inp_inspect`.
2. "Show the contact and boundary cards." Answered by `inp_cards`.
3. "Is the mesh sound?" Answered by `mesh_check`.
4. "Solve it and tell me when it is done." Answered by `ccx_solve` and `ccx_job_status`.

**(b) Mesh a part, add a load surface and solve it**
1. "Build a 100 x 20 x 10 block with 20-node bricks." `cgx_help` for syntax, `cgx_run_batch` to mesh it.
2. Write a small deck that includes `all.msh` with a material and section, then "Check it." `mesh_check`.
3. "Save the face at x = 0 as node set FIX and the face at x = 100 as surface LOAD." `mesh_select` twice.
4. Add the includes, the boundary and the `*DLOAD`, then "Solve it." `ccx_solve`, then `ccx_job_status` with a wait.
5. "Peak stress and displacement?" `frd_summary`, then `frd_query_set` on `FIX`.

**(c) Find contact between two parts and write the cards**
1. "Check the mesh and tell me how many parts there are." `mesh_check`.
2. "Find contact candidates up to gap 0.6 and save them as c2." `mesh_contact_pairs`.
3. Review the card template it returns; add the two includes and the `*CONTACT PAIR` card to the deck. `inp_edit` can insert them.
4. "Solve it." `ccx_solve`.

**(d) A run that converges badly**
1. "Why did job X fail?" `ccx_diagnose` (and `ccx_job_status` with `tail_lines` for solver output).
2. "Show the contact behaviour and the second step's static card." `inp_cards`.
3. "Use a softer contact stiffness and a smaller first increment." `inp_edit`, which writes a new deck and a diff.
4. "Solve the edited deck." `ccx_solve`, then `ccx_job_status`; `ccx_diagnose` confirms the verdict.

## Setup reminder

- Install and connect: see "Install and connect" in the [README](../README.md#install-and-connect); all flags are in the [server README](../crates/calculix-mcp/README.md#run-and-flags).
- `--ccx NAME=PATH` enables solving (`ccx_solve`, the job tools, `ccx_diagnose` on live jobs); `--cgx PATH` enables geometry and meshing (`cgx_run_batch`).
- Everything else, including `cgx_help`, `.inp` tools, mesh tools and `frd_*`, needs only `--root`.
