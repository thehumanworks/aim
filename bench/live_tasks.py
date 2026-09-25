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


# Scripting tasks (T4b, bench/plans/code-mode.md): fan-out over many files, then one written
# answer. Their tests are hidden from the workspace (the answer is in them) and grade the report
# the model writes; the fixture files are never copied to the grader.

def _source(name: str, todo: int, fixme: int) -> str:
    comment = "#" if name.endswith((".py", ".sh")) else "//"
    lines = [f"{comment} {name}: part of the sample service"]
    body = {
        ".py": ["def handler(event):", "    return {'ok': True, 'event': event}"],
        ".sh": ["set -eu", "echo \"starting\""],
        ".js": ["export function handler(event) {", "  return { ok: true, event };", "}"],
        ".rs": ["pub fn handler(event: &str) -> String {", "    event.to_owned()", "}"],
        ".go": ["package sample", "", "func Handler(event string) string {", "\treturn event", "}"],
    }[Path(name).suffix]
    notes = [f"{comment} TODO: handle case {index} before release" for index in range(todo)]
    notes += [f"{comment} FIXME: this breaks when input {index} is empty" for index in range(fixme)]
    # Interleave the notes with the code so no single read of the file's head finds them all.
    for index, note in enumerate(notes):
        lines.append(note)
        lines.append(body[index % len(body)] if index < len(body) else f"{comment} step {index}")
    lines.extend(body)
    return "\n".join(lines) + "\n"


TODO_COUNTS: dict[str, tuple[int, int]] = {
    "src/alpha.py": (3, 1), "src/beta.py": (0, 2), "src/gamma.js": (2, 0), "src/delta.js": (1, 1),
    "src/epsilon.rs": (4, 0), "src/zeta.rs": (0, 0), "src/eta.go": (1, 3), "src/theta.go": (2, 2),
    "src/iota.sh": (0, 1), "src/kappa.py": (5, 0),
}

CALLS: list[tuple[str, int, str]] = [
    ("app/cli.py", 6, "main"), ("app/server.py", 5, "start_server"), ("app/server.py", 10, "reload"),
    ("app/jobs/nightly.py", 5, "run_nightly"), ("app/worker.py", 7, "setup"),
]

HEADINGS: dict[str, str] = {
    "docs/README.md": "Project docs", "docs/setup.md": "Installing the tool", "docs/usage.md": "Command line usage",
    "docs/faq.md": "Frequently asked questions", "docs/changelog.md": "Changelog", "docs/api/index.md": "API overview",
    "docs/api/auth.md": "Authentication", "docs/api/errors.md": "Error codes", "docs/guides/deploy.md": "Deploying to production",
    "docs/guides/testing.md": "Writing tests",
}

TASKS["todo_table"] = {name: _source(Path(name).name, *counts) for name, counts in TODO_COUNTS.items()}
TASKS["callers"] = {
    "app/__init__.py": "",
    "app/config.py": "import json\n\n\ndef load_config(path):\n    with open(path) as stream:\n        return json.load(stream)\n\n\ndef load_config_file(path):\n    with open(path) as stream:\n        return stream.read()\n",
    "app/cli.py": "import sys\nfrom app.config import load_config\n\n\ndef main():\n    settings = load_config(sys.argv[1])\n    print(settings)\n",
    "app/server.py": "from app.config import load_config\n\n\ndef start_server(path):\n    settings = load_config(path)\n    return settings['port']\n\n\ndef reload(path):\n    return load_config(path)\n",
    "app/jobs/__init__.py": "",
    "app/jobs/nightly.py": "from app import config\n\n\ndef run_nightly():\n    settings = config.load_config('nightly.json')\n    return settings.get('jobs', [])\n",
    "app/worker.py": "from app.config import load_config\n\n\nclass Worker:\n    def setup(self, path):\n        self.path = path\n        self.settings = load_config(path)\n        return self\n",
    "app/legacy.py": "from app.config import load_config_file\n\n\ndef old_loader(path):\n    return load_config_file(path)\n",
    "app/helpers.py": "def describe():\n    \"\"\"Settings come from load_config(path); see app/config.py.\"\"\"\n    return 'settings'\n\n\ndef helper():\n    # load_config() is called by main, not here.\n    return describe()\n",
    "app/models.py": "class Settings:\n    def __init__(self, port):\n        self.port = port\n",
    "app/utils.py": "def merge(left, right):\n    merged = dict(left)\n    merged.update(right)\n    return merged\n",
}
TASKS["doc_index"] = {
    "docs/README.md": "# Project docs\n\nStart here.\n\n## Contents\n\nSee the other pages.\n",
    "docs/setup.md": "---\ntitle: Setup notes\n---\n\n# Installing the tool\n\nRun the installer.\n",
    "docs/usage.md": "The tool reads its input from standard input.\n\n## Command line usage\n\n    tool run FILE\n\n# Options\n",
    "docs/faq.md": "# Frequently asked questions\n\n## Is it fast?\n\nYes.\n",
    "docs/changelog.md": "# Changelog\n\n## 1.2.0\n\n- Faster startup.\n",
    "docs/api/index.md": "# API overview\n\nThe API is JSON over HTTP.\n",
    "docs/api/auth.md": "# Authentication\n\nSend a bearer token.\n",
    "docs/api/errors.md": "Errors use standard status codes.\n\n# Error codes\n\n## 4xx\n\nClient errors.\n",
    "docs/guides/deploy.md": "# Deploying to production\n\n## Checklist\n",
    "docs/guides/testing.md": "# Writing tests\n\nUse the test runner.\n",
    "docs/notes.txt": "# Not a Markdown file\n",
}

