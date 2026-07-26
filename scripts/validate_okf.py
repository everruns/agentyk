#!/usr/bin/env python3
"""Validate Agentyk's Open Knowledge Format v0.2 bundle profile."""

from __future__ import annotations

import argparse
import datetime as dt
import re
import sys
from dataclasses import dataclass
from pathlib import Path

FRONTMATTER = re.compile(r"\A---\n(?P<yaml>.*?)\n---(?:\n|\Z)", re.DOTALL)
SCALAR = re.compile(r"^(?P<key>[A-Za-z_][A-Za-z0-9_-]*):(?:\s*(?P<value>.*))?$")
MARKDOWN_LINK = re.compile(r"\[[^]]+\]\((?P<target>[^)]+)\)")
INDEX_ENTRY = re.compile(
    r"^- \[(?P<label>[^]]+)\]\((?P<path>[^)#]+\.md)\) — (?P<description>.+)$",
    re.MULTILINE,
)
ALLOWED_STATUS = {"draft", "stable", "deprecated"}
ACTOR = re.compile(r"^(?:human:[^\s:]+|process:[^\s:]+|[^\s/]+/[^\s/]+)$")


@dataclass(frozen=True)
class Document:
    path: Path
    metadata: dict[str, str]
    body: str


def error(errors: list[str], path: Path, message: str) -> None:
    errors.append(f"{path}: {message}")


def parse_document(path: Path, errors: list[str]) -> Document | None:
    text = path.read_text(encoding="utf-8")
    match = FRONTMATTER.match(text)
    if not match:
        error(errors, path, "missing YAML frontmatter")
        return None

    metadata: dict[str, str] = {}
    for number, line in enumerate(match.group("yaml").splitlines(), 2):
        if not line or line[0].isspace() or line.lstrip().startswith("-"):
            continue
        scalar = SCALAR.match(line)
        if not scalar:
            error(errors, path, f"line {number}: unsupported top-level YAML syntax")
            continue
        key = scalar.group("key")
        if key in metadata:
            error(errors, path, f"duplicate frontmatter key {key!r}")
        metadata[key] = (scalar.group("value") or "").strip().strip('"\'')

    return Document(path, metadata, text[match.end() :])


def validate_iso_date(value: str) -> bool:
    try:
        dt.date.fromisoformat(value)
    except ValueError:
        return False
    return True


def validate_concept(document: Document, errors: list[str]) -> None:
    metadata = document.metadata
    for key in ("type", "title", "description"):
        if not metadata.get(key):
            error(errors, document.path, f"missing required Agentyk profile field {key!r}")
    if "timestamp" in metadata:
        error(errors, document.path, "obsolete 'timestamp' field; use OKF v0.2 'generated'")
    if metadata.get("status") and metadata["status"] not in ALLOWED_STATUS:
        error(errors, document.path, f"invalid status {metadata['status']!r}")
    if metadata.get("stale_after") and not validate_iso_date(metadata["stale_after"]):
        error(errors, document.path, "stale_after must be an ISO 8601 date (YYYY-MM-DD)")
    if "generated" in metadata and not metadata["generated"]:
        # Nested generated mappings are checked from their source lines below.
        yaml = FRONTMATTER.match(document.path.read_text(encoding="utf-8")).group("yaml")  # type: ignore[union-attr]
        generated = re.search(r"^generated:\s*\n(?P<body>(?:[ \t]+.*\n?)*)", yaml, re.MULTILINE)
        by = re.search(r"^\s+by:\s*([^\s]+)\s*$", generated.group("body") if generated else "", re.MULTILINE)
        if not by or not ACTOR.match(by.group(1).strip('"\'')):
            error(errors, document.path, "generated.by must use an OKF v0.2 actor identity")
    if re.search(r"^#{1,6}\s+Citations\s*$", document.body, re.MULTILINE | re.IGNORECASE):
        error(errors, document.path, "body Citations sections are obsolete; use frontmatter sources")


def resolve_link(bundle: Path, source: Path, target: str) -> Path | None:
    target = target.split("#", 1)[0]
    if not target or "://" in target or target.startswith("mailto:") or "<" in target:
        return None
    if target.startswith("/"):
        return bundle / target.removeprefix("/")
    return source.parent / target


def validate_bundle(bundle: Path) -> list[str]:
    errors: list[str] = []
    if not bundle.is_dir():
        return [f"{bundle}: bundle directory does not exist"]

    index_path = bundle / "index.md"
    index = parse_document(index_path, errors) if index_path.exists() else None
    if not index:
        if not index_path.exists():
            error(errors, index_path, "missing required bundle index")
        return errors
    if index.metadata.get("okf_version") != "0.2":
        error(errors, index_path, 'okf_version must be "0.2"')

    concepts: dict[str, Document] = {}
    for path in sorted(bundle.rglob("*.md")):
        relative = path.relative_to(bundle).as_posix()
        if relative in {"index.md", "log.md"}:
            continue
        document = parse_document(path, errors)
        if document:
            concepts[relative] = document
            validate_concept(document, errors)

    entries: dict[str, list[tuple[str, str]]] = {}
    for match in INDEX_ENTRY.finditer(index.body):
        entries.setdefault(match.group("path"), []).append(
            (match.group("label"), match.group("description"))
        )

    for relative, document in concepts.items():
        matches = entries.get(relative, [])
        if len(matches) != 1:
            error(errors, index_path, f"concept {relative!r} must be listed exactly once")
            continue
        label, description = matches[0]
        if label != document.metadata.get("title"):
            error(errors, index_path, f"entry for {relative!r} must use its frontmatter title")
        if description != document.metadata.get("description"):
            error(errors, index_path, f"entry for {relative!r} must use its frontmatter description")
    for relative in sorted(set(entries) - set(concepts)):
        error(errors, index_path, f"entry targets missing concept {relative!r}")

    for path in sorted(bundle.rglob("*.md")):
        text = path.read_text(encoding="utf-8")
        for match in MARKDOWN_LINK.finditer(text):
            resolved = resolve_link(bundle, path, match.group("target"))
            if resolved and not resolved.resolve().exists():
                error(errors, path, f"broken local link {match.group('target')!r}")

    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bundle", nargs="?", default="knowledge", type=Path)
    args = parser.parse_args()
    errors = validate_bundle(args.bundle)
    if errors:
        print("OKF v0.2 validation failed:", file=sys.stderr)
        for message in errors:
            print(f"- {message}", file=sys.stderr)
        return 1
    print(f"OKF v0.2 validation passed: {args.bundle}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
