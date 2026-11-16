# CGX command channel — feasibility study (Task 7)

Date: 2026-11-16. Read-only study; nothing in `CalculiX-CGX-New3D` was edited or built.
Source paths below are relative to `/Users/carlo/code/CalculiX/cgx/CalculiX-CGX-New3D/cgx_2.23/src/`.
Binary tested: `/Users/carlo/code/CalculiX/cgx/bin/cgx_glfw` (built 2026-10-22; the CGX working tree has a newer, staged-but-uncommitted `cgx.c` from 2026-11-15, so source and binary can differ in detail).
Scratch files and all PNGs: `/Users/carlo/code/CalculiX/cgx-top/target/cgx-study/`.

> **Reviewer note (2026-11-16, after viewing the images in `target/cgx-study/`).** The "correct" snapshot
> `live3b.png` does show the legend, the colour bar and the beam, but the beam is rendered almost black:
> the contour colours are barely distinguishable from the background. Another capture, `live_fv2.png`,
> shows the legend and colour bar with an empty viewport. So no experiment produced a snapshot that is
> usable as-is. Besides the timing problems described below, the captured frame has a shading or
> lighting problem that must be understood before the `hcpy` fix can be called done.

## Summary

- **A live `cgx_glfw` already accepts commands from another process**: a stdin listener thread (`cgx_glut_glfw.c:244-260`, started in `glutCreateWindow`, `:347-349`) queues each stdin line; the main loop executes the queue on the GL thread (`:1963`, `:228-242`). Piping commands into `cgx_glfw beam_modal.frd` worked and produced a contour PNG.
- **It is not usable by a program as is**, for four reasons, all measured: (1) commands sent before startup finishes run before the model is loaded (`ERROR: no Datasets available`); (2) `hcpy` reads the back buffer at an arbitrary point in the frame, so a snapshot can be stale or empty; (3) stdout is block-buffered when piped, so a client sees nothing until CGX exits; (4) no command returns a status.
- **`cgx_glfw -bg` cannot render**: it exits with 0, prints the extrema, and writes no image, silently. `-bg` runs the `.fbd` and calls `exit(0)` before any GLFW/GL initialisation (`cgx.c:7839-7851`), and every graphics routine starts with `if(!inpformat) return;` (`createHardcopy`, `cgx.c:883`).
- **`cgx_glfw -b` (windowed batch) does write a PNG, but it shows no model** (back-buffer problem, see Experiment) and a window opens.
- **Recommendation:** run CGX as a **child process of the MCP server, driven through the existing stdin channel**, after about 100 lines of opt-in changes in CGX. The changes: a deferred `hcpy`, a ready gate, a flushed acknowledgement line, and a `-safe` flag. A socket or named pipe is only needed to attach to a CGX the user started; leave it for later.

## 1. How commands flow today

| Path | Entry | Executes via | Notes |
|---|---|---|---|
| Keyboard in the 3D window (legacy line parser) | `Keyboard()` `cgx.c:5703` | `commandoInterpreter()` `cgx.c:5752-5755` | `q*` interactive commands are dispatched before it (`:5730-5751`). |
| GLFW command bar | `submit_command_bar()` `cgx_glut_glfw.c:706` | `cgx_execute_command_string()` `:727` | Multi-line clipboard paste runs line by line (`:822`). |
| **stdin (any process)** | `stdin_listener_thread()` `cgx_glut_glfw.c:244` → `queue_command()` `:216` | `process_queued_commands()` `:228` → `cgx_execute_command_string()` `:236` | Always on, in every windowed mode. |
| `-b file.fbd` | `idleFunction()` `cgx.c:6843` (third loop pass) | `readfbd()` `setFunktions.c:19211` → `commandoInterpreter()` `:19298` | Runs the whole file inside one idle call: no frame is rendered between lines. |
| `-bg file.fbd` | `main()` `cgx.c:7839-7851` | `readfbd(datin,0); exit(0);` | Before `glutInit` (`cgx.c:8029`), so there is no window and no GL context. `inpformat=0` turns every drawing routine into a no-op. |
| `read x.fbd` (any path above) | `pre_read()` `setFunktions.c:19614` | nested `readfbd()` | Nested files can `STOP` and resume (`setFunktions.c:18793`). |
| `~/.cgx` init file | `idleFunction()` `cgx.c:6812-6819` | `readfbd()` | Executed at every windowed start. |