# Where each report may be written: the repository root, or the directory the prompt names (the
# smoke found "Write INDEX.md listing every Markdown file under docs/" answered in docs/INDEX.md).
# The first existing path is graded under the first name.
OUTPUTS: dict[str, tuple[str, ...]] = {"todo_table": ("REPORT.md", "src/REPORT.md"), "callers": ("CALLERS.md",),
                                       "doc_index": ("INDEX.md", "docs/INDEX.md")}

# The hidden tests read a report as exact entries (T4b cohort 2, bench/plans/code-mode.md): a path
# that names the right file, relative to the repository root (or to the directory the prompt
# names), and the right counts, function or heading for it. Every entry must be right and none may
# be missing, extra or listed twice, under any spelling. Markdown decoration (bullets, backticks,
# bold or italic stars, links, table pipes) is ignored.
#
# `_LINKS` is shared by the three tests. Where a link stands for a path, `[text](target)` becomes
# its text if that names a file, else its target if that does; elsewhere it becomes its text.
_LINKS = r"""
LINK = re.compile(r'\[([^\]]*)\]\(([^)\s]*)\)')
FILE = re.compile(r'\.\w+$')
def link_path(match):
    label, target = match.group(1).strip(), match.group(2).split('#', 1)[0]
    return label if FILE.search(label) else target if FILE.search(target) else label
def unlabel(text):
    return LINK.sub(lambda match: match.group(1), text)
def plain(text):
    return LINK.sub(link_path, text).replace('`', '').replace('*', '').strip()
"""

_REPORT_TEST = r"""import re, unittest
EXPECTED = {expected!r}
""" + _LINKS + r"""
def cells(line):
    return [plain(cell) for cell in line.strip().strip('|').split('|')]
def column(header, *words):
    return next((index for index, cell in enumerate(header) if any(word in cell.upper() for word in words)), None)
class TestTask(unittest.TestCase):
    def test_counts(self):
        # A table row may omit its outer pipes (GitHub-flavoured Markdown).
        rows = [cells(line) for line in open('REPORT.md', encoding='utf-8').read().splitlines() if '|' in line]
        header = next((row for row in rows if column(row, 'TODO') is not None), None)
        self.assertIsNotNone(header, 'a table with a TODO column')
        todo, fixme = column(header, 'TODO'), column(header, 'FIXME')
        self.assertIsNotNone(fixme, 'a FIXME column')
        # The file column by its header; else the one column that holds no count.
        rest = [index for index in range(len(header)) if index not in (todo, fixme)]
        name_column = column(header, 'FILE', 'PATH', 'NAME')
        name_column = name_column if name_column is not None else (rest[0] if len(rest) == 1 else None)
        self.assertIsNotNone(name_column, 'a file column')
        found = {{}}
        for row in rows:
            if row is header or all(set(cell) <= set('-: ') for cell in row):
                continue
            name = row[name_column].removeprefix('./') if name_column < len(row) else ''
            if not FILE.search(name):
                continue  # a total or note row names no file
            self.assertEqual(len(row), len(header), 'a row with the header\'s columns: ' + ' | '.join(row))
            path = name if name.startswith('src/') else 'src/' + name  # relative to src/, as the prompt scopes it
            self.assertIn(path, EXPECTED, 'not a file under src/: ' + row[name_column])
            self.assertNotIn(path, found, 'listed twice: ' + path)
            self.assertTrue(row[todo].isdigit() and row[fixme].isdigit(), 'counts of ' + path)
            found[path] = (int(row[todo]), int(row[fixme]))
        self.assertEqual(found, EXPECTED)
"""

