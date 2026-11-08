"""Transparent stdio logger launching only the isolated repair binary/config."""
import json
import io
import os
from pathlib import Path
import subprocess
import signal
import sys
import threading
import time

ROOT = Path(__file__).resolve().parent
lock = threading.Lock()
log = (ROOT / "acp-transcript.jsonl").open("a", buffering=1)
env = dict(os.environ, ZEROCLAW_CONFIG_DIR=os.environ.get("ACP_TITLE_CONFIG_DIR", str(ROOT / "config")))
child = subprocess.Popen([str(ROOT / "zeroclaw"), "acp", "--agent", "proof"],
                         stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=(ROOT / "acp-stderr.log").open("a"), env=env)
signal.signal(signal.SIGTERM, lambda *_args: child.terminate())


def copy(source, destination, direction):
    for line in source:
        with lock:
            log.write(json.dumps({"time": time.time(), "pid": child.pid,
                                  "direction": direction, "line": line.decode().rstrip()}) + "\n")
        destination.write(line)
        destination.flush()
    if direction == "client_to_server":
        destination.close()


threading.Thread(target=copy, args=(io.FileIO(sys.stdin.fileno(), closefd=False), child.stdin, "client_to_server"), daemon=True).start()
try:
    copy(child.stdout, sys.stdout.buffer, "server_to_client")
finally:
    child.terminate()
    child.wait(timeout=5)
