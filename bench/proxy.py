#!/usr/bin/env python3
"""Loopback Responses/Chat recorder with scripted SSE and optional live forwarding.

Only numeric usage, sizes, hashes, and timings are persisted. Authentication headers and
request/response bodies stay in memory; neither is printed.
"""

from __future__ import annotations

import argparse
import compression.zstd
import gzip
import hashlib
import http.client
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

MAX_REQUEST = 16 * 1024 * 1024
HOP_HEADERS = {"connection", "proxy-connection", "keep-alive", "transfer-encoding", "content-length", "content-encoding", "host", "accept-encoding"}


def encoded(value: object) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()


def token_estimate(body: bytes) -> int:
    """Deliberately labelled 4-byte/token estimate, not provider billing."""
    return (len(body) + 3) // 4


def usage_fields(raw: dict | None) -> dict:
    raw = raw or {}
    prompt = raw.get("input_tokens", raw.get("prompt_tokens"))
    completion = raw.get("output_tokens", raw.get("completion_tokens"))
    prompt_details = raw.get("input_tokens_details", raw.get("prompt_tokens_details")) or {}
    completion_details = raw.get("output_tokens_details", raw.get("completion_tokens_details")) or {}
    fields = {
        "input_tokens": prompt,
        "cached_tokens": prompt_details.get("cached_tokens", raw.get("cached_tokens")),
        "cache_write_tokens": prompt_details.get("cache_write_tokens", raw.get("cache_write_tokens")),
        "output_tokens": completion,
        "reasoning_tokens": completion_details.get("reasoning_tokens", raw.get("reasoning_tokens")),
        "cost_usd": raw.get("cost", raw.get("gateway_cost")),
    }
    return {key: value for key, value in fields.items() if isinstance(value, (int, float)) and not isinstance(value, bool)}


def request_shape(body: dict) -> dict:
    tools = body.get("tools") or []
    messages = body.get("messages") or body.get("input") or []
    messages = messages if isinstance(messages, list) else [messages]
    tool_outputs = []
    system_chars = len(str(body.get("instructions") or ""))
    for message in messages:
        if not isinstance(message, dict):
            continue
        if message.get("role") in {"system", "developer"}:
            system_chars += len(json.dumps(message.get("content", ""), ensure_ascii=False))
        if message.get("role") == "tool" or message.get("type") == "function_call_output":
            tool_outputs.append(str(message.get("content", message.get("output", ""))))
    return {
        "tools_count": len(tools),
        "tools_json_bytes": len(encoded(tools)),
        "instructions_chars": system_chars,
        "tool_output_chars": sum(len(value) for value in tool_outputs),
        "tool_output_has_recovery_hint": any(any(word in value.lower() for word in ("truncat", "handle", "full output", "artifact")) for value in tool_outputs),
    }


class Recorder:
    def __init__(self, path: Path, scenario: str, steps: int, command: str, harness: str):
        self.path = path
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.touch(mode=0o600)
        self.lock = threading.Lock()
        self.previous: bytes | None = None
        self.count = 0
        self.scenario = scenario
        self.steps = steps
        self.command = command
        self.harness = harness

    def reserve(self, body: bytes) -> tuple[int, int | None]:
        with self.lock:
            self.count += 1
            prefix = None
            if self.previous is not None:
                prefix = next((i for i, (a, b) in enumerate(zip(self.previous, body)) if a != b), min(len(self.previous), len(body)))
            self.previous = body
            return self.count, prefix

    def write(self, row: dict) -> None:
        with self.lock, self.path.open("a", encoding="utf-8") as stream:
            stream.write(json.dumps(row, sort_keys=True) + "\n")


def sse(event: dict) -> bytes:
    return b"data: " + encoded(event) + b"\n\n"


