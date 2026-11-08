"""Local deterministic provider for PR 12286 client verification; no credentials."""
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent
COMMAND = "  sleep  30; printf   ACP-title-verified  "


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        self.respond({"object": "list", "data": [{"id": "acp-title-fixture", "object": "model"}]})

    def respond(self, value):
        payload = json.dumps(value).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with (ROOT / "provider-requests.jsonl").open("a") as log:
            log.write(json.dumps(request) + "\n")
        messages = request.get("messages", [])
        last_user = max((i for i, item in enumerate(messages) if item["role"] == "user"), default=-1)
        completed = any(item["role"] == "tool" for item in messages[last_user + 1:])
        if completed:
            message = {"role": "assistant", "content": "The harmless shell title check completed."}
            reason = "stop"
        else:
            message = {"role": "assistant", "content": None, "tool_calls": [{
                "id": "acp-title-shell", "type": "function",
                "function": {"name": "shell", "arguments": json.dumps({"command": COMMAND})}
            }]}
            reason = "tool_calls"
        if request.get("stream"):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            delta = dict(message)
            if "tool_calls" in delta:
                delta["tool_calls"][0]["index"] = 0
            for chunk in [
                {"id": "proof", "object": "chat.completion.chunk", "model": "acp-title-fixture", "choices": [{"index": 0, "delta": delta, "finish_reason": None}]},
                {"id": "proof", "object": "chat.completion.chunk", "model": "acp-title-fixture", "choices": [{"index": 0, "delta": {}, "finish_reason": reason}], "usage": {"prompt_tokens": 32, "completion_tokens": 8, "total_tokens": 40}},
            ]:
                self.wfile.write(("data: " + json.dumps(chunk) + "\n\n").encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
        else:
            self.respond({"id": "proof", "object": "chat.completion", "model": "acp-title-fixture", "choices": [{"index": 0, "message": message, "finish_reason": reason}], "usage": {"prompt_tokens": 32, "completion_tokens": 8, "total_tokens": 40}})


server = ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1]) if len(sys.argv) > 1 else 0), Handler)
config_dir = ROOT / "config"
config_dir.mkdir(exist_ok=True)
(ROOT / "workspace").mkdir(exist_ok=True)
(config_dir / "config.toml").write_text(f'''
[providers.models.llamacpp.proof]
uri = "http://127.0.0.1:{server.server_port}/v1"
model = "acp-title-fixture"
native_tools = true

[risk_profiles.proof]
level = "supervised"
workspace_only = true
allowed_tools = ["shell"]
allowed_commands = ["printf", "sleep"]
always_ask = ["shell"]

[runtime_profiles.proof]
agentic = true
max_tool_iterations = 3

[agents.proof]
model_provider = "llamacpp.proof"
risk_profile = "proof"
runtime_profile = "proof"

[acp]
default_agent = "proof"
''')
print(f"Local fixture provider ready on port {server.server_port}", flush=True)
server.serve_forever()
