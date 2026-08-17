#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
from pathlib import Path
import re
import unittest


REPOSITORY = Path(__file__).resolve().parents[2]
MODULE_PATH = REPOSITORY / "scripts/po2lmo.py"
SPEC = importlib.util.spec_from_file_location("flowsplice_po2lmo", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("failed to load FlowSplice po2lmo module")
PO2LMO = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PO2LMO)


class FlowSpliceI18nTest(unittest.TestCase):
    def test_every_luci_string_has_a_simplified_chinese_translation(self) -> None:
        javascript = (
            REPOSITORY
            / "openwrt/root/www/luci-static/resources/view/flowsplice.js"
        ).read_text(encoding="utf-8")
        messages = set(re.findall(r"_\('([^']*)'\)", javascript))
        translations = dict(
            PO2LMO.parse_po(
                REPOSITORY / "openwrt/po/zh_Hans/flowsplice.po"
            )
        )
        deliberately_language_neutral = {"FlowSplice", "PID"}
        self.assertEqual(messages - set(translations), deliberately_language_neutral)
        template = (
            REPOSITORY / "openwrt/po/templates/flowsplice.pot"
        ).read_text(encoding="utf-8")
        template_messages = set(re.findall(r'^msgid "([^"].*)"$', template, re.MULTILINE))
        self.assertEqual(messages, template_messages)

    def test_catalog_compiles_deterministically(self) -> None:
        path = REPOSITORY / "openwrt/po/zh_Hans/flowsplice.po"
        first = PO2LMO.compile_po(path)
        second = PO2LMO.compile_po(path)
        self.assertEqual(first, second)
        self.assertGreater(len(first), 100)


if __name__ == "__main__":
    unittest.main()
