"""Move Trunk's generated inline module to a same-origin file for a strict CSP."""

from pathlib import Path
import re


def main() -> None:
    dist = Path(__file__).resolve().parent / "dist"
    index = dist / "index.html"
    html = index.read_text(encoding="utf-8")
    pattern = re.compile(r'<script type="module">\s*(.*?)\s*</script>', re.DOTALL)
    matches = list(pattern.finditer(html))
    if len(matches) != 1:
        raise SystemExit("Expected exactly one Trunk bootstrap module")
    (dist / "bootstrap.mjs").write_text(matches[0].group(1) + "\n", encoding="utf-8")
    html = pattern.sub('<script type="module" src="/bootstrap.mjs"></script>', html, count=1)
    if re.search(r"<script\b(?![^>]*\bsrc=)", html):
        raise SystemExit("Generated page still contains an inline script")
    index.write_text(html, encoding="utf-8")


if __name__ == "__main__":
    main()
