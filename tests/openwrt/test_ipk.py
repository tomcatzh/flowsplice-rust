#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import io
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest


REPOSITORY = Path(__file__).resolve().parents[2]
BUILDER = REPOSITORY / "scripts/build-openwrt-ipk.py"


def open_member(archive: tarfile.TarFile, name: str) -> bytes:
    member = archive.extractfile(name)
    if member is None:
        raise AssertionError(f"missing archive member: {name}")
    return member.read()


class IpkBuilderTest(unittest.TestCase):
    def test_package_is_deterministic_and_has_expected_opkg_shape(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            server = root / "flowsplice-server"
            relay = root / "flowsplice-relay"
            server.write_bytes(b"server-binary")
            relay.write_bytes(b"relay-binary")
            server.chmod(0o755)
            relay.chmod(0o755)
            outputs = []
            for index in range(2):
                output = root / f"out-{index}"
                result = subprocess.run(
                    [
                        sys.executable,
                        str(BUILDER),
                        "--server",
                        str(server),
                        "--relay",
                        str(relay),
                        "--architecture",
                        "aarch64_generic",
                        "--version",
                        "0.1.0",
                        "--output-dir",
                        str(output),
                        "--source-date-epoch",
                        "1",
                    ],
                    check=True,
                    capture_output=True,
                    text=True,
                )
                outputs.append(Path(result.stdout.strip()))
            self.assertEqual(
                hashlib.sha256(outputs[0].read_bytes()).digest(),
                hashlib.sha256(outputs[1].read_bytes()).digest(),
            )

            with tarfile.open(outputs[0], mode="r:gz") as outer:
                self.assertEqual(
                    outer.getnames(),
                    ["./debian-binary", "./data.tar.gz", "./control.tar.gz"],
                )
                self.assertEqual(open_member(outer, "./debian-binary"), b"2.0\n")
                data_bytes = open_member(outer, "./data.tar.gz")
                control_bytes = open_member(outer, "./control.tar.gz")

            with tarfile.open(fileobj=io.BytesIO(data_bytes), mode="r:gz") as data:
                names = set(data.getnames())
                self.assertTrue(data.getmember("./usr/libexec/flowsplice").isdir())
                self.assertTrue(
                    data.getmember("./usr/share/licenses/flowsplice-openwrt").isdir()
                )
                for member in data.getmembers():
                    if not member.isfile():
                        continue
                    parent = Path(member.name.removeprefix("./")).parent
                    while parent != Path("."):
                        self.assertIn(f"./{parent.as_posix()}", names)
                        parent = parent.parent
                self.assertIn("./usr/bin/flowsplice-server", names)
                self.assertIn("./usr/bin/flowsplice-relay", names)
                self.assertIn("./etc/config/flowsplice", names)
                self.assertIn("./etc/init.d/flowsplice", names)
                self.assertIn("./www/luci-static/resources/view/flowsplice.js", names)
                self.assertIn("./usr/lib/lua/luci/i18n/flowsplice.zh-cn.lmo", names)
                self.assertEqual(data.getmember("./etc/init.d/flowsplice").mode, 0o755)
                self.assertEqual(data.getmember("./etc/config/flowsplice").mode, 0o644)
                packaged_config = open_member(data, "./etc/config/flowsplice").decode()
                self.assertIn(
                    "option travel_authorization_state '/etc/flowsplice/state/server-authorization.json'",
                    packaged_config,
                )
                self.assertNotIn(
                    "option travel_authorization_state '/etc/flowsplice/travel-credentials.json'",
                    packaged_config,
                )
                self.assertNotIn("option admin_socket", packaged_config)
                self.assertGreater(
                    len(open_member(data, "./usr/lib/lua/luci/i18n/flowsplice.zh-cn.lmo")),
                    100,
                )

            with tarfile.open(fileobj=io.BytesIO(control_bytes), mode="r:gz") as control:
                self.assertEqual(
                    set(control.getnames()),
                    {"./control", "./conffiles", "./postinst", "./prerm"},
                )
                metadata = open_member(control, "./control").decode()
                self.assertIn("Package: flowsplice-openwrt\n", metadata)
                self.assertIn("Architecture: aarch64_generic\n", metadata)
                self.assertIn("Depends: luci-base\n", metadata)


if __name__ == "__main__":
    unittest.main()
