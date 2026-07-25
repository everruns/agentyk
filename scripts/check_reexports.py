#!/usr/bin/env python3
"""Fail if `agentyk` does not re-export everything `agentyk-core` exports.

Applications depend only on `agentyk`, so anything core makes public has to be
reachable through it. That used to be guaranteed by `pub use agentyk_core::*`
— at the cost of every future core item silently widening this crate's
surface, including names that collide with one it already owns.

The glob is gone, so the list is explicit. This script is what keeps an
explicit list from developing the mirror-image problem: an item added to core
and forgotten here, invisible until a user cannot find it.

Zero dependencies; run from the repository root.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

CORE = Path("crates/agentyk-core/src/lib.rs")
FRAMEWORK = Path("crates/agentyk/src/lib.rs")

# `pub use <path>::{a, b, c};` or `pub use <path>::Item;`, across newlines.
BRACED = re.compile(r"pub use ([\w:]*?)\{(.*?)\}\s*;", re.DOTALL)
SINGLE = re.compile(r"pub use ([\w:]+)::(\w+)\s*;")


def exported_names(source: str, only_from: str | None = None) -> set[str]:
    """Every identifier re-exported by `source`.

    `only_from` restricts to re-exports whose path starts with that prefix,
    which is how the framework's core re-exports are separated from its own.
    """
    names: set[str] = set()

    for path, body in BRACED.findall(source):
        if only_from is not None and not path.startswith(only_from):
            continue
        for part in body.split(","):
            part = part.strip()
            if not part or part.startswith("//"):
                continue
            names.add(part.split(" as ")[0].strip())

    for path, item in SINGLE.findall(source):
        full = f"{path}::"
        if only_from is not None and not full.startswith(only_from):
            continue
        names.add(item)

    return names


def main() -> int:
    if not CORE.exists() or not FRAMEWORK.exists():
        print("error: run this from the repository root", file=sys.stderr)
        return 2

    core = exported_names(CORE.read_text())
    framework = exported_names(FRAMEWORK.read_text(), only_from="agentyk_core::")

    missing = sorted(core - framework)
    if missing:
        print("agentyk does not re-export these agentyk-core items:\n", file=sys.stderr)
        for name in missing:
            print(f"  {name}", file=sys.stderr)
        print(
            f"\nAdd them to the `pub use agentyk_core::{{..}}` list in {FRAMEWORK}.",
            file=sys.stderr,
        )
        return 1

    print(f"ok: agentyk re-exports all {len(core)} agentyk-core items")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
