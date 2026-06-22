"""Realistic mock LLM upstream for recording the Cachet demo GIF.

Unlike a toy mock, this returns paragraph-length completions (~260 tokens) so the
token counts — and therefore the estimated savings — are realistic, not tiny. It
speaks the OpenAI chat-completions shape for both non-streaming and streaming
(SSE) requests. No API key, no network needed.

    python3 scripts/demo_upstream.py        # listens on 127.0.0.1:9999
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# A believable medium-length answer (~1050 chars ≈ 260 tokens at ~4 chars/token).
ANSWER = (
    "Great question. The short answer is that it depends on your context, but a few "
    "principles hold up well in practice. First, the core idea is well established and "
    "widely used in production, so you can rely on it being stable and well documented. "
    "Second, weigh the usual trade-offs: performance versus simplicity, memory versus "
    "speed, and flexibility versus safety. In most real cases the pragmatic move is to "
    "start with the simplest approach that correctly solves the problem, measure it under "
    "realistic load, and only then optimize the parts that actually matter. Third, watch "
    "for the classic pitfalls — off-by-one errors, unhandled edge cases, and assumptions "
    "about input that stop holding once real users arrive. Finally, when unsure, prototype "
    "both options and compare them with a small benchmark; the data usually makes the "
    "decision obvious. Choose for clarity first and cleverness second, because code is read "
    "far more often than it is written."
)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    calls = 0

    def _chunk(self, data: bytes):
        self.wfile.write(f"{len(data):X}\r\n".encode())
        self.wfile.write(data)
        self.wfile.write(b"\r\n")
        self.wfile.flush()

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length)
        try:
            req = json.loads(raw)
        except Exception:
            req = {}
        Handler.calls += 1
        model = req.get("model", "gpt-4o")
        streaming = req.get("stream") is True
        print(f"CALL {Handler.calls} {'stream' if streaming else 'json'} model={model}", flush=True)

        if streaming:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            words = ANSWER.split(" ")
            # ~5 words per event so the stream visibly arrives in pieces.
            for i in range(0, len(words), 5):
                piece = " ".join(words[i : i + 5]) + " "
                self._chunk(("data: " + json.dumps({"choices": [{"delta": {"content": piece}}]}) + "\n\n").encode())
                time.sleep(0.04)
            self._chunk(b"data: [DONE]\n\n")
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            return

        body = json.dumps(
            {
                "id": "chatcmpl-demo",
                "object": "chat.completion",
                "model": model,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": ANSWER}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0},
            }
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    host, port = "127.0.0.1", 9999
    print(f"demo upstream listening on {host}:{port} (answer ≈ {len(ANSWER)} chars)", flush=True)
    try:
        ThreadingHTTPServer((host, port), Handler).serve_forever()
    except KeyboardInterrupt:
        sys.exit(0)
