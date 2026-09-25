"""Checks the measurement boundary and independent task baselines."""

import copy
import json
import os
import subprocess
import tempfile
import time
import tomllib
import unittest
from unittest import mock
from types import SimpleNamespace
from pathlib import Path

from live_tasks import TASKS, grade, prepare
from compare import compare_wire
from proxy import BenchServer, Handler, Recorder, SseUsage, append_only, has_generated_delta, mock_response, render_order, request_shape, usage_fields
from run import TRIAL_ROOT, invocation, isolated_env, path_map, split_arm, summary, temporary_workspace, update_diagnostics


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
            {"role": "system", "content": "hidden"}, {"role": "tool", "content": "secret full output handle h (BashOutput/exec.read)"}]})
        self.assertEqual(shape["tool_output_chars"], len("secret full output handle h (BashOutput/exec.read)"))
        self.assertTrue(shape["tool_output_has_recovery_hint"])
        self.assertNotIn("secret", str(shape))

    def test_mock_scripts_a_cell_when_only_the_code_tool_is_offered(self):
        recorder = SimpleNamespace(scenario="big_output", steps=1, command="seq 1 3")
        def call(offered):
            payload = mock_response("/v1/chat/completions", "m", 1, recorder, 10, offered)
            first = json.loads(payload.split(b"\n\n")[0][len(b"data: "):])
            return first["choices"][0]["delta"]["tool_calls"][0]["function"]
        self.assertEqual(call({"Bash", "run_code"})["name"], "Bash", "a direct Bash is used when offered")
        self.assertEqual(call(None)["name"], "Bash")
        cell = call({"run_code", "list_programs"})
        self.assertEqual(cell["name"], "run_code")
        self.assertIn('tools.Bash({"command": "seq 1 3"})', json.loads(cell["arguments"])["code"])

    def test_render_order_recognizes_append_only_despite_json_key_order(self):
        first = {"messages": [{"role": "system", "content": "stable"}, {"role": "user", "content": "one"}],
                 "tools": [{"name": "Bash"}], "model": "m"}
        second = {**first, "messages": [*first["messages"], {"role": "assistant", "content": "two"}]}
        self.assertTrue(append_only(first, second))
        self.assertTrue(render_order(second).startswith(render_order(first)))
        changed = {**second, "tools": [{"name": "Read"}]}
        self.assertFalse(append_only(first, changed))
        self.assertFalse(request_shape({"messages": [{"role": "tool", "content": "Warning: truncated output"}]})[
            "tool_output_has_recovery_hint"])

    def test_proxy_keeps_usage_when_client_disconnects_after_completion(self):
        class Response:
            status = 200
            chunks = [b'data: {"usage":{"prompt_tokens":10,"completion_tokens":2,"cost":0.001}}\n\n', b'']

            def getheader(self, _name, default):
                return default

            def read1(self, _size):
                return self.chunks.pop(0)

        class Connection:
            def request(self, *_args, **_kwargs):
                pass

            def getresponse(self):
                return Response()

            def close(self):
                pass

        class ClosedWriter:
            def write(self, _data):
                raise BrokenPipeError

        client = SimpleNamespace(
            server=SimpleNamespace(incoming_base="/v1", upstream_base="/api/v1"),
            headers={}, command="POST", wfile=ClosedWriter(),
            send_response=lambda _status: None, send_header=lambda *_args: None, end_headers=lambda: None,
        )
        row = {}
        with mock.patch("proxy.http.client.HTTPSConnection", return_value=Connection()):
            Handler.forward(client, "openrouter.ai", "/v1/chat/completions", b"{}", row, time.monotonic_ns())
        self.assertEqual(row["status"], 200)
        self.assertTrue(row["client_disconnected"])
        self.assertEqual(row["usage"]["input_tokens"], 10)
        self.assertEqual(row["usage"]["cost_usd"], 0.001)
        self.assertGreater(row["response_bytes"], 0)

    def test_wire_gate_rejects_failed_trajectory_and_byte_growth(self):
        bench = Path(__file__).resolve().parent
        baseline = json.loads((bench / "results/w26-main-baseline.json").read_text())
        candidate = json.loads((bench / "results/w26-after-wire.json").read_text())
        with (bench / "manifest.toml").open("rb") as stream:
            manifest = tomllib.load(stream)
        self.assertEqual(compare_wire(candidate, baseline, manifest), [])
        broken = copy.deepcopy(candidate)
        broken["runs"][0]["passed"] = False
        broken["runs"][0]["first_request_bytes"] += 10_000
        errors = compare_wire(broken, baseline, manifest)
        self.assertTrue(any("trajectory failed" in error for error in errors))
        self.assertTrue(any("first request grew" in error for error in errors))

    def test_proxy_reserves_spend_before_each_request_and_settles_usage(self):
        with tempfile.TemporaryDirectory() as temp:
            server = BenchServer(0, Recorder(Path(temp) / "requests.jsonl", "reply", 0, "true", "aim_openrouter"),
                                 "mock", "m", "openrouter.ai", "/v1", "/api/v1", 1.0, 0.6)
            try:
                self.assertTrue(server.admit_spend())
                self.assertFalse(server.admit_spend(), "a concurrent request cannot spend the same reserve")
                server.settle_spend({"cost_usd": 0.1})
                self.assertTrue(server.admit_spend())
                server.settle_spend({"cost_usd": 0.5})
                self.assertFalse(server.admit_spend(), "the run stops when the remaining cap cannot cover one request")
            finally:
                server.server_close()


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

    def test_grader_ignores_workspace_import_hooks_and_credentials(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = Path(temp) / "work"
            prepare("clamp", workspace)
            (workspace / "sitecustomize.py").write_text("import os; os._exit(0)\n")
            (workspace / "unittest.py").write_text("raise RuntimeError('shadowed')\n")
            (workspace / "mathutil.py").write_text(
                "import os\n"
                "if os.getenv('OPENROUTER_API_KEY'): raise RuntimeError('credential leaked')\n"
                "def clamp(value, low, high): return max(low, min(high, value))\n"
            )
            with mock.patch.dict(os.environ, {"OPENROUTER_API_KEY": "fixture-secret"}):
                self.assertTrue(grade("clamp", workspace))

    def test_grader_timeout_fails_one_trial(self):
        with tempfile.TemporaryDirectory() as temp:
            workspace = Path(temp) / "work"
            prepare("clamp", workspace)
            with mock.patch("live_tasks.subprocess.run", side_effect=subprocess.TimeoutExpired("grader", 30)):
                self.assertFalse(grade("clamp", workspace))

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

    def test_binary_paths_use_cargo_metadata_target_directory(self):
        metadata = SimpleNamespace(stdout=json.dumps({"target_directory": "/tmp/isolated-target"}))
        with mock.patch("run.subprocess.run", return_value=metadata), mock.patch("run.pinned", return_value=Path("/tmp/pinned")):
            paths = path_map(SimpleNamespace(omp=None, unreal=None))
        self.assertEqual(paths["aim"], Path("/tmp/isolated-target/debug/aim"))
        self.assertEqual(paths["aimx"], Path("/tmp/isolated-target/debug/aimx"))

    def test_diagnostics_keep_counts_without_model_or_argument_text(self):
        lines = b'\n'.join((
            b'{"type":"tool_started","name":"Bash","arguments":"secret-value"}',
            b'{"type":"tool_finished","name":"Bash","result":{"is_error":true,"content":"secret-value"}}',
            b'{"type":"turn_failed","message":"turn exceeded 24 model requests"}',
        ))
        diagnostic = update_diagnostics(lines)
        self.assertEqual(diagnostic["tool_calls_by_name"], {"Bash": 1})
        self.assertEqual((diagnostic["direct_tool_calls"], diagnostic["nested_tool_calls"]), (1, 0))
        self.assertEqual(diagnostic["failed_tools_by_name"], {"Bash": 1})
        self.assertEqual(diagnostic["turn_failure_class"], "max_requests")
        self.assertNotIn("secret-value", str(diagnostic))

    def test_diagnostics_separate_the_models_calls_from_a_cells_nested_calls(self):
        lines = b'\n'.join((
            b'{"type":"tool_started","call_id":"c1","name":"run_code","arguments":"{}"}',
            b'{"type":"tool_started","call_id":"n1","name":"Read","arguments":"{}","parent":"c1"}',
            b'{"type":"tool_started","call_id":"n2","name":"Grep","arguments":"{}","parent":"c1"}',
            b'{"type":"tool_started","call_id":"c2","name":"Bash","arguments":"{}"}',
        ))
        diagnostic = update_diagnostics(lines)
        self.assertEqual(diagnostic["tool_calls_by_name"], {"run_code": 1, "Read": 1, "Grep": 1, "Bash": 1})
        self.assertEqual(diagnostic["nested_tool_calls_by_name"], {"Read": 1, "Grep": 1})
        self.assertEqual((diagnostic["direct_tool_calls"], diagnostic["nested_tool_calls"]), (2, 2))

    def test_trial_paths_do_not_depend_on_the_callers_tmpdir(self):
        with mock.patch.dict("run.BASE_ENV", {"PATH": "/bin", "TMPDIR": "/var/folders/xx/long-caller-temp/T/"}, clear=True):
            env = isolated_env(Path("/h"))
            with temporary_workspace() as root:
                self.assertEqual(root.parent, TRIAL_ROOT)
        self.assertEqual(env["TMPDIR"], str(TRIAL_ROOT), "harnesses name their temp files in model-visible text")

    def test_aim_arms_select_aim_code_mode(self):
        self.assertEqual(split_arm("aim_openrouter@only"), ("aim_openrouter", "only"))
        self.assertEqual(split_arm("codex"), ("codex", None))
        for bad in ("codex@only", "aim_openrouter@sometimes"):
            with self.assertRaises(ValueError):
                split_arm(bad)
        paths = {name: Path(f"/bin/{name}") for name in ("aim", "aimx", "aim_coderun", "python")}
        def env_of(harness, base_env):
            with mock.patch.dict("run.BASE_ENV", base_env, clear=True):
                env, command = invocation(harness, paths, Path("/h"), "http://127.0.0.1:1", "m", "hi", "mock", None, Path("/w"))
            self.assertEqual(command[0], "/bin/aim")
            self.assertEqual(env["AIM_CODERUN"], "/bin/aim_coderun", "the worker is always explicit")
            return env.get("AIM_CODE_MODE")
        self.assertEqual(env_of("aim_openrouter@only", {"AIM_CODE_MODE": "off"}), "only", "the arm wins")
        self.assertEqual(env_of("aim_openrouter", {"AIM_CODE_MODE": "on"}), "on")
        self.assertEqual(env_of("aim_openrouter", {"AIM_BENCH_CODE_MODE": "off"}), "off", "the legacy switch still works")
        self.assertIsNone(env_of("aim_openrouter", {}), "unset keeps aim's default")


if __name__ == "__main__":
    unittest.main()
