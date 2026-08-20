#!/usr/bin/env python3
from __future__ import annotations

from pathlib import Path
import subprocess
import tempfile
import textwrap
import tomllib
import unittest


REPOSITORY = Path(__file__).resolve().parents[2]
RENDERER = REPOSITORY / "openwrt/root/usr/libexec/flowsplice/render-config"


FAKE_FUNCTIONS = r"""
config_load() { :; }

config_get() {
	local variable="$1" section="$2" option="$3" resolved="${4:-}"
	case "$section.$option" in
		server.id) resolved='server-1' ;;
		server.control_listen) resolved='192.0.2.1:7443' ;;
		server.cert) resolved='/etc/flowsplice/server.crt' ;;
		server.key) resolved='/etc/flowsplice/server.key' ;;
		server.management_ca) resolved='/etc/flowsplice/management-ca.crt' ;;
		server.deployment_root_public_key) resolved='/etc/flowsplice/deployment-root.pub' ;;
		server.deployment_trust) resolved='/etc/flowsplice/deployment-trust.json' ;;
		server.control_signing_key) resolved='/etc/flowsplice/server-control.key' ;;
		server.travel_authorization_state) resolved='/etc/flowsplice/server-authorization.json' ;;
		server.control_generation_state) resolved='/etc/flowsplice/server-control-generation.json' ;;
		server.state_store) resolved='/etc/flowsplice/server-state.redb' ;;
		server.ui_listen) resolved='127.0.0.1:9083' ;;
		server.handshake_timeout_secs) resolved='10' ;;
		server.work_ttl_secs) resolved='15' ;;
		server.max_pending_work) resolved='256' ;;
		server.control_snapshot_ttl_secs) resolved='120' ;;
		server.max_control_connections) resolved='256' ;;
		home_1.id) resolved='home-1' ;;
		home_2.id) resolved='home-2' ;;
		relay_1.id) resolved='relay-1' ;;
		relay_1.management_addr) resolved='relay-1.example:8443' ;;
		relay_1.data_public_addr) resolved='relay-1.example:8444' ;;
		relay_2.id) resolved='relay-2' ;;
		relay_2.management_addr) resolved='relay-2.example:8443' ;;
		relay_2.data_public_addr) resolved='relay-2.example:8444' ;;
		relay_test.id) resolved='relay-test' ;;
		relay_test.management_listen) resolved='0.0.0.0:8443' ;;
		relay_test.data_listen) resolved='0.0.0.0:8444' ;;
		relay_test.data_public_addr) resolved='relay-test.example:8444' ;;
		relay_test.server_id) resolved='server-1' ;;
		relay_test.cert) resolved='/etc/flowsplice/relay.crt' ;;
		relay_test.key) resolved='/etc/flowsplice/relay.key' ;;
		relay_test.management_ca) resolved='/etc/flowsplice/management-ca.crt' ;;
		relay_test.deployment_root_public_key) resolved='/etc/flowsplice/deployment-root.pub' ;;
		relay_test.deployment_trust) resolved='/etc/flowsplice/deployment-trust.json' ;;
		relay_test.travel_authorization_cache) resolved='/etc/flowsplice/relay-auth.json' ;;
		relay_test.state_store) resolved='/etc/flowsplice/relay-state.redb' ;;
		relay_test.ui_listen) resolved='127.0.0.1:9084' ;;
		relay_test.handshake_timeout_secs) resolved='10' ;;
		relay_test.route_ttl_secs) resolved='15' ;;
		relay_test.max_pending_routes) resolved='256' ;;
		relay_test.max_management_connections) resolved='1024' ;;
		relay_test.max_data_connections) resolved='2048' ;;
	esac
	eval "$variable=\$resolved"
}

config_get_bool() {
	local variable="$1"
	eval "$variable=1"
}

config_list_foreach() {
	local section="$1" option="$2" callback="$3" value
	case "$section.$option" in
		relay_test.server_spki_pin) set -- '0123456789abcdef' 'fedcba9876543210' ;;
		*) set -- ;;
	esac
	for value in "$@"; do "$callback" "$value"; done
}

config_foreach() {
	local callback="$1" type="$2"
	case "$type" in
		home)
			[ "${FAKE_NO_HOMES:-0}" = 1 ] || {
				"$callback" home_1
				"$callback" home_2
			}
			;;
		relay_endpoint)
			"$callback" relay_1
			"$callback" relay_2
			;;
	esac
}
"""


class RendererTest(unittest.TestCase):
    def run_renderer(
        self, *arguments: str, no_homes: bool = False
    ) -> subprocess.CompletedProcess[str]:
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        root = Path(directory.name)
        functions = root / "functions.sh"
        functions.write_text(textwrap.dedent(FAKE_FUNCTIONS), encoding="utf-8")
        renderer = root / "render-config"
        source = RENDERER.read_text(encoding="utf-8").replace(
            ". /lib/functions.sh", f'. "{functions}"', 1
        )
        renderer.write_text(source, encoding="utf-8")
        renderer.chmod(0o755)
        output = root / f"{arguments[0]}.toml"
        environment = {"FAKE_NO_HOMES": "1"} if no_homes else None
        result = subprocess.run(
            [str(renderer), *arguments, str(output)],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=environment,
        )
        result.output_path = output  # type: ignore[attr-defined]
        return result

    def test_server_renders_two_independent_home_tables(self) -> None:
        result = self.run_renderer("server")
        self.assertEqual(result.returncode, 0, result.stderr)
        output = result.output_path  # type: ignore[attr-defined]
        config = tomllib.loads(output.read_text(encoding="utf-8"))
        self.assertEqual([home["id"] for home in config["homes"]], ["home-1", "home-2"])
        self.assertTrue(all(set(home) == {"id"} for home in config["homes"]))
        self.assertEqual([relay["id"] for relay in config["relays"]], ["relay-1", "relay-2"])
        self.assertNotIn("travel_authorities", config)
        self.assertEqual(config["control_snapshot_ttl_secs"], 120)
        self.assertEqual(config["deployment_root_public_key"], "/etc/flowsplice/deployment-root.pub")
        self.assertEqual(config["state_store"], "/etc/flowsplice/server-state.redb")
        self.assertEqual(config["ui_listen"], "127.0.0.1:9083")
        self.assertNotIn("data_listens", config)
        self.assertEqual(
            [relay["data_public_addr"] for relay in config["relays"]],
            ["relay-1.example:8444", "relay-2.example:8444"],
        )

    def test_server_render_fails_without_a_home(self) -> None:
        result = self.run_renderer("server", no_homes=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("at least one home section", result.stderr)

    def test_relay_renders_direct_data_plane_and_redb_state(self) -> None:
        result = self.run_renderer("relay", "relay_test")
        self.assertEqual(result.returncode, 0, result.stderr)
        output = result.output_path  # type: ignore[attr-defined]
        config = tomllib.loads(output.read_text(encoding="utf-8"))
        self.assertEqual(config["data_public_addr"], "relay-test.example:8444")
        self.assertEqual(config["state_store"], "/etc/flowsplice/relay-state.redb")
        self.assertEqual(config["ui_listen"], "127.0.0.1:9084")
        self.assertEqual(config["server_spki_pins"], ["0123456789abcdef", "fedcba9876543210"])
        self.assertNotIn("server_data_addr", config)


if __name__ == "__main__":
    unittest.main()