**The single function that executes one line** is `cgx_execute_command_string(const char*)` (`cgx.c:5836-5907`). It trims the line, echoes `cgx> <line>` to stdout (`:5852`), appends to the history, dispatches the `q*` commands, and otherwise calls `commandoInterpreter(prognam, &k_ptr, pos, 0, 0, 0, &gtolFlag)` (`:5900`). It then calls `glutPostRedisplay()`. It returns `void` and discards the interpreter's result (`(void)res`, `:5902`).

`commandoInterpreter()` (`setFunktions.c:16361-19180`) is a single `if/else if` chain on the upper-cased keyword. Its return value says almost nothing: `returnFlag` is 1 except for an unknown keyword (0, `:19157`), `READ` (whatever `pre_read` returns, `:17851`) and `STOP` (-2). Every other error is only a `printf` (for example `ERROR: no Datasets available`, ` set:%s does not exist`).

Relevant commands (keyword → line in `setFunktions.c`):
- `DS` (17021): selects a dataset and entity (`ds 1 e 4`).
- `PLOT` / `PLUS` (17821 / 17828): resets the scale for `ev`/`fv`, then calls `plot()`.
- `FRAME` (17232): calls `frame()` (`cgx.c:1312`).
- `READ` (17849): `pre_read()`.
- `HCPY` (17364): calls `createHardcopy(5, name)` **directly**. `createHardcopy()` (`cgx.c:879-938`) goes to `cgx_save_png()` (`cgx_capture.c:62`), which does `glReadPixels` of the default read buffer with no prior render. The menu path is different: it sets `hcpyFlag=5` (`cgx.c:1551`), and the capture is deferred until after a draw (`cgx.c:6151-6154`).
- `SEND` (17983): `pre_write`, which writes files.
- `SYS` (18821).
- `QUIT` (17848): `exit(0)`.

## 2. Where a non-blocking poll could go, and the threading concerns

**Location.** The hook already exists. In `glutMainLoop()` (`cgx_glut_glfw.c:1937`), each iteration does `glfwPollEvents()` or `glfwWaitEventsTimeout(0.05)` (`:1955-1962`), then `process_queued_commands()` (`:1963`), then the idle function, then rendering. Any new source should feed the same queue:
- A socket or named pipe should be read on its own thread that calls `queue_command()` and `glfwPostEmptyEvent()`. GLFW documents `glfwPostEmptyEvent` as callable from any thread. This is the same design as the stdin thread.
- The alternative is a non-blocking `poll`/`select` on the socket at `:1963`. That is simple on POSIX, but Windows needs `WSAPoll` and a different handle type for named pipes. A reader thread keeps the platform-specific code in one place.

**Thread safety today.**
- Commands execute on the main thread, where the GL context is current, which is correct. The listener thread only touches the ring buffer under `g_cmd_queue_mutex`.
- There is one benign data race: the listener writes `g_last_cmd_echo` without the lock (`:253-258`), and the renderer reads it.
- The queue holds 64 entries of 511 characters. **A full queue drops the line silently** (`:219-223`). **`fgets` into 512 bytes splits a longer line, and the tail is executed as a separate command** (`:249`). The channel must reject lines that are too long instead of splitting them.

**Re-entrancy and ordering.**
- **Startup race (observed):** `process_queued_commands()` runs before `g_idle_func()`, and `idleFunction` loads the model only on its third call (`cgx.c:6827`, `:6964-6979`). Commands piped immediately ran before the `.frd` was read; see live4 below. Fix: process the queue only when `g_idle_func == NULL`, and print a ready line at the end of `idleFunction` (`cgx.c:7008`).
- Each command runs to completion before the next one starts, so there is no interleaving. A long `read` freezes the window, which is acceptable.
- `commandoInterpreter` keeps static state (`ifFlag`, `whileFlag`, `seq`, `picnr`, `setFunktions.c:16385`). `WHILE`/`IF` loop by re-reading the open `.fbd` handle (`handle1`), and the channel passes `NULL`. Multi-line control flow therefore must be sent as a temporary `.fbd` plus `read tmp.fbd`, not line by line.
- `sys` without `ALLOW_SYS` calls `scanf` on stdin (`setFunktions.c:18829-18836`) and competes with the listener thread for the same stdin. In channel mode it must be refused instead.
- `quit`/`exit` call `exit(0)` from inside the interpreter. `exit` and `save` also rename the input file with `system("mv …")` (`setFunktions.c:17114-17139`, `17929-17953`).

