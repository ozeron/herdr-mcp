"""Live check of the remote surface against the real Herdr, before a release.

Runs the given herdr-mcp binary as a tunnel bridge (a remote caller, like ChatGPT) over stdio,
in two throwaway workspaces it creates and always closes. The audit log goes to a temporary
state dir, never the real one. Prints one PASS or FAIL line per check; exits 1 on any FAIL.

    python3 -I probes/live_check.py target/debug/herdr-mcp
"""

import json
import os
import subprocess
import sys
import tempfile

LABELS = ("mcp-live-check-a", "mcp-live-check-b")


def herdr(*args):
    out = subprocess.run(["herdr", *args], capture_output=True, text=True, check=True).stdout
    return json.loads(out)


class Bridge:
    def __init__(self, binary, state):
        env = {k: v for k, v in os.environ.items() if not k.startswith("HERDR_PANE")}
        env.pop("HERDR_TAB_ID", None)
        env.pop("HERDR_WORKSPACE_ID", None)
        env["XDG_STATE_HOME"] = state
        self.proc = subprocess.Popen(
            [binary, "--tools", "control", "--tunnel"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=env,
            text=True,
        )
        self.next_id = 0
        self.rpc("initialize", {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "openai-mcp", "version": "live-check"},
        })
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, msg):
        self.proc.stdin.write(json.dumps(msg) + "\n")
        self.proc.stdin.flush()

    def rpc(self, method, params):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        while True:
            reply = json.loads(self.proc.stdout.readline())
            if reply.get("id") == self.next_id:
                return reply

    def tool(self, name, args):
        result = self.rpc("tools/call", {"name": name, "arguments": args})["result"]
        return result.get("structuredContent") or {}, bool(result.get("isError"))

    def close(self):
        self.proc.stdin.close()
        self.proc.terminate()
        self.proc.wait(timeout=5)


def find_pane(listing, pane_id):
    for workspace in listing.get("workspaces", []):
        for tab in workspace["tabs"]:
            for pane in tab["panes"]:
                if pane["id"] == pane_id:
                    return pane
    return None


def main(binary):
    failures = 0

    def check(name, ok, detail=""):
        nonlocal failures
        failures += not ok
        print(f"{'PASS' if ok else 'FAIL'} {name}" + (f": {detail}" if detail and not ok else ""))

    created = []
    with tempfile.TemporaryDirectory() as state:
        try:
            for label in LABELS:
                created.append(herdr("workspace", "create", "--label", label, "--cwd", "/tmp",
                                     "--no-focus")["result"]["workspace"]["workspace_id"])
            a, b = created
            bridge = Bridge(binary, state)
            try:
                caps, _ = bridge.tool("herdr_capabilities", {})
                check("remote caller has no pane", caps.get("caller") is None, str(caps.get("caller")))
                check("remote inbox is chatgpt", caps.get("inbox") == "chatgpt", str(caps.get("inbox")))

                out, err = bridge.tool("herdr_run", {"command": "sleep 120"})
                check("no workspace is refused", err and out.get("code") == "destination_required",
                      str(out.get("code")))

                out, err = bridge.tool("herdr_run", {"command": "sleep 120", "label": "probe",
                                                     "where": {"workspace_id": LABELS[0]}})
                pane = out.get("pane")
                check("label places in the named workspace", not err and out.get("workspace") == a,
                      json.dumps(out)[:200])

                listing, _ = bridge.tool("herdr_list", {"label_contains": "mcp-live-check", "tail": 3})
                listed = find_pane(listing, pane) if pane else None
                check("list tail returns lines", bool(listed) and isinstance(listed.get("tail"), list))

                read, err = bridge.tool("herdr_read", {"target": pane, "source": "visible"})
                check("read visible works", not err and read.get("source") == "visible")

                moved, err = bridge.tool("herdr_move", {"target": pane, "workspace_id": b})
                new = moved.get("pane")
                check("move changes workspace", not err and moved.get("workspace") == b,
                      json.dumps(moved)[:200])
                listing, _ = bridge.tool("herdr_list", {"label_contains": "mcp-live-check"})
                after = find_pane(listing, new) if new else None
                fg = (after or {}).get("foreground", {}).get("name")
                check("moved process keeps running", fg == "sleep", str(fg))
                check("moved pane keeps launched_by", (after or {}).get("launched_by") == "external",
                      str((after or {}).get("launched_by")))
            finally:
                bridge.close()
        finally:
            for workspace in created:
                subprocess.run(["herdr", "workspace", "close", workspace], capture_output=True)
    print("all checks passed" if failures == 0 else f"{failures} check(s) failed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1]))
