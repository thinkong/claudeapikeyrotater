"""A tiny fake of the Claude API used by the end-to-end test.

Behaviour per upstream key is controlled with POST /mock/config:
  {"credit_400": [...keys], "credit_402": [...], "ratelimit": [...], "auth": [...]}
"""
import hashlib
import json
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

STATE = {"credit_400": [], "credit_402": [], "ratelimit": [], "auth": []}
LOG = []
FILES = {}  # file_id -> key
LOCK = threading.Lock()
USAGE = {
    "input_tokens": 100,
    "output_tokens": 50,
    "cache_read_input_tokens": 1000,
    "cache_creation_input_tokens": 200,
    "cache_creation": {"ephemeral_5m_input_tokens": 200, "ephemeral_1h_input_tokens": 0},
    "service_tier": "standard",
}


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _json(self, status, obj, extra=None):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.send_header("request-id", "req_mock")
        for k, v in (extra or {}).items():
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def _err(self, status, etype, msg, extra=None):
        self._json(status, {"type": "error", "error": {"type": etype, "message": msg}}, extra)

    def _body(self):
        n = int(self.headers.get("content-length") or 0)
        return self.rfile.read(n) if n else b""

    def do_GET(self):
        key = self.headers.get("x-api-key")
        with LOCK:
            LOG.append({"method": "GET", "path": self.path, "key": key})
        if self.path == "/mock/log":
            return self._json(200, LOG)
        if self.path.startswith("/v1/files/") and self.path.endswith("/content"):
            fid = self.path.split("/")[3]
            if FILES.get(fid) != key:
                return self._err(404, "not_found_error", "file not found for this key")
            return self._json(200, {"content": "ok"})
        if self.path.startswith("/v1/messages/batches/") and self.path.endswith("/results"):
            lines = [
                {"custom_id": "a", "result": {"type": "succeeded", "message": {"id": "m1", "type": "message", "model": "claude-opus-5-5", "content": [], "usage": dict(USAGE)}}},
                {"custom_id": "b", "result": {"type": "errored", "error": {"type": "invalid_request_error"}}},
            ]
            body = "".join(json.dumps(l) + "\n" for l in lines).encode()
            self.send_response(200)
            self.send_header("content-type", "application/binary")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if self.path.startswith("/v1/messages/batches/"):
            bid = self.path.split("/")[4]
            # Like the real API: an absolute results_url on the API host.
            return self._json(200, {"id": bid, "type": "message_batch", "processing_status": "ended",
                                    "results_url": "http://127.0.0.1:9901/v1/messages/batches/%s/results" % bid})
        if self.path.startswith("/v1/models"):
            return self._json(200, {"data": [{"id": "claude-opus-5-5", "type": "model"}], "has_more": False})
        self._err(404, "not_found_error", "unknown path " + self.path)

    def do_POST(self):
        key = self.headers.get("x-api-key")
        raw = self._body()
        if self.path == "/mock/config":
            STATE.update(json.loads(raw))
            return self._json(200, STATE)
        with LOCK:
            LOG.append({"method": "POST", "path": self.path, "key": key, "beta": self.headers.get("anthropic-beta")})
        if key in STATE["auth"]:
            return self._err(401, "authentication_error", "invalid x-api-key")
        if key in STATE["credit_400"]:
            return self._err(400, "invalid_request_error", "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.")
        if key in STATE["credit_402"]:
            return self._err(402, "billing_error", "Insufficient credits.")
        if key in STATE["ratelimit"]:
            return self._err(429, "rate_limit_error", "slow down", {"retry-after": "7"})

        if self.path == "/v1/files":
            fid = "file_%s_%d" % (key[-1], len(FILES))
            FILES[fid] = key
            return self._json(200, {"id": fid, "type": "file", "filename": "x.txt", "size_bytes": len(raw)})
        if self.path == "/v1/messages/batches":
            body = json.loads(raw)
            return self._json(200, {"id": "msgbatch_%s" % key[-1], "type": "message_batch", "processing_status": "in_progress"}, {"x-mock-body-sha256": hashlib.sha256(raw).hexdigest()})
        if self.path == "/v1/messages":
            body = json.loads(raw)
            saw_top = hashlib.sha256(raw).hexdigest()  # lets the test verify byte-for-byte forwarding
            for m in body.get("messages", []):
                for c in m.get("content", []) if isinstance(m.get("content"), list) else []:
                    src = c.get("source", {}) if isinstance(c, dict) else {}
                    if src.get("type") == "file" and FILES.get(src.get("file_id")) != key:
                        return self._err(404, "not_found_error", "file not found for this key")
            model = body.get("model", "claude-opus-5-5")
            if body.get("stream"):
                return self._stream(model, saw_top, self.headers.get("x-mock-slow") == "1")
            msg = {
                "id": "msg_1", "type": "message", "role": "assistant", "model": model,
                "content": [{"type": "text", "text": "hello"}], "stop_reason": "end_turn",
                "stop_sequence": None, "usage": dict(USAGE),
            }
            return self._json(200, msg, {"x-mock-body-sha256": saw_top})
        self._err(404, "not_found_error", "unknown path " + self.path)

    def _stream(self, model, saw_top, slow):
        start_usage = {"input_tokens": 100, "output_tokens": 1, "cache_read_input_tokens": 1000,
                       "cache_creation_input_tokens": 200,
                       "cache_creation": {"ephemeral_5m_input_tokens": 200, "ephemeral_1h_input_tokens": 0}}
        events = [
            ("message_start", {"type": "message_start", "message": {"id": "msg_s", "type": "message", "role": "assistant", "model": model, "content": [], "stop_reason": None, "stop_sequence": None, "usage": start_usage}}),
            ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ("ping", {"type": "ping"}),
        ]
        for w in ["Hel", "lo ", "wor", "ld"]:
            events.append(("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": w}}))
        events += [
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None}, "usage": {"output_tokens": 50}}),
            ("message_stop", {"type": "message_stop"}),
        ]
        blob = "".join("event: %s\ndata: %s\n\n" % (n, json.dumps(d)) for n, d in events).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.send_header("x-mock-body-sha256", saw_top)
        self.end_headers()
        # Deliberately split into odd-sized chunks so events straddle chunk boundaries.
        step = 37
        try:
            for i in range(0, len(blob), step):
                part = blob[i:i + step]
                self.wfile.write(b"%x\r\n%s\r\n" % (len(part), part))
                self.wfile.flush()
                if slow:
                    time.sleep(0.05)
            self.wfile.write(b"0\r\n\r\n")
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            pass


class Server(ThreadingHTTPServer):
    def handle_error(self, request, client_address):
        pass  # disconnects are part of the test


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 9901
    Server(("127.0.0.1", port), H).serve_forever()
