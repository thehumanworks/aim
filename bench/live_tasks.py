"""Small independent coding graders for the bounded live benchmark."""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
from pathlib import Path


TASKS: dict[str, dict[str, str]] = {
    "even_sum": {
        "calc.py": "def sum_even(values):\n    return sum(value for value in values if value % 2)\n",
        "test_task.py": "import unittest\nfrom calc import sum_even\nclass TestTask(unittest.TestCase):\n    def test_mixed(self): self.assertEqual(sum_even([1, 2, 3, 4]), 6)\n    def test_negative(self): self.assertEqual(sum_even([-4, -3, 0, 8]), 4)\n    def test_empty(self): self.assertEqual(sum_even([]), 0)\n",
    },
    "clamp": {
        "mathutil.py": "def clamp(value, low, high):\n    raise NotImplementedError\n",
        "test_task.py": "import unittest\nfrom mathutil import clamp\nclass TestTask(unittest.TestCase):\n    def test_middle(self): self.assertEqual(clamp(5, 0, 10), 5)\n    def test_lower(self): self.assertEqual(clamp(-2, 0, 10), 0)\n    def test_upper(self): self.assertEqual(clamp(12, 0, 10), 10)\n    def test_equal(self): self.assertEqual(clamp(2, 2, 2), 2)\n",
    },
    "slug": {
        "strings.py": "def slugify(text):\n    return text.replace(' ', '-')\n",
        "test_task.py": "import unittest\nfrom strings import slugify\nclass TestTask(unittest.TestCase):\n    def test_words(self): self.assertEqual(slugify('Hello World'), 'hello-world')\n    def test_spaces(self): self.assertEqual(slugify('  two   words  '), 'two-words')\n    def test_empty(self): self.assertEqual(slugify('   '), '')\n",
    },
    "retry": {
        "retry.py": "def retry(operation, attempts):\n    for _ in range(attempts + 1):\n        try:\n            return operation()\n        except ValueError:\n            pass\n    raise ValueError('failed')\n",
        "test_task.py": "import unittest\nfrom retry import retry\nclass TestTask(unittest.TestCase):\n    def test_success_second(self):\n        calls = []\n        def operation():\n            calls.append(1)\n            if len(calls) < 2: raise ValueError('again')\n            return 9\n        self.assertEqual(retry(operation, 2), 9)\n        self.assertEqual(len(calls), 2)\n    def test_exact_limit(self):\n        calls = []\n        def operation(): calls.append(1); raise ValueError('again')\n        with self.assertRaises(ValueError): retry(operation, 3)\n        self.assertEqual(len(calls), 3)\n",
    },
    "ledger": {
        "ledger.py": "def total_cents(rows):\n    return sum(int(amount) for amount in rows)\n",
        "test_task.py": "import unittest\nfrom ledger import total_cents\nclass TestTask(unittest.TestCase):\n    def test_decimal_prices(self): self.assertEqual(total_cents(['1.25', '0.30']), 155)\n    def test_rounding(self): self.assertEqual(total_cents(['0.10', '0.20']), 30)\n    def test_empty(self): self.assertEqual(total_cents([]), 0)\n",
    },
    "normalize": {
        "normalize.py": "def unique_sorted(words):\n    return sorted(set(words))\n",
        "test_task.py": "import unittest\nfrom normalize import unique_sorted\nclass TestTask(unittest.TestCase):\n    def test_case(self): self.assertEqual(unique_sorted(['Beta', 'alpha', 'ALPHA']), ['alpha', 'beta'])\n    def test_spaces(self): self.assertEqual(unique_sorted(['  Pear ', 'pear', 'apple']), ['apple', 'pear'])\n    def test_empty(self): self.assertEqual(unique_sorted([]), [])\n",
    },
}


def prepare(task_id: str, workspace: Path) -> None:
    """Create the same fresh fixture for each harness/repetition."""
    if task_id not in TASKS:
        raise ValueError(f"unknown task {task_id}")
    workspace.mkdir(parents=True)
    for name, content in TASKS[task_id].items():
        (workspace / name).write_text(content, encoding="utf-8")
    subprocess.run(["git", "init", "-q", str(workspace)], check=True)


def grade(task_id: str, workspace: Path) -> bool:
    """Run the task's independent tests, without trusting a harness's own summary."""
    with tempfile.TemporaryDirectory(prefix="aim-hidden-grader-") as temporary:
        hidden = Path(temporary)
        home = hidden / "home"
        home.mkdir()
        for name in TASKS[task_id]:
            if name == "test_task.py":
                continue
            source = workspace / name
            if source.is_symlink() or not source.is_file() or source.stat().st_size > 1_000_000:
                return False
            (hidden / name).write_bytes(source.read_bytes())
        (hidden / "test_task.py").write_text(TASKS[task_id]["test_task.py"], encoding="utf-8")
        env = {"HOME": str(home), "TMPDIR": str(home), "PYTHONDONTWRITEBYTECODE": "1", "LANG": "C.UTF-8"}
        try:
            result = subprocess.run([sys.executable, "-I", "-B", "-m", "unittest", "discover", "-s", str(hidden), "-q"],
                                    cwd=hidden, env=env, capture_output=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired):
            return False
        return result.returncode == 0
