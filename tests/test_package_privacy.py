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
    def run_checker(self, name: str, body: str) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / name
            config.write_text(body)
            return subprocess.run(
                [sys.executable, str(CHECKER), str(config)],
                check=False,
                capture_output=True,
                text=True,
            )

    def travel_config(self, relay: str) -> str:
        return (
            'deployment_root_public_key = "deployment-root.pub"\n'
            'deployment_trust = "deployment-trust.json"\n'
            f'bootstrap_relays = ["{relay}"]\n'
            'ui_listen = "127.0.0.1:9080"\n'
        )

    def test_accepts_reserved_example_hosts(self) -> None:
        travel = self.run_checker(
            "travel-bootstrap.example.toml",
            self.travel_config("relay-1.example.net:8443"),
        )
        home = self.run_checker(
            "home-bootstrap.example.toml",
            'deployment_root_public_key = "deployment-root.pub"\n'
            'deployment_trust = "deployment-trust.json"\n'
            'server_id = "server-1"\n'
            'server_name = "server.example.org"\n'
            'server_control_port = 7443\n'
            'ui_listen = "127.0.0.1:9082"\n',
        )
        self.assertEqual(travel.returncode, 0, travel.stderr)
        self.assertEqual(home.returncode, 0, home.stderr)

    def test_rejects_every_non_example_subdomain_without_echoing_it(self) -> None:
        forbidden = "relay.operator.invalid:8443"
        result = self.run_checker(
            "travel-bootstrap.example.toml",
            self.travel_config(forbidden),
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("secret-equivalent", result.stderr)
        self.assertNotIn(forbidden, result.stderr)

    def test_rejects_ip_addresses_without_echoing_them(self) -> None:
        for forbidden in (
            "203.0.113.10:8443",
            "192.168.1.10:8443",
            "[2001:db8::1]:8443",
        ):
            with self.subTest(forbidden=forbidden):
                result = self.run_checker(
                    "travel-bootstrap.example.toml",
                    self.travel_config(forbidden),
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("must not contain an IP address", result.stderr)
                self.assertNotIn(forbidden, result.stderr)

    def test_rejects_non_sample_filename(self) -> None:
        result = self.run_checker(
            "travel-bootstrap.toml",
            self.travel_config("relay-1.example.net:8443"),
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(".example.toml", result.stderr)

    def test_rejects_fields_outside_the_public_allowlist(self) -> None:
        result = self.run_checker(
            "travel-bootstrap.example.toml",
            self.travel_config("relay-1.example.net:8443") + 'deployment_id = "sample"\n',
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("keys do not match", result.stderr)


if __name__ == "__main__":
    unittest.main()
