#!/usr/bin/env python3
"""Probe helper for the scratch Herdr session only.

Usage:
  herdr_probe.py call <method> '<json params>'
  herdr_probe.py watch <seconds> '<json subscriptions>'   # prints events as NDJSON
  herdr_probe.py poll <seconds> <target> [interval_s]     # agent.get snapshots on change
"""

import json
import os
import socket
import sys
import time

SESSION = "herdr-mcp-probe"
SOCKET = os.path.expanduser(f"~/.config/herdr/sessions/{SESSION}/herdr.sock")


def _connect():
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(SOCKET)
    return sock


def call(method, params=None, timeout=330):
    sock = _connect()
    sock.settimeout(timeout)
    req = {"id": f"probe:{method}:{time.time_ns()}", "method": method, "params": params or {}}
    sock.sendall((json.dumps(req) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        chunk = sock.recv(65536)
        if not chunk:
            break
        buf += chunk
    sock.close()
    return json.loads(buf)


def watch(seconds, subscriptions):
    sock = _connect()
    sock.settimeout(1)
    req = {"id": "probe:watch", "method": "events.subscribe", "params": {"subscriptions": subscriptions}}
    sock.sendall((json.dumps(req) + "\n").encode())
    end = time.time() + seconds
    buf = b""
    while time.time() < end:
        try:
            chunk = sock.recv(65536)
        except socket.timeout:
            continue
        if not chunk:
            break
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            msg = json.loads(line)
            msg["_t"] = round(time.time(), 3)
            print(json.dumps(msg), flush=True)
    sock.close()


def poll(seconds, target, interval):
    last = None
    end = time.time() + seconds
    keys = ("agent_status", "state_change_seq", "completion_seq", "interactive_ready", "launch_pending", "terminal_id")
    while time.time() < end:
        res = call("agent.get", {"target": target}, timeout=5)
        agent = res.get("result", {}).get("agent")
        snap = {k: agent.get(k) for k in keys} if agent else res.get("error")
        if snap != last:
            print(json.dumps({"_t": round(time.time(), 3), **(snap if isinstance(snap, dict) else {"error": snap})}), flush=True)
            last = snap
        time.sleep(interval)


if __name__ == "__main__":
    cmd = sys.argv[1]
    if cmd == "call":
        print(json.dumps(call(sys.argv[2], json.loads(sys.argv[3]) if len(sys.argv) > 3 else {})))
    elif cmd == "watch":
        watch(float(sys.argv[2]), json.loads(sys.argv[3]))
    elif cmd == "poll":
        poll(float(sys.argv[2]), sys.argv[3], float(sys.argv[4]) if len(sys.argv) > 4 else 0.2)
    else:
        sys.exit(__doc__)
