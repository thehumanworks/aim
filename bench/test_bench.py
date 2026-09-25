"""Checks the measurement boundary and independent task baselines."""

import tempfile
import unittest
from unittest.mock import patch
import http.client
import os
import socket
import subprocess
import sys
from pathlib import Path

from live_tasks import TASKS, grade, prepare
from proxy import SseUsage, has_generated_delta, request_shape, usage_fields
from run import PROXY, free_port, summary, wait_port
from port import fixed_port, require_fixed_port


class RecorderTests(unittest.TestCase):
    def test_gate_fixed_port_is_validated_and_shared_with_proxy(self):
        with patch.dict(os.environ, {"AIM_GATE_BENCH_PORT": "43117"}):
            self.assertEqual(free_port(), 43117)
            require_fixed_port(43117, os.environ["AIM_GATE_BENCH_PORT"])
            with self.assertRaises(ValueError):
                require_fixed_port(43118, os.environ["AIM_GATE_BENCH_PORT"])
        for invalid in ("", "0", "65536", "-1", "1.5", " 43117", "４３１１７"):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                fixed_port(invalid)
        self.assertIsNone(fixed_port(None))

    def test_occupied_fixed_port_never_accepts_another_server_as_recorder(self):
        with socket.socket() as occupied, tempfile.TemporaryDirectory() as temp:
            occupied.bind(("127.0.0.1", 0))
            occupied.listen()
            port = occupied.getsockname()[1]
            ready = Path(temp) / "proxy.ready"
            with patch.dict(os.environ, {"AIM_GATE_BENCH_PORT": str(port)}):
                proxy = subprocess.Popen(
                    [sys.executable, "-B", str(PROXY), "--port", str(port), "--out", str(Path(temp) / "rows.jsonl"),
                     "--ready-file", str(ready), "--mode", "mock", "--harness", "aim_openrouter"],
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                )
                try:
                    with self.assertRaises(RuntimeError):
                        wait_port(port, proxy, ready)
                    self.assertFalse(ready.exists())
                finally:
                    proxy.wait(timeout=3)

    def test_fixed_port_proxy_listens_and_answers_on_that_port(self):
        with socket.socket() as selector:
            selector.bind(("127.0.0.1", 0))
            port = selector.getsockname()[1]
        with tempfile.TemporaryDirectory() as temp, patch.dict(os.environ, {"AIM_GATE_BENCH_PORT": str(port)}):
            ready = Path(temp) / "proxy.ready"
            proxy = subprocess.Popen(
                [sys.executable, "-B", str(PROXY), "--port", str(port), "--out", str(Path(temp) / "rows.jsonl"),
                 "--ready-file", str(ready), "--mode", "mock", "--harness", "aim_openrouter"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            )
            try:
                wait_port(port, proxy, ready)
                connection = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
                connection.request("GET", "/v1/models")
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                response.read()
                connection.close()
            finally:
                proxy.terminate()
                proxy.wait(timeout=3)

    def test_chat_usage_is_read_from_provider_fields(self):
        usage = usage_fields({"prompt_tokens": 100, "completion_tokens": 8,
                              "prompt_tokens_details": {"cached_tokens": 40, "cache_write_tokens": 5},
                              "completion_tokens_details": {"reasoning_tokens": 3}, "cost": 0.001})
        self.assertEqual(usage, {"input_tokens": 100, "cached_tokens": 40, "cache_write_tokens": 5,
                                 "output_tokens": 8, "reasoning_tokens": 3, "cost_usd": 0.001})

    def test_responses_usage_and_first_token_across_chunks(self):
        collector = SseUsage()
        collector.add(b'data: {"type":"response.output_text.delta","delta":"O"}\n\n')
        collector.add(b'data: {"type":"response.completed","response":{"usage":{"input_tokens":21,')
        collector.add(b'"input_tokens_details":{"cached_tokens":4},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":1}}}}\n\n')
        self.assertIsNotNone(collector.first_token_ns)
        self.assertEqual(collector.usage["input_tokens"], 21)
        self.assertEqual(collector.usage["cached_tokens"], 4)
        self.assertEqual(collector.usage["reasoning_tokens"], 1)

    def test_tool_arguments_are_generated_tokens(self):
        self.assertTrue(has_generated_delta({"type": "response.function_call_arguments.delta", "delta": '{"cmd"'}))
        self.assertTrue(has_generated_delta({"choices": [{"delta": {"tool_calls": [{"function": {"arguments": "echo"}}]}}]}))
        self.assertFalse(has_generated_delta({"type": "response.output_item.added", "item": {"type": "function_call"}}))

    def test_tool_output_measure_does_not_return_body(self):
        shape = request_shape({"tools": [{"name": "Bash"}], "messages": [
            {"role": "system", "content": "hidden"}, {"role": "tool", "content": "secret full output handle"}]})
        self.assertEqual(shape["tool_output_chars"], len("secret full output handle"))
        self.assertTrue(shape["tool_output_has_recovery_hint"])
        self.assertNotIn("secret", str(shape))


class LiveTaskTests(unittest.TestCase):
    def test_all_fixtures_fail_the_independent_grader_before_an_edit(self):
        for task in TASKS:
            with self.subTest(task=task), tempfile.TemporaryDirectory() as temp:
                workspace = Path(temp) / "work"
                prepare(task, workspace)
                self.assertFalse(grade(task, workspace))

    def test_grader_ignores_editable_visible_tests(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = Path(temp) / "work"
            prepare("clamp", workspace)
            (workspace / "test_task.py").write_text("", encoding="utf-8")
            self.assertFalse(grade("clamp", workspace))
            (workspace / "mathutil.py").write_text("def clamp(value, low, high):\n    return max(low, min(high, value))\n")
            self.assertTrue(grade("clamp", workspace))

    def test_cost_per_pass_includes_failed_attempts_and_missing_cost_stays_unknown(self):
        runs = [
            {"harness": "aim", "passed": True, "ite": 100, "usage": {"cost_usd": 0.01}, "provider_cost_complete": True, "wall_ms": 1},
            {"harness": "aim", "passed": False, "ite": 300, "usage": {"cost_usd": 0.02}, "provider_cost_complete": True, "wall_ms": 2},
        ]
        measured = summary(runs, ["aim"])[0]
        self.assertEqual(measured["ite_per_passed"], 400)
        self.assertEqual(measured["usd_per_passed"], 0.03)
        runs[1]["provider_cost_complete"] = False
        self.assertIsNone(summary(runs, ["aim"])[0]["usd_per_passed"])


if __name__ == "__main__":
    unittest.main()