**Acknowledgement (CGX has no return values).** Proposed protocol, opt-in (for example `-pipe`), so interactive users see no change:
1. `setvbuf(stdout, NULL, _IONBF, 0)`, or `fflush(stdout)` after each command. Measured: when piped, CGX output arrives only at exit, because there is no `setvbuf` and only two `fflush` calls (`cgx.c:5535`, `:5567`). `_IOLBF` is not enough because Windows CRT treats it as full buffering.
2. Have `cgx_execute_command_string` return the interpreter's `int`. Around each queued command, print `@cgx begin <seq>` and `@cgx end <seq> rc=<n> warn=<count>` on their own lines. `warn` counts `ERROR`/`WARNING` lines seen in between; a simple way is a counter incremented by a small wrapper, or the client counts them itself between the markers. Each command's output is the text between its two markers.
3. Print `@cgx ready` after `idleFunction` finishes. Report `hcpy` results as `@cgx file <path> <w>x<h>` after the deferred capture.

**Zero-change workaround (measured, works today except for the buffering):** send a comment line such as `# MARK-7` after a batch. `commandoInterpreter` echoes `#`/`!` lines (`setFunktions.c:19151-19154`). Because commands run sequentially, seeing ` # MARK-7` means everything before it has finished. It is only useful once stdout is flushed, which today requires a pty on the client side.

## 3. Headless snapshot experiment

All runs were made in `target/cgx-study/` with `beam_modal.frd` copied there. `~/.cgx` exists and contains `ALLOW_SYS`.

Common `.fbd` (`snap_bg.fbd`; `snap_b.fbd` is identical with `snap_b`):
```
read beam_modal.frd
ds 1 e 4
plot fv all
frame
hcpy png snap_bg
quit
```

| # | Command | Exit | Result |
|---|---|---|---|
| 1 | `timeout 60 cgx_glfw -bg snap_bg.fbd > bg.out 2> bg.err` | 0 | **No PNG, no error.** 139 stdout lines, stderr empty. The `.frd` loads, and `plot` prints `DISP D4 max 1.514370e-06 at node 70 / min -1.534470e-06 at node 4`. `hcpy` returns silently (`cgx.c:883`). No window. |
| 2 | `timeout 60 cgx_glfw -b snap_b.fbd < /dev/null` | 0 | Window opens briefly. `Screenshot saved: snap_b.png (1568x1344 px)`, 87 KB, but the image is only the dark background and the ruler. **No model, no legend**: the `.fbd` runs inside one idle call, so nothing was rendered before `glReadPixels`. |
| 3 | `( sleep 4; echo "ds 1 e 4"; echo "plot fv all"; sleep 2; echo "hcpy png snap_live"; sleep 2; echo quit ) \| cgx_glfw beam_modal.frd` | 0 | Visible window. The PNG (105 KB) shows the window and the model outline, but the frame is from **before** `plot fv`: no colours, no legend. |
| 4 | Same idea, two `hcpy` 1 s apart (`live3.png`, `live3b.png`) | 0 | The first PNG is again stale (105 KB). **The second (`live3b.png`, 192 KB) is correct: legend `1/1:DISP Freq 3.97161E-01 Entity:D4`, cubehelix colour bar, contoured beam.** The contour itself renders very dark at this view. This confirms that capture timing, not rendering, is the problem. |
| 5 | Commands piped with no delay, `# MARK-n` markers, an unknown command | 0 | `ds 1 e 4` → `ERROR: no Datasets available` (it ran before the model loaded). `bogus_cmd` → `[ERROR] Unknown command`. Markers are echoed in order. stderr: `UNSUPPORTED (log once): … GLD_TEXTURE_INDEX_2D is unloadable` (macOS GL driver, after `plot fv` with no data). |
| 6 | Markers 3 s apart, stdout piped and timestamped per line | 0 | All lines arrived within 1 s, at exit: **stdout is block-buffered when piped.** |

So a correct contour PNG can be produced today only from a visible live session, and only by luck of timing. **`-bg` cannot produce images by design.** `-bg` can still produce data, for example `send … frd`, or values printed by `prnt`.

The fix that makes runs 2–4 deterministic: `HCPY` should set a pending request (file name), not capture immediately. `glutMainLoop` should then capture after the full render pass and before `glfwSwapBuffers` (`cgx_glut_glfw.c:2071`), with `glReadBuffer(GL_BACK)`. The menu path already defers in a similar way (`cgx.c:6151`). About 30–40 lines. In `-b` mode, `readfbd` would have to yield to the main loop at each `hcpy`. Alternatively, `-b` could feed the `.fbd` lines through the queue instead of `readfbd` in idle; that is a larger change (about 60 lines) and must keep `WHILE`/`STOP` working.

