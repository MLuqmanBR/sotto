#!/usr/bin/env python3
"""Check the public README before it is published.

Three things have broken here before, and each is cheap to catch:

1. An internal link (`#section`) pointing at a heading that does not exist. The
   badge row linked to `#requirements` for a while with no such heading.
2. A referenced asset that is not in the tree — the banner image, the signing
   key, the licence.
3. The banner disappearing from the top, which is the first thing a visitor sees.

Run with no arguments to check `public/README.md`.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def find_readme() -> Path:
    """The README to check, in either checkout shape.

    In the development tree the publishable files live under `public/`. In the
    public repository that directory *is* the root, and `public/README.md` does
    not exist. Choosing the first that exists keeps one script working for both
    instead of failing contributors with a path they never had.
    """
    for candidate in (ROOT / "public" / "README.md", ROOT / "README.md"):
        if candidate.is_file():
            return candidate
    return ROOT / "public" / "README.md"


README = find_readme()

failures = 0


def ok(name):
    print(f"  ok   {name}")


def bad(name, detail=""):
    global failures
    failures += 1
    print(f"  FAIL {name}" + (f" — {detail}" if detail else ""))


if not README.is_file():
    print(f"  FAIL no README at {README}")
    sys.exit(1)

src = README.read_text(encoding="utf-8")
public = README.parent


def anchor(heading: str) -> str:
    a = heading.lower()
    a = re.sub(r"[^\w\s-]", "", a)
    return re.sub(r"\s+", "-", a.strip())


headings = re.findall(r"^#{2,3} (.+)$", src, re.M)
have = {anchor(h) for h in headings}
links = set(re.findall(r"\]\(#([\w-]+)\)", src))
broken = sorted(l for l in links if l not in have)
if broken:
    bad("every internal link resolves", "missing: " + ", ".join("#" + b for b in broken))
else:
    ok(f"every internal link resolves ({len(links)} links, {len(headings)} headings)")

# Referenced local files must exist in the public tree.
missing = []
for ref in re.findall(r'(?:src|href)="([^"#:]+)"', src) + re.findall(r"\]\(([^)#:]+)\)", src):
    if ref.startswith(("http", "//", "mailto:")):
        continue
    if not (public / ref).exists():
        missing.append(ref)
if missing:
    bad("every referenced file exists", ", ".join(sorted(set(missing))))
else:
    ok("every referenced file exists")

# The banner must be the first content, centred, and the icon must be present.
top = src[: src.find("---")]
if ".github/banner.png" in top:
    ok("the banner is at the very top")
else:
    bad("the banner is at the very top")

if '<div align="center">' in top:
    ok("the banner block is centre-aligned")
else:
    bad("the banner block is centre-aligned")

banner = public / ".github" / "banner.png"
if banner.is_file() and banner.stat().st_size > 10_000:
    ok(f"the banner image is present ({banner.stat().st_size // 1024} KB)")
else:
    bad("the banner image is present")

# A release-facing README must not leak internal project material.
leaks = [p for p in ("BUILD-PLAN", "progress/", "plan/", "research/", "archive/") if p in src]
if leaks:
    bad("no internal project references", ", ".join(leaks))
else:
    ok("no internal project references")

print()
if failures:
    print(f"  {failures} README check(s) failed.")
    sys.exit(1)
print("  All README checks passed.")