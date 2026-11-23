#!/usr/bin/env python3
"""Check that the command surface and the UI agree.

This exists because of a real failure. The app once passed every test while
roughly a dozen backend features had no UI entry point at all: retention never
ran, favourites could not be set, playback did not exist. A green suite proved
what the tests called, not what a user could reach.

Two rules, both cheap to check and both able to catch that class of bug:

1. Every command the UI invokes must be registered in `generate_handler!`.
   Otherwise the button is dead and only a click would reveal it.
2. Every registered command must have a caller — the UI, or Rust itself. An
   unreachable command is either forgotten wiring or leftover code; both should
   fail loudly rather than sit in the binary.

Exit status is non-zero on a violation, so this can gate packaging.
"""

from __future__ import annotations

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
UI = ROOT / "ui" / "app.js"
MAIN = ROOT / "src-tauri" / "src" / "main.rs"

# Commands are reached two ways: from the page, or from Rust itself (the tray,
# the shortcut handler, single-instance activation). Rust-side callers are found
# by looking for a call rather than a definition, so this list does not need
# maintaining.
RUST_SOURCES = ["src-tauri/src/main.rs", "src-tauri/src/pipeline.rs",
                "src-tauri/src/lib.rs"]


def registered_commands(main_src: str) -> set[str]:
    start = main_src.index("generate_handler![")
    block = main_src[start:]
    block = block[: block.index("])")]
    names = set()
    for line in block.splitlines():
        line = line.strip().rstrip(",")
        if line and re.fullmatch(r"[a-z_0-9]+", line):
            names.add(line)
    return names


def rust_callers(names: set[str]) -> set[str]:
    """Commands that something in Rust actually calls."""
    used = set()
    for rel in RUST_SOURCES:
        path = ROOT / rel
        if not path.is_file():
            continue
        for line in path.read_text().splitlines():
            # Skip the definition and the registration list itself.
            if re.match(r"\s*(pub )?fn ", line):
                continue
            for name in names:
                if re.search(rf"\b{name}\s*\(", line):
                    used.add(name)
    return used


def main() -> int:
    js = UI.read_text()
    main_src = MAIN.read_text()

    called = set(re.findall(r"invoke\(\s*[\"']([a-z_0-9]+)[\"']", js))
    registered = registered_commands(main_src)

    problems: list[str] = []

    for name in sorted(called - registered):
        problems.append(f"the UI calls `{name}` but it is not registered")

    from_rust = rust_callers(registered)
    for name in sorted(registered - called - from_rust):
        problems.append(f"`{name}` is registered but nothing calls it")

    print(f"  commands invoked by the UI: {len(called)}")
    print(f"  commands registered:        {len(registered)}")
    print(f"  reached from Rust only:     {len(from_rust - called)}"
          f" ({', '.join(sorted(from_rust - called)) or 'none'})")

    if problems:
        for p in problems:
            print(f"  FAIL {p}")
        return 1

    print("  PASS every command has a caller and every call has a command")
    return 0


if __name__ == "__main__":
    sys.exit(main())