**True headless rendering (no visible window)** under the GLFW-only rule:
- Create the window with `glfwWindowHint(GLFW_VISIBLE, GLFW_FALSE)` and render into an FBO. The default framebuffer of a hidden window is not reliable for `glReadPixels`. FBO entry points are not in `cgx_gl_loader.h` today; macOS has them natively, and Windows/Linux need loader entries.
- A display server is still required on Linux (X11/Wayland, or Xvfb on a server).
- GLFW 3.4+ (3.5.1 is installed) has a null platform that can use OSMesa for a context without a display. That is not verified here.
- About 100–150 lines, behind a new flag (for example `-offscreen WxH`), with `-bg` left as is.

## 4. Security

Commands that execute programs or write files:

| Command / path | Effect | Where |
|---|---|---|
| `sys <cmd>` | `system(<cmd>)`. Prompts on stdin unless `ALLOW_SYS` is in `~/.cgx` (**it is, on this machine**) or `ALLOW_SYS_FLAG`. Answering `e` writes `ALLOW_SYS` into `~/.cgx` (`addCommandToInitFile`, `cgx.c:7399`). | `setFunktions.c:18821-18838`, flag read at `cgx.c:7773-7786` |
| `exit`, `save` | `system("mv <datin> <base>.fbb")`, with `datin` **unquoted** on Unix (shell injection through a crafted input file name), then write `all.fbd`. | `setFunktions.c:17114-17139`, `17929-17953` |
| `send …` | Writes mesh/result/set files of many formats, by name, into the cwd. Some writers shell out to `cat`/`rm` with set names (`setFunktions.c:25018-25027`). | `pre_write` `setFunktions.c:22311` |
| `hcpy <name>` | Writes `<name>.png` to any path, including `../`. `hcpy clean` deletes `hcpy_*.png` in the cwd. | `setFunktions.c:17364`, `cgx_capture.c:109` |
| `movi` | `popen(ffmpeg …)`. | `cgx_capture.c:264` |
| `graph` | `system("gnuplot …")`, then `system(psviewer …)`. | `graph.c:202`, `:224` |
| `read x.fbd` | Runs any command file, including `sys`. `read` of a step file shells out (`readstep.c:1475-1636`). | `pre_read` |
| Help menu | `system(browser …)`. | `cgx.c:1705-1715` |
| `~/.cgx` | Any commands, run at every windowed start. | `cgx.c:6812` |

**How a channel can restrict them.** The MCP side should never pass free text through. Its tools should generate commands from a whitelist: `read <confined path>`, `ds`, `plot`/`plus`, `view`, `rot`, `zoom`, `frame`, `cmap`, `hcpy <server-chosen name>`, `prnt`. The server should reject `;`, newlines and `..` in arguments, and enforce a maximum length of 500 characters.

Defence in depth inside CGX: a `-safe` flag (about 40 lines) that
1. forces `allowSysFlag=0` and refuses `sys` without prompting;
2. refuses `exit`, `save`, `movi`, `graph`, `subm`, `sys`, and `read` of `.fbd`/`.step` from the channel;
3. skips `~/.cgx`;
4. makes `hcpy` and `send` reject absolute paths and `..`.

With a socket, add a per-session random token and create the socket or pipe in a user-private directory (`0700`). Never listen on TCP.

## 5. Portability (macOS / Linux / Windows, GLFW only)

- **stdin channel:** portable as is. It uses `pthread` (`cgx_glut_glfw.c:19`): native on macOS and Linux, winpthreads on MinGW. The Makefile links `-lpthread` for MINGW/MSYS (`Makefile.glfw:40-45`). MSVC would need a C11 `threads.h` or a Win32 shim; the build does not target MSVC today. `main` already accounts for the blocked stdin reader on Windows with `TerminateProcess` (`cgx.c:8224-8234`).
- **Child process with pipes:** works the same on all three systems from Rust (`std::process::Command` with piped stdin and stdout). This is the main portability argument for design A as recommended.
- **Socket / named pipe:** Unix domain sockets on macOS and Linux. On Windows, either AF_UNIX (Windows 10 1803+, Winsock, `afunix.h`) or `CreateNamedPipe`. Both need `#ifdef _WIN32` code outside GLFW: allowed by the rule, which concerns windowing/input, but it is new platform-specific code in core CGX. Keep it in one new file (`cgx_channel.c`).
- **stdout flushing:** use `_IONBF` or explicit `fflush`, because `_IOLBF` is full buffering on Windows.
- **Offscreen rendering:** hidden GLFW window plus FBO works on macOS, Windows and Linux with a display. A headless Linux server needs Xvfb or GLFW's null platform with OSMesa.

## 6. Designs compared

### (A) Command channel into a live CGX (recommended: a child process over stdin)

**CGX changes**, all opt-in behind `-pipe` (and `-safe`) except the `hcpy` fix, which is a plain bug fix:

| Change | Where | Lines |
|---|---|---|
| Deferred `hcpy`: set a pending file name, capture after the full render pass and before swap, print `@cgx file …` | `setFunktions.c:17364`, `cgx_glut_glfw.c` main loop near `:2071`, `cgx.c:879` | 30–40 |
| Ready gate: do not drain the queue while `g_idle_func` is set; print `@cgx ready` | `cgx_glut_glfw.c:1963`, `cgx.c:7008` | 10 |
| `cgx_execute_command_string` returns `int`; `@cgx begin/end seq rc`; unbuffered stdout | `cgx.c:5836-5907`, `cgx_glut_glfw.c:228-242` | 30 |
| Reject lines that are too long, never split them; report a full queue | `cgx_glut_glfw.c:216-260` | 15 |
| `-safe` | `cgx.c` argv parsing, `setFunktions.c` (SYS/EXIT/SAVE/MOVI/GRAPH/READ/HCPY/SEND) | 40 |
| *Later:* socket or named pipe for attaching to a user-started CGX | new `cgx_channel.c` | 150–250 |

**MCP tools** (new `cgx_tools.rs` with a session manager, like `jobs.rs`):
- `cgx_open {path (.frd/.inp, root-confined), visible?: bool=true}` → `{session_id, datasets: [...]}`. Spawns `cgx_glfw -pipe -safe <file>` with cwd set to a private session directory, waits for `@cgx ready`, and keeps the child's stdout away from the MCP stdout.
- `cgx_show {session_id, step|dataset, entity, view?: "iso"|"+x"…, deformed?: bool, scale?}` → `{min, max, node_min, node_max}`, parsed from the `plot` output.
- `cgx_snapshot {session_id, width?, height?}` → PNG as MCP image content, plus its path.
- `cgx_command {session_id, command}`: whitelisted keywords only; returns `{rc, output}`.
- `cgx_close {session_id}`.

**Risks:**
- The visible window takes focus on macOS.
- The user and the agent can both drive the session; they share one queue, so interleaving is possible but safe.
- A child crash surfaces as stdout EOF.
- Output parsing depends on CGX's free-text messages.
- Per-machine GL driver quirks (the stderr warning seen in run 5).

**Effort:** CGX about 130 lines, 1–1.5 days including manual tests on macOS; Linux and Windows smoke tests are extra. Rust about 500 lines plus tests, 2 days.

### (B) Generate an `.fbd` and run CGX once per request

`-bg` cannot do this (no GL). It needs either `-b` (a window opens and closes for each request) or a new `-offscreen` mode.

**CGX changes:**
- The same deferred `hcpy`, plus either making `readfbd` in `-b` mode yield a rendered frame at `hcpy` (about 60 lines, because the `.fbd` currently runs inside a single idle call), or adding `-offscreen WxH` (hidden window plus FBO, about 120–150 lines).
- `-safe` (40 lines).
- Total about 130–250 lines.

**MCP tools:**
- `cgx_render {path, step|dataset, entity, view?, deformed?, width?, height?}` → PNG and `{min, max}`. It generates an `.fbd` in a private temp directory, runs `cgx_glfw -offscreen … -safe x.fbd` under the existing job-style timeout and process-group kill, and returns the image.
- Optional `cgx_render_batch` for several entities in one run, so the model is read only once.

**Risks:**
- The model is re-read for every request: slow for large `.frd` files, for example the 409 MB `lavffcyc.frd`.
- There is no visible session for the user to look at.
- A window flashes on the screen unless offscreen mode is built.
- Offscreen mode depends on the GL driver; Linux needs a display or Xvfb.

**Effort:** CGX 1.5–2.5 days (FBO and loader work on three platforms is the uncertain part). Rust about 250 lines, 1 day.

### Recommendation

Do **A as a child process over the stdin channel that already exists**. It reuses the queue that is already thread-safe, needs no socket and no Winsock, and keeps the user's visible session and its state (loaded model, view) between requests. Large `.frd` files are loaded once. B's per-request `.fbd` remains useful for unattended batch rendering once `-offscreen` exists, and it can reuse A's `-safe` mode and deferred `hcpy`.

**Smallest first step:** the deferred `hcpy` fix plus the ready gate (about 40 lines in `setFunktions.c:17364`, `cgx_glut_glfw.c:1963` and near `:2071`). This is an ordinary bug fix that makes `hcpy` from the command bar, from stdin and from `-b` deterministic. Verify it by repeating runs 2 and 3 above: the first `hcpy` must show the contour and the legend. Then add `-pipe` (acknowledgement lines plus unbuffered stdout) and `-safe` before writing any Rust.
