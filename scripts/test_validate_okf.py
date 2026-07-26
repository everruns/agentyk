#!/usr/bin/env python3
"""Tests for the repository's OKF v0.2 validator."""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("validate_okf.py")
SPEC = importlib.util.spec_from_file_location("validate_okf", MODULE_PATH)
assert SPEC and SPEC.loader
VALIDATOR = importlib.util.module_from_spec(SPEC)
import sys
sys.modules[SPEC.name] = VALIDATOR
SPEC.loader.exec_module(VALIDATOR)


class ValidateOkfTests(unittest.TestCase):
    def make_bundle(
        self,
        *,
        version: str = '"0.2"',
        concept_metadata: str = "",
        index_description: str = "Explains the thing.",
    ) -> Path:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        bundle = Path(temporary.name) / "knowledge"
        bundle.mkdir()
        (bundle / "index.md").write_text(
            f'''---\nokf_version: {version}\n---\n\n# Knowledge\n\n## Concepts\n\n- [Thing](thing.md) — {index_description}\n''',
            encoding="utf-8",
        )
        (bundle / "thing.md").write_text(
            f'''---\ntitle: Thing\ntype: Concept\ndescription: Explains the thing.\n{concept_metadata}---\n\n# Thing\n''',
            encoding="utf-8",
        )
        return bundle

    def test_accepts_valid_v0_2_bundle(self) -> None:
        self.assertEqual([], VALIDATOR.validate_bundle(self.make_bundle()))

    def test_rejects_wrong_bundle_version(self) -> None:
        errors = VALIDATOR.validate_bundle(self.make_bundle(version='"0.1"'))
        self.assertTrue(any("okf_version" in message for message in errors))

    def test_rejects_obsolete_timestamp(self) -> None:
        errors = VALIDATOR.validate_bundle(
            self.make_bundle(concept_metadata="timestamp: 2026-07-25\n")
        )
        self.assertTrue(any("obsolete 'timestamp'" in message for message in errors))

    def test_rejects_index_description_drift(self) -> None:
        errors = VALIDATOR.validate_bundle(
            self.make_bundle(index_description="Different description.")
        )
        self.assertTrue(any("frontmatter description" in message for message in errors))

    def test_rejects_unindexed_concept(self) -> None:
        bundle = self.make_bundle()
        (bundle / "other.md").write_text(
            "---\ntitle: Other\ntype: Concept\ndescription: Another concept.\n---\n",
            encoding="utf-8",
        )
        errors = VALIDATOR.validate_bundle(bundle)
        self.assertTrue(any("must be listed exactly once" in message for message in errors))

    def test_rejects_broken_local_link(self) -> None:
        bundle = self.make_bundle()
        with (bundle / "thing.md").open("a", encoding="utf-8") as concept:
            concept.write("\nSee [missing](missing.md).\n")
        errors = VALIDATOR.validate_bundle(bundle)
        self.assertTrue(any("broken local link" in message for message in errors))


if __name__ == "__main__":
    unittest.main()