_CALLERS_TEST = r"""import re, unittest
EXPECTED = {expected!r}
QUALIFIED = {{('app/worker.py', 7): 'Worker'}}  # a method may be named with its class
ENTRY = re.compile(r'(?:\./)?(?P<path>[\w/-]+(?:\.[\w-]+)*\.py):(?P<line>\d+)\s*[:,\-–—]?\s*(?P<name>[A-Za-z_][\w.]*)(?:\(\))?')
""" + _LINKS + r"""
class TestTask(unittest.TestCase):
    def test_callers(self):
        found = {{}}
        for text in open('CALLERS.md', encoding='utf-8').read().splitlines():
            # A link's text carries path:line; its target (app/cli.py#L6) does not.
            text = unlabel(text)
            if not re.search(r'\.py:\d+', text):
                continue  # a heading or a note
            entry = re.sub(r'^\s*(?:[-+]|\d+[.)])\s+', '', plain(text).replace('|', ' ')).strip()
            entry = ENTRY.fullmatch(entry)
            self.assertIsNotNone(entry, 'not "path:line function_name" with a path relative to the root: ' + text)
            path, line, name = entry['path'], int(entry['line']), entry['name']
            owner = QUALIFIED.get((path, line))
            if owner and name.startswith(owner + '.'):
                name = name[len(owner) + 1:]
            self.assertNotIn((path, line), found, 'listed twice: {{}}:{{}}'.format(path, line))
            found[(path, line)] = name
        self.assertEqual({{(path, line, name) for (path, line), name in found.items()}}, set(EXPECTED))
"""

_INDEX_TEST = r"""import re, unittest
EXPECTED = {expected!r}
""" + _LINKS + r"""
class TestTask(unittest.TestCase):
    def test_index(self):
        found = {{}}
        for text in open('INDEX.md', encoding='utf-8').read().splitlines():
            bullet = re.match(r'^\s*(?:[-*+]|\d+[.)])\s+(.*)$', text)
            if not bullet:
                continue  # the format is "- path: heading" (any list marker); other lines are titles or notes
            # A leading link stands for the path; any other link is heading text.
            entry = bullet.group(1).replace('`', '').replace('*', '').strip()
            lead = LINK.match(entry)
            entry = unlabel(link_path(lead) + entry[lead.end():] if lead else entry)
            path, separator, heading = entry.partition(':')
            self.assertTrue(separator, 'not "- path: heading": ' + text)
            path = path.strip().removeprefix('./')
            path = path if path.startswith('docs/') else 'docs/' + path  # relative to docs/, as the prompt scopes it
            if path == 'docs/INDEX.md':
                continue  # the index itself
            self.assertIn(path, EXPECTED, 'not a Markdown file under docs/: ' + text)
            self.assertNotIn(path, found, 'listed twice: ' + path)
            found[path] = heading.strip().lstrip('#').strip().strip('"\'_').strip()
        self.assertEqual({{path: heading.lower() for path, heading in found.items()}},
                         {{path: heading.lower() for path, heading in EXPECTED.items()}})
"""

HIDDEN_TESTS: dict[str, str] = {
    "todo_table": _REPORT_TEST.format(expected=TODO_COUNTS),
    "callers": _CALLERS_TEST.format(expected=CALLS),
    "doc_index": _INDEX_TEST.format(expected=HEADINGS),
}

def prepare(task_id: str, workspace: Path) -> None:
    """Create the same fresh fixture for each harness/repetition."""
    if task_id not in TASKS:
        raise ValueError(f"unknown task {task_id}")
    workspace.mkdir(parents=True)
    for name, content in TASKS[task_id].items():
        path = workspace / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
    subprocess.run(["git", "init", "-q", str(workspace)], check=True)


def graded_files(task_id: str) -> list[str]:
    """The workspace files the grader copies: a scripting task's report, else the edited modules."""
    return list(OUTPUTS.get(task_id) or (name for name in TASKS[task_id] if name != "test_task.py"))


def grade(task_id: str, workspace: Path) -> bool:
    """Run the task's independent tests, without trusting a harness's own summary."""
    with tempfile.TemporaryDirectory(prefix="aim-hidden-grader-") as temporary:
        hidden = Path(temporary)
        home = hidden / "home"
        home.mkdir()
        def usable(source: Path) -> bool:
            return not source.is_symlink() and source.is_file() and source.stat().st_size <= 1_000_000
        if task_id in OUTPUTS:
            reports = [workspace / name for name in OUTPUTS[task_id] if usable(workspace / name)]
            if not reports:
                return False
            (hidden / OUTPUTS[task_id][0]).write_bytes(reports[0].read_bytes())
        for name in [] if task_id in OUTPUTS else graded_files(task_id):
            source = workspace / name
            if not usable(source):
                return False
            (hidden / name).write_bytes(source.read_bytes())
        test = HIDDEN_TESTS.get(task_id) or TASKS[task_id]["test_task.py"]
        (hidden / "test_task.py").write_text(test, encoding="utf-8")
        env = {"HOME": str(home), "TMPDIR": str(home), "PYTHONDONTWRITEBYTECODE": "1", "LANG": "C.UTF-8"}
        try:
            result = subprocess.run([sys.executable, "-I", "-B", "-m", "unittest", "discover", "-s", str(hidden), "-q"],
                                    cwd=hidden, env=env, capture_output=True, timeout=30)
        except (OSError, subprocess.TimeoutExpired):
            return False
        return result.returncode == 0