def mock_response(route: str, model: str, index: int, recorder: Recorder, request_size: int) -> bytes:
    tool = recorder.scenario in {"big_output", "big_file", "steps"} and index <= recorder.steps
    command = recorder.command
    if route.endswith("/chat/completions"):
        base = {"id": f"mock-{index}", "object": "chat.completion.chunk", "created": 1, "model": model}
        if tool:
            first = {**base, "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{"index": 0, "id": f"call-{index}", "type": "function", "function": {"name": "Bash", "arguments": encoded({"command": command}).decode()}}]}, "finish_reason": None}]}
            finish = "tool_calls"
        else:
            first = {**base, "choices": [{"index": 0, "delta": {"role": "assistant", "content": "OK"}, "finish_reason": None}]}
            finish = "stop"
        last = {**base, "choices": [{"index": 0, "delta": {}, "finish_reason": finish}], "usage": {"prompt_tokens": token_estimate(b"x" * request_size), "completion_tokens": 1, "prompt_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 0}, "cost": 0.0}}
        return sse(first) + sse(last) + b"data: [DONE]\n\n"
    response = {"id": f"resp-mock-{index}", "object": "response", "status": "in_progress", "model": model, "output": [], "tools": [], "parallel_tool_calls": True}
    if tool:
        tool_name, tool_arguments = {
            "codex": ("exec_command", {"cmd": command}),
            "codex_openrouter": ("exec_command", {"cmd": command}),
            "pi": ("bash", {"command": command, "timeout": None}),
            "omp": ("bash", {"i": "count", "command": command}),
            "unreal": ("Bash", {"command": command}),
        }.get(recorder.harness, ("exec_command", {"cmd": command}))
        item = {"id": f"item-{index}", "type": "function_call", "call_id": f"call-{index}", "name": tool_name, "arguments": encoded(tool_arguments).decode(), "status": "completed"}
        events = [
            {"type": "response.created", "sequence_number": 0, "response": response},
            {"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": {**item, "arguments": "", "status": "in_progress"}},
            {"type": "response.function_call_arguments.delta", "sequence_number": 2, "item_id": item["id"], "output_index": 0, "delta": item["arguments"]},
            {"type": "response.function_call_arguments.done", "sequence_number": 3, "item_id": item["id"], "output_index": 0, "name": item["name"], "arguments": item["arguments"]},
            {"type": "response.output_item.done", "sequence_number": 4, "output_index": 0, "item": item},
        ]
    else:
        item = {"id": f"item-{index}", "type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "OK", "annotations": []}]}
        events = [
            {"type": "response.created", "sequence_number": 0, "response": response},
            {"type": "response.output_item.added", "sequence_number": 1, "output_index": 0, "item": {**item, "status": "in_progress", "content": []}},
            {"type": "response.content_part.added", "sequence_number": 2, "item_id": item["id"], "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}},
            {"type": "response.output_text.delta", "sequence_number": 3, "item_id": item["id"], "output_index": 0, "content_index": 0, "delta": "OK"},
            {"type": "response.output_text.done", "sequence_number": 4, "item_id": item["id"], "output_index": 0, "content_index": 0, "text": "OK"},
            {"type": "response.content_part.done", "sequence_number": 5, "item_id": item["id"], "output_index": 0, "content_index": 0, "part": item["content"][0]},
            {"type": "response.output_item.done", "sequence_number": 6, "output_index": 0, "item": item},
        ]
    events.append({"type": "response.completed", "sequence_number": 7, "response": {**response, "status": "completed", "output": [item], "usage": {"input_tokens": token_estimate(b"x" * request_size), "input_tokens_details": {"cached_tokens": 0, "cache_write_tokens": 0}, "output_tokens": 1, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": token_estimate(b"x" * request_size) + 1}}})
    return b"".join(sse(event) for event in events)


def has_generated_delta(event: dict) -> bool:
    kind = event.get("type", "")
    if kind.endswith(".delta") and isinstance(event.get("delta"), str) and event["delta"]:
        return True
    for choice in event.get("choices", []):
        delta = choice.get("delta") or {}
        if delta.get("content") or delta.get("reasoning"):
            return True
        if any((call.get("function") or {}).get("arguments") for call in delta.get("tool_calls", [])):
            return True
    return False


class SseUsage:
    def __init__(self):
        self.buffer = b""
        self.usage: dict = {}
        self.first_token_ns: int | None = None

    def add(self, chunk: bytes) -> None:
        self.buffer = (self.buffer + chunk).replace(b"\r\n", b"\n")
        while b"\n\n" in self.buffer:
            frame, self.buffer = self.buffer.split(b"\n\n", 1)
            data = b"".join(line[5:].strip() for line in frame.splitlines() if line.startswith(b"data:"))
            if not data or data == b"[DONE]":
                continue
            try:
                event = json.loads(data)
            except (ValueError, UnicodeError):
                continue
            usage = event.get("usage") or (event.get("response") or {}).get("usage")
            if isinstance(usage, dict):
                self.usage = usage_fields(usage)
            if self.first_token_ns is None and has_generated_delta(event):
                self.first_token_ns = time.monotonic_ns()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: "BenchServer"

    def log_message(self, *_args: object) -> None:
        pass

    def target(self) -> tuple[str, str]:
        if self.path.startswith(("http://", "https://")):
            parsed = urlsplit(self.path)
            return parsed.hostname or "", parsed.path + ("?" + parsed.query if parsed.query else "")
        return self.server.upstream_host, self.path

    def do_GET(self) -> None:
        host, route = self.target()
        start = time.monotonic_ns()
        row = {"kind": "auxiliary", "method": "GET", "host": host, "path": urlsplit(route).path,
               "arrival_wall_ns": time.time_ns(), "request_wire_bytes": 0, "request_json_bytes": 0,
               "response_bytes": 0, "status": None, "usage": {}}
        try:
            if self.server.mode == "mock":
                payload = encoded({"models": [{"slug": self.server.model, "id": self.server.model, "context_window": 128000, "supported_reasoning_levels": [{"effort": "low"}], "default_reasoning_level": "low", "input_modalities": ["text"]}]})
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                row.update(status=200, response_bytes=len(payload))
            else:
                self.forward(host, route, None, row, start)
        except (OSError, http.client.HTTPException) as error:
            row.update(status=502, proxy_error_type=type(error).__name__)
            try:
                self.send_error(502)
            except OSError:
                pass
        finally:
            row["wall_ms"] = round((time.monotonic_ns() - start) / 1e6, 3)
            self.server.recorder.write(row)

    def do_POST(self) -> None:
        host, route = self.target()
        size = int(self.headers.get("Content-Length", "0"))
        if size < 0 or size > MAX_REQUEST:
            self.send_error(413)
            return
        wire = self.rfile.read(size)
        encoding = self.headers.get("Content-Encoding", "").lower()
        try:
            body = compression.zstd.decompress(wire) if encoding == "zstd" else gzip.decompress(wire) if encoding == "gzip" else wire
            parsed = json.loads(body)
        except (ValueError, UnicodeError):
            self.send_error(400)
            return
        index, prefix = self.server.recorder.reserve(body)
        start = time.monotonic_ns()
        row = {"kind": "model", "method": "POST", "index": index, "mode": self.server.mode, "host": host, "path": urlsplit(route).path,
               "arrival_wall_ns": time.time_ns(),
               "request_wire_bytes": len(wire), "request_json_bytes": len(body), "request_tokens_estimate": token_estimate(body),
               "request_sha256": hashlib.sha256(body).hexdigest(), "lcp_bytes_with_previous": prefix,
               "status": None, "response_bytes": 0, "first_byte_ms": None, "first_token_ms": None, "wall_ms": None,
               "usage": {}, **request_shape(parsed)}
        try:
            if self.server.mode == "mock":
                payload = mock_response(route, parsed.get("model", self.server.model), index, self.server.recorder, len(body))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)
                self.wfile.flush()
                events = SseUsage()
                events.add(payload)
                row.update(status=200, response_bytes=len(payload), first_byte_ms=0.0,
                           first_token_ms=round((events.first_token_ns - start) / 1e6, 3) if events.first_token_ns else None,
                           usage=events.usage)
            else:
                self.forward(host, route, body, row, start)
        except (OSError, http.client.HTTPException) as error:
            row["status"] = 502
            row["proxy_error_type"] = type(error).__name__
            try:
                self.send_error(502)
            except OSError:
                pass
        finally:
            row["wall_ms"] = round((time.monotonic_ns() - start) / 1e6, 3)
            self.server.recorder.write(row)

    def forward(self, host: str, route: str, body: bytes | None, row: dict | None = None, start: int | None = None) -> None:
        if host not in {"openrouter.ai", "chatgpt.com"}:
            self.send_error(403)
            return
        upstream = http.client.HTTPSConnection(host, timeout=120)
        incoming = self.server.incoming_base.rstrip("/")
        if incoming and route.startswith(incoming + "/"):
            route = self.server.upstream_base.rstrip("/") + route[len(incoming):]
        headers = {key: value for key, value in self.headers.items() if key.lower() not in HOP_HEADERS}
        headers["Accept-Encoding"] = "identity"
        try:
            upstream.request(self.command, route, body=body, headers=headers)
            response = upstream.getresponse()
            self.send_response(response.status)
            content_type = response.getheader("Content-Type", "application/octet-stream")
            self.send_header("Content-Type", content_type)
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            events = SseUsage()
            received = 0
            first_byte_ns = None
            while chunk := response.read1(65536):
                if first_byte_ns is None:
                    first_byte_ns = time.monotonic_ns()
                received += len(chunk)
                events.add(chunk)
                self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            if row is not None and start is not None:
                row.update(status=response.status, response_bytes=received,
                           first_byte_ms=round((first_byte_ns - start) / 1e6, 3) if first_byte_ns else None,
                           first_token_ms=round((events.first_token_ns - start) / 1e6, 3) if events.first_token_ns else None,
                           usage=events.usage)
        finally:
            upstream.close()


class BenchServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, port: int, recorder: Recorder, mode: str, model: str, upstream_host: str, incoming_base: str, upstream_base: str):
        super().__init__(("127.0.0.1", port), Handler)
        self.recorder = recorder
        self.mode = mode
        self.model = model
        self.upstream_host = upstream_host
        self.incoming_base = incoming_base
        self.upstream_base = upstream_base


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--mode", choices=["mock", "live"], required=True)
    parser.add_argument("--model", default="gpt-5.5")
    parser.add_argument("--scenario", default="reply")
    parser.add_argument("--steps", type=int, default=0)
    parser.add_argument("--command", default="true")
    parser.add_argument("--harness", required=True)
    parser.add_argument("--upstream-host", choices=["openrouter.ai", "chatgpt.com"], default="openrouter.ai")
    parser.add_argument("--incoming-base", default="/v1")
    parser.add_argument("--upstream-base", default="/api/v1")
    args = parser.parse_args()
    BenchServer(args.port, Recorder(args.out, args.scenario, args.steps, args.command, args.harness), args.mode,
                args.model, args.upstream_host, args.incoming_base, args.upstream_base).serve_forever()


if __name__ == "__main__":
    main()
