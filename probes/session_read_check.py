"""Compare saved assistant output with the assistant region of a terminal read.

Targets must be idle Claude/Codex probe panes containing a single short plain-text
answer. This script never launches agents, sends prompts, or changes configuration.
It prints only pass/fail metrics, never transcript contents. Terminal `recent`
reads can scroll an idle agent and restore its viewport; session reads do not.

    python3 -I probes/session_read_check.py target/debug/herdr-mcp \
        --target <claude-probe> --target <codex-probe>

Optionally pass --expected-file with a JSON object mapping each target to its exact
expected answer. This also checks byte-for-byte native text, including Unicode.
"""

import argparse
import json
import re
import sys
import tempfile
import time
from pathlib import Path

# `-I` omits the script directory from sys.path; reuse the existing stdio harness.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from live_check import Bridge


def normalized(text):
    return "".join(character for character in text if not character.isspace())


def assistant_region(text, provider):
    marker = "⏺" if provider == "claude" else "•"
    starts = list(re.finditer(r"(?m)^[ \t]*" + re.escape(marker) + r" ", text))
    return text[starts[-1].end():] if starts else ""


def check(bridge, target, expected):
    started = time.monotonic()
    native, error = bridge.tool("herdr_read", {"target": target, "source": "session"})
    native_ms = round((time.monotonic() - started) * 1000)
    if error:
        return {"pass": False, "reason": native.get("code", "native read failed")}
    started = time.monotonic()
    terminal, terminal_error = bridge.tool("herdr_read", {
        "target": target, "source": "recent", "lines": 200,
    })
    terminal_ms = round((time.monotonic() - started) * 1000)
    repeated, repeat_error = bridge.tool("herdr_read", {
        "target": target, "source": "session", "after": native["next_cursor"],
    })
    region = assistant_region(terminal.get("text", ""), native["provider"])
    content = native["text"]
    checks = {
        "native_matches_assistant_region_excluding_prompt": (
            not terminal_error and bool(content) and bool(region)
            and normalized(content) in normalized(region)
        ),
        "unchanged_cursor_returns_empty": (
            not repeat_error and repeated.get("text") == ""
            and repeated.get("next_cursor") == native["next_cursor"]
        ),
        "same_session": repeated.get("session_id") == native["session_id"],
        "settled_status": native.get("status") in ("idle", "done"),
    }
    if expected is not None:
        checks["exact_expected_answer"] = content == expected
    return {
        "provider": native["provider"], "pass": all(checks.values()),
        "checks": checks, "native_ms": native_ms, "terminal_ms": terminal_ms,
        "native_chars": len(content),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary")
    parser.add_argument("--target", action="append", required=True)
    parser.add_argument("--expected-file", type=Path)
    args = parser.parse_args()
    expected = json.loads(args.expected_file.read_text()) if args.expected_file else {}
    results = []
    with tempfile.TemporaryDirectory(prefix="herdr-session-probe-") as state:
        bridge = Bridge(args.binary, state)
        try:
            for target in args.target:
                result = check(bridge, target, expected.get(target))
                results.append(result)
                print(json.dumps(result), flush=True)
        finally:
            bridge.close()
    return 0 if all(result["pass"] for result in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
