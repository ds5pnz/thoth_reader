#!/usr/bin/env python3
"""Dev-only OpenAI-compatible mock (loopback). Echoes each segment as "[번역] <text>" so the
full pipeline (probe, SSE streaming, validation, placeholders, cache) can be exercised in the
real app without a model. Prints only request counts, never request bodies.

usage: python3 -I scripts/mock_openai_server.py <port> [delay_ms]
"""
import json, sys, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1]); DELAY = int(sys.argv[2]) / 1000 if len(sys.argv) > 2 else 0.05
COUNT = {"chat": 0, "models": 0}

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a):
        pass
    def _json(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        if self.path.endswith("/models"):
            COUNT["models"] += 1
            return self._json(200, {"object": "list", "data": [{"id": "mock-ko", "object": "model"}]})
        if self.path.endswith("/__count"):
            return self._json(200, COUNT)
        self._json(404, {"error": "not found"})
    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0")); req = json.loads(self.rfile.read(n) or b"{}")
        if not self.path.endswith("/chat/completions"):
            return self._json(404, {"error": "not found"})
        COUNT["chat"] += 1
        user = next((m["content"] for m in reversed(req.get("messages", [])) if m.get("role") == "user"), "")
        try:
            segs = json.loads(user).get("segments", [])
        except Exception:
            segs = [{"id": "s0", "text": user}]
        structured = "response_format" in req
        if structured:
            reply = json.dumps({"segments": [{"id": s["id"], "text": "[번역] " + s["text"]} for s in segs]}, ensure_ascii=False)
        else:
            reply = "[번역] " + (segs[0]["text"] if segs else "")
        print(json.dumps({"chat": COUNT["chat"], "stream": bool(req.get("stream")), "structured": structured}), flush=True)
        if not req.get("stream"):
            return self._json(200, {"choices": [{"index": 0, "message": {"role": "assistant", "content": reply}, "finish_reason": "stop"}]})
        self.send_response(200); self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked"); self.end_headers()
        def chunk(data: str):
            b = data.encode(); self.wfile.write(f"{len(b):x}\r\n".encode() + b + b"\r\n"); self.wfile.flush()
        step = max(1, len(reply) // 4)
        for i in range(0, len(reply), step):
            time.sleep(DELAY)
            chunk("data: " + json.dumps({"choices": [{"index": 0, "delta": {"content": reply[i:i + step]}, "finish_reason": None}]}, ensure_ascii=False) + "\n\n")
        chunk("data: " + json.dumps({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}) + "\n\n")
        chunk("data: [DONE]\n\n")
        self.wfile.write(b"0\r\n\r\n"); self.wfile.flush()

ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
