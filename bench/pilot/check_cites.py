#!/usr/bin/env python3
"""Check that every `path:line` citation in the report resolves inside refs/.

Section headers decide the default repo for relative paths.
"""
import re, sys
from pathlib import Path

REFS = Path(__file__).resolve().parent.parent / "refs"
REPORT = Path(sys.argv[1])
SECTION_REPO = [("### F1", "tny"), ("### F2", "codex"), ("### F3", "pi"), ("### F4", "oh-my-pi"),
                ("### F5", "unreal-agent"), ("### F6", None), ("### F7", "tny"), ("### F8", None), ("## Implications", None)]
repo = None
bad, ok = [], 0
for n, line in enumerate(REPORT.read_text().splitlines(), 1):
    for prefix, r in SECTION_REPO:
        if line.startswith(prefix):
            repo = r
    for m in re.finditer(r"`([A-Za-z0-9_./\-…]+?\.[A-Za-z0-9]+)(?::([0-9,\- ]+))?`", line):
        path, lines = m.group(1), m.group(2)
        if "…" in path or path.startswith("http"):
            continue
        if path.startswith("refs/"):
            full = REFS / path[len("refs/"):]
        elif repo:
            full = REFS / repo / path
        else:
            continue
        if not full.exists():
            # skip things that are clearly not paths (e.g. mise.toml, SKILL.md mentions)
            if "/" in path:
                bad.append((n, path, "missing"))
            continue
        if lines:
            count = sum(1 for _ in full.open(errors="ignore"))
            nums = [int(x) for x in re.findall(r"\d+", lines)]
            if nums and max(nums) > count:
                bad.append((n, path, f"line {max(nums)} > {count}"))
                continue
        ok += 1
print("ok", ok)
for b in bad:
    print("BAD", *b)
