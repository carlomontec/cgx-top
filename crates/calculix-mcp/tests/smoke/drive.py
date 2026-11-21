import json, subprocess, sys, os, time, glob
"""End-to-end smoke test of calculix-mcp over stdio with a real ccx binary.

Run from the workspace root after `cargo build -p calculix-mcp`:
    python3 crates/calculix-mcp/tests/smoke/drive.py [name=]/abs/path/to/ccx_binary
Works with a CalculiX MultiSolver build (telemetry_source "json") and with stock ccx
(telemetry_source "sta_cvg"); the assertions that only JSON can satisfy branch on it.
It solves a one-element cube in an isolated run directory, queries the results, then checks
that a client disconnect kills a hanging (fake) solver, and finally that without --root the
models folder is the current directory and can be switched with models_folder. Scratch files go
to target/mcp-solve.
"""
import shutil, stat
HERE = os.path.dirname(os.path.abspath(__file__))
shutil.rmtree("target/mcp-solve", ignore_errors=True)
os.makedirs("target/mcp-solve/hang")
shutil.copytree(os.path.join(HERE, "mesh"), "target/mcp-solve/cube/mesh")
shutil.copy(os.path.join(HERE, "cube.inp"), "target/mcp-solve/cube/cube.inp")
with open("target/mcp-solve/hang/fake_ccx", "w") as f: f.write("#!/bin/sh\nsleep 300\n")
os.chmod("target/mcp-solve/hang/fake_ccx", 0o755)
with open("target/mcp-solve/hang/h.inp", "w") as f: f.write("*STEP\n*STATIC\n*END STEP\n")

class Server:
    def __init__(self, args, cwd=None):
        self.p = subprocess.Popen([os.path.abspath("target/debug/calculix-mcp")] + args, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        self.n = 0
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "drive", "version": "0"}})
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n"); self.p.stdin.flush()
    def rpc(self, method, params):
        self.n += 1
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params}) + "\n"); self.p.stdin.flush()
        while True:
            m = json.loads(self.p.stdout.readline())
            if m.get("id") == self.n: return m.get("result", m.get("error"))
    def call(self, name, **args):
        r = self.rpc("tools/call", {"name": name, "arguments": args})
        return bool(r.get("isError")), r.get("structuredContent", r)
def show(tag, v, n=700): print(tag, json.dumps(v)[:n], "\n")

ccx_arg = sys.argv[1]
if "=" in ccx_arg:
    solver_name, ccx = ccx_arg.split("=", 1)
else:
    solver_name, ccx = "ccx", ccx_arg
s = Server(["--root", "target/mcp-solve/cube", "--ccx", f"{solver_name}={ccx}"])
print("tools:", sorted(t["name"] for t in s.rpc("tools/list", {})["tools"]), "\n")
show("unknown solver:", s.call("ccx_solve", inp_path="cube.inp", solver="pardiso")[1])
err, job = s.call("ccx_solve", inp_path="cube.inp", threads=2, label="smoke"); show("solve:", job); assert not err
jid = job["job_id"]
for _ in range(10):
    err, st = s.call("ccx_job_status", job_id=jid, wait_s=20, tail_lines=6)
    if st["state"] not in ("QUEUED", "STARTING", "RUNNING", "CANCELLING"): break
show("status:", st, 1500)
assert st["state"] == "SUCCEEDED", st
src = st["telemetry_source"]
assert src == job["telemetry_source"] and src in ("json", "sta_cvg"), (src, job)
print("telemetry_source:", src, "\n")
if src == "json":
    assert "json" in st["files"] and st["telemetry"]["meta"]["success"] is True and not st.get("notes"), st
else:
    # Stock ccx writes no <job>.json: the status says so instead of staying silent.
    assert "json" not in st["files"] and st["telemetry"] is None, st
    assert len(st["notes"]) == 1 and st["notes"][0].startswith("Stock CalculiX: no JSON telemetry"), st
    assert st["progress"]["increments_done"] >= 1, st
diag = s.call("ccx_diagnose", job_id=jid)[1]
show("diagnose:", diag, 500)
assert diag["verdict"] == "LINEAR", diag
assert diag["coverage"]["json"] == (src == "json") and diag["coverage"]["sta_rows"] >= 1, diag
frd = st["files"]["frd"]
show("summary:", s.call("frd_summary", path=frd)[1], 900)
err, query = s.call("frd_query_set", path=frd, set="NTOP", inp_path="cube.inp", quantity="displacement", top_n=2)
show("query NTOP:", query, 700)
# One C3D8 cube, 100 N on the top face: the result is fixed by the deck, whatever the solver.
u_max = query["envelope"]["u_max"][0]
assert not err and abs(u_max - 0.0018111742) < 1e-6, query
print("original deck dir untouched:", sorted(os.listdir("target/mcp-solve/cube")), "\n")
jobs = s.call("ccx_job_list", limit=5)[1]
show("job list:", jobs)
assert jobs["jobs"]["columns"][-1] == "telemetry_source" and jobs["jobs"]["rows"][0][-1] == src, jobs
show("cancel finished job:", s.call("ccx_job_cancel", job_id=jid)[1])
show("unknown job:", s.call("ccx_job_status", job_id="j0-dead")[1])
s.p.stdin.close(); s.p.wait(timeout=20)

# Client disconnect while a job runs: the solver must not survive the server.
h = Server(["--root", "target/mcp-solve/hang", "--ccx", "fake=target/mcp-solve/hang/fake_ccx"])
err, job = h.call("ccx_solve", inp_path="h.inp"); assert not err, job
err, st = h.call("ccx_job_status", job_id=job["job_id"], wait_s=5)
man = json.load(open(glob.glob("target/mcp-solve/hang/.calculix-mcp/runs/*/manifest.json")[0]))
pid = man["pid"]; os.kill(pid, 0)
print("hang job state:", st["state"], "pid", pid, "rss_mb", st["rss_mb"])
t = time.time(); h.p.stdin.close(); h.p.wait(timeout=30)
try:
    os.kill(pid, 0); alive = True
except ProcessLookupError: alive = False
man = json.load(open(glob.glob("target/mcp-solve/hang/.calculix-mcp/runs/*/manifest.json")[0]))
assert not alive and man["state"] == "CANCELLED"
print("after disconnect: server exit", h.p.returncode, "in %.1fs," % (time.time() - t), "solver alive:", alive, "manifest state:", man["state"])

# No --root: the models folder is the directory the client started the server in, and
# models_folder switches it. This client declares no roots capability, so cwd applies.
cube, hang = os.path.realpath("target/mcp-solve/cube"), os.path.realpath("target/mcp-solve/hang")
f = Server([], cwd=cube)
err, here = f.call("models_folder"); show("models_folder:", here)
assert not err and here["source"] == "cwd" and here["folders"] == [cube] and here["fixed"] is False, here
err, there = f.call("models_folder", path=hang); show("models_folder switch:", there)
assert not err and there["source"] == "tool" and there["folders"] == [hang], there
err, deck = f.call("inp_inspect", path="h.inp")
assert not err, deck  # relative paths now resolve in the new folder
err, refused = f.call("models_folder", path="/")
assert err and refused["code"] == "FOLDER_REFUSED", refused
f.p.stdin.close(); f.p.wait(timeout=20)
print("models folder: cwd", cube, "-> tool", hang, "; '/' refused")
