#!/usr/bin/env python3

from __future__ import annotations

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


REPOSITORY = Path(__file__).resolve().parents[1]
CHECKER = REPOSITORY / "scripts/check-package-privacy.py"


class PackagePrivacyTest(unittest.TestCase):
    def run_checker(self, relays: list[str]) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / "travel-bootstrap.toml"
            entries = ", ".join(f'"{relay}"' for relay in relays)
            config.write_text(
                'deployment_root_public_key = "deployment-root.pub"\n'
                'deployment_trust = "deployment-trust.json"\n'
                f"bootstrap_relays = [{entries}]\n"
                'ui_listen = "127.0.0.1:9080"\n'
            )
            return subprocess.run(
                [sys.executable, str(CHECKER), str(config)],
                check=False,
                capture_output=True,
                text=True,
            )

    def test_accepts_dns_bootstrap_hosts(self) -> None:
        result = self.run_checker(["relay.example.net:8443", "relay-2.internal:8443"])
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_rejects_ipv4_bootstrap_host(self) -> None:
        result = self.run_checker(["203.0.113.10:8443"])
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must use DNS names", result.stderr)
        self.assertNotIn("203.0.113.10", result.stderr)

    def test_rejects_private_ipv4_bootstrap_host(self) -> None:
        result = self.run_checker(["192.168.1.10:8443"])
        self.assertNotEqual(result.returncode, 0)

    def test_rejects_ipv6_bootstrap_host(self) -> None:
        result = self.run_checker(["[2001:db8::1]:8443"])
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
