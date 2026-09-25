"""Checks the measurement boundary and independent task baselines."""

import tempfile
import unittest
from pathlib import Path

from live_tasks import TASKS, grade, prepare
from proxy import SseUsage, has_generated_delta, request_shape, usage_fields
from run import summary


class RecorderTests(unittest.TestCase):
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
