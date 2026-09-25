#!/usr/bin/env python3
"""Zero-latency mock of the OpenAI Responses SSE endpoint (scratch measurement only).

Records every POST .../responses body (decompressing zstd/gzip), and returns a
canned assistant message "OK". Usage: mock_responses.py PORT OUTDIR
"""
import gzip, json, sys, time, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PORT = int(sys.argv[1]); OUT = Path(sys.argv[2]); OUT.mkdir(parents=True, exist_ok=True)
LOCK = threading.Lock(); COUNT = [0]; T0 = 0.0

def chars(v):
    return len(v) if isinstance(v, str) else len(json.dumps(v, ensure_ascii=False, separators=(",", ":")))

def analyse(body):
    instr = [body["instructions"]] if body.get("instructions") else []
    tools = list(body.get("tools") or [])
    items = body.get("input") or []
    items = items if isinstance(items, list) else [items]
    sys_items = []
    for it in items:
        if isinstance(it, dict) and it.get("role") in ("developer", "system"):
            sys_items.append(chars(it.get("content", "")))
    by_type = {}
    for it in items:
        t = (it.get("type") or "message") if isinstance(it, dict) else "text"
        if isinstance(it, dict) and t == "message":
            t = "message:" + str(it.get("role"))
        by_type[t] = by_type.get(t, 0) + 1
    return {
        "instructions_chars": sum(chars(x) for x in instr),
        "system_or_developer_items_chars": sys_items,
        "tools_count": len(tools),
        "tools_chars": sum(chars(t) for t in tools),
        "per_tool": [(t.get("name", t.get("type")), chars(t)) for t in tools],
        "input_items": by_type,
        "input_chars": chars(items),
        "keys": sorted(body.keys()),
        "prompt_cache_key": body.get("prompt_cache_key"),
        "reasoning": body.get("reasoning"),
        "store": body.get("store"), "include": body.get("include"),
        "parallel_tool_calls": body.get("parallel_tool_calls"),
        "text": body.get("text"),
    }

def sse(ev):
    return f"event: {ev['type']}\ndata: {json.dumps(ev)}\n\n".encode()

class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a):
        pass
    def do_GET(self):
        with LOCK:
            (OUT / "gets.log").open("a").write(self.path + "\n")
        b = b'{"error":{"message":"not found"}}'
        self.send_response(404); self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_POST(self):
        global T0
        T0 = time.time()
        n = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(n)
        enc = (self.headers.get("content-encoding") or "").lower()
        data = raw
        if enc == "zstd":
            import compression.zstd as z; data = z.decompress(raw)
        elif enc == "gzip":
            data = gzip.decompress(raw)
        if not self.path.split("?")[0].endswith("/responses"):
            with LOCK:
                (OUT / "posts_other.log").open("a").write(self.path + "\n")
            self.send_response(404); self.send_header("content-length", "0"); self.end_headers(); return
        body = json.loads(data)
        with LOCK:
            COUNT[0] += 1; idx = COUNT[0]
            (OUT / f"req-{idx:03d}.json").write_bytes(data)
            hdr = {k.lower(): v for k, v in self.headers.items() if k.lower() in (
                "content-encoding", "session-id", "session_id", "x-session-id", "thread-id", "originator",
                "openai-beta", "user-agent", "chatgpt-account-id", "conversation_id", "x-codex-turn-metadata")}
            row = {"idx": idx, "t_arrival": T0, "path": self.path, "wire_bytes": len(raw), "json_bytes": len(data),
                   "headers": hdr, **analyse(body)}
            (OUT / "requests.jsonl").open("a").write(json.dumps(row) + "\n")
        in_tok = max(1, len(data) // 4)
        model = body.get("model", "mock")
        now = int(time.time())
        msg = {"id": "msg_mock", "type": "message", "role": "assistant", "status": "completed",
               "content": [{"type": "output_text", "text": "OK", "annotations": []}]}
        resp = {"id": f"resp_mock_{idx}", "object": "response", "created_at": now, "status": "in_progress",
                "model": model, "output": [], "parallel_tool_calls": True, "tool_choice": "auto", "tools": [],
                "error": None, "incomplete_details": None, "instructions": None, "metadata": {},
                "temperature": 1.0, "top_p": 1.0, "text": {"format": {"type": "text"}}}
        events = [
            {"type": "response.created", "sequence_number": 0, "response": resp},
            {"type": "response.in_progress", "sequence_number": 1, "response": resp},
            {"type": "response.output_item.added", "sequence_number": 2, "output_index": 0,
             "item": {**msg, "status": "in_progress", "content": []}},
            {"type": "response.content_part.added", "sequence_number": 3, "item_id": "msg_mock", "output_index": 0,
             "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}},
            {"type": "response.output_text.delta", "sequence_number": 4, "item_id": "msg_mock", "output_index": 0,
             "content_index": 0, "delta": "OK", "logprobs": []},
            {"type": "response.output_text.done", "sequence_number": 5, "item_id": "msg_mock", "output_index": 0,
             "content_index": 0, "text": "OK", "logprobs": []},
            {"type": "response.content_part.done", "sequence_number": 6, "item_id": "msg_mock", "output_index": 0,
             "content_index": 0, "part": {"type": "output_text", "text": "OK", "annotations": []}},
            {"type": "response.output_item.done", "sequence_number": 7, "output_index": 0, "item": msg},
            {"type": "response.completed", "sequence_number": 8, "response": {**resp, "status": "completed",
             "completed_at": now, "output": [msg],
             "usage": {"input_tokens": in_tok, "input_tokens_details": {"cached_tokens": 0},
                       "output_tokens": 1, "output_tokens_details": {"reasoning_tokens": 0},
                       "total_tokens": in_tok + 1}}},
        ]
        import os
        tool_name = os.environ.get("MOCK_TOOL_NAME")
        if tool_name and idx == 1:
            args = os.environ["MOCK_TOOL_ARGS"]
            fc = {"id": "fc_mock", "type": "function_call", "call_id": "call_mock", "name": tool_name,
                  "arguments": args, "status": "completed"}
            events = [
                {"type": "response.created", "sequence_number": 0, "response": resp},
                {"type": "response.in_progress", "sequence_number": 1, "response": resp},
                {"type": "response.output_item.added", "sequence_number": 2, "output_index": 0,
                 "item": {**fc, "arguments": "", "status": "in_progress"}},
                {"type": "response.function_call_arguments.delta", "sequence_number": 3, "item_id": "fc_mock",
                 "output_index": 0, "delta": args},
                {"type": "response.function_call_arguments.done", "sequence_number": 4, "item_id": "fc_mock",
                 "output_index": 0, "name": tool_name, "arguments": args},
                {"type": "response.output_item.done", "sequence_number": 5, "output_index": 0, "item": fc},
                {"type": "response.completed", "sequence_number": 6, "response": {**resp, "status": "completed",
                 "completed_at": now, "output": [fc],
                 "usage": {"input_tokens": in_tok, "input_tokens_details": {"cached_tokens": 0},
                           "output_tokens": 1, "output_tokens_details": {"reasoning_tokens": 0},
                           "total_tokens": in_tok + 1}}},
            ]
        payload = b"".join(sse(e) for e in events)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(payload)))
        self.end_headers(); self.wfile.write(payload)

ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
