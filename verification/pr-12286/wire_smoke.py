"""Exercise real ACP stdio, shell execution, and durable session/load."""
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import sys
import threading

ROOT = Path(__file__).resolve().parent
(ROOT / "wire-config").mkdir(exist_ok=True)
shutil.copyfile(ROOT / "config" / "config.toml", ROOT / "wire-config" / "config.toml")
process = subprocess.Popen([sys.executable, str(ROOT / "acp_wrapper.py")],
                           stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                           env=dict(os.environ, ACP_TITLE_CONFIG_DIR=str(ROOT / "wire-config")))
frames = queue.Queue()
threading.Thread(target=lambda: [frames.put(json.loads(line)) for line in process.stdout], daemon=True).start()
transcript = []


def send(value):
    process.stdin.write(json.dumps(value) + "\n")
    process.stdin.flush()


def call(request_id, method, params):
    send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
    observed = []
    while True:
        value = frames.get(timeout=60)
        transcript.append(value)
        observed.append(value)
        if value.get("method") == "session/request_permission":
            options = value["params"]["options"]
            once = next(option["optionId"] for option in options if option["kind"] == "allow_once")
            send({"jsonrpc": "2.0", "id": value["id"], "result": {"outcome": {"outcome": "selected", "optionId": once}}})
        elif value.get("id") == request_id and "method" not in value:
            assert "error" not in value, value
            return value["result"], observed


def check_titles(frames, label):
    updates = [frame["params"]["update"] for frame in frames if frame.get("method") == "session/update"]
    calls = [update for update in updates if update.get("toolCallId") == "acp-title-shell" and update.get("sessionUpdate") == "tool_call"]
    results = [update for update in updates if update.get("toolCallId") == "acp-title-shell" and update.get("sessionUpdate") == "tool_call_update"]
    assert len(calls) == 1 and len(results) == 1, updates
    assert calls[0]["title"] == "sleep 30; printf ACP-title-verified", calls
    assert calls[0]["status"] == ("pending" if label == "live" else "completed"), calls
    assert results[0]["title"] == "sleep 30; printf ACP-title-verified", results
    assert results[0]["status"] == "completed", results
    assert "ACP-title-verified" in json.dumps(results[0]["content"]), results
    print(f"PASS: {label}: command titles, statuses, and real shell output", flush=True)


try:
    call(1, "initialize", {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "acp-title-wire-smoke", "version": "1"}})
    new, _ = call(2, "session/new", {"cwd": str(ROOT / "workspace"), "mcpServers": []})
    sid = new["sessionId"]
    result, live = call(3, "session/prompt", {"sessionId": sid, "prompt": [{"type": "text", "text": "Run the harmless shell title check."}]})
    assert result["stopReason"] == "end_turn", result
    check_titles(live, "live")
    call(4, "session/close", {"sessionId": sid})
    _, replay = call(5, "session/load", {"sessionId": sid, "cwd": str(ROOT / "workspace"), "mcpServers": []})
    check_titles(replay, "session/load")
    (ROOT / "wire-smoke-transcript.json").write_text(json.dumps(transcript, indent=2) + "\n")
finally:
    process.stdin.close()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.terminate()
