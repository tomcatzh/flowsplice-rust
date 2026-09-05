#!/usr/bin/env python3
"""Unit tests for the private Travel packaging trust boundary."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import zipfile


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
HELPER_PATH = REPOSITORY_ROOT / "scripts" / "private-travel-trust.py"
COPY_TRUST_RESOURCE_SCRIPT = REPOSITORY_ROOT / "scripts" / "copy-travel-trust-resource.sh"
SPEC = importlib.util.spec_from_file_location("private_travel_trust", HELPER_PATH)
assert SPEC is not None and SPEC.loader is not None
trust = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = trust
SPEC.loader.exec_module(trust)

# NIST P-256's public generator. It is test data, not a deployment identity.
VALID_ROOT = (
    "04"
    "6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296"
    "4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5"
)


def toml_string(value: str) -> str:
    return json.dumps(value)


def run_git(directory: Path, *arguments: str) -> None:
    subprocess.run(
        ["git", "-C", os.fspath(directory), *arguments],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def alternate_valid_root() -> str:
    point = bytes.fromhex(VALID_ROOT)
    x = int.from_bytes(point[1:33], "big")
    y = int.from_bytes(point[33:65], "big")
    slope = (3 * x * x + trust.P256_A) * pow(2 * y, -1, trust.P256_PRIME)
    slope %= trust.P256_PRIME
    doubled_x = (slope * slope - 2 * x) % trust.P256_PRIME
    doubled_y = (slope * (x - doubled_x) - y) % trust.P256_PRIME
    return (b"\x04" + doubled_x.to_bytes(32, "big") + doubled_y.to_bytes(32, "big")).hex()


class PrivateTravelTrustTests(unittest.TestCase):
    def make_profile(
        self,
        root: Path,
        output: Path,
        keystore: Path,
        *,
        profile_directory: Path | None = None,
        extra: str = "",
    ) -> Path:
        profile = (profile_directory or root.parent) / "private-profile.toml"
        profile.write_text(
            "\n".join(
                (
                    "[trust]",
                    f"deployment_root_public_key = {toml_string(os.fspath(root))}",
                    "[output]",
                    f"directory = {toml_string(os.fspath(output))}",
                    "[android]",
                    f"keystore_file = {toml_string(os.fspath(keystore))}",
                    'key_alias = "release"',
                    'store_password_env = "PRIVATE_TRAVEL_STORE_PASSWORD"',
                    'key_password_env = "PRIVATE_TRAVEL_KEY_PASSWORD"',
                    "[apple]",
                    'team_id = "TEAMID"',
                    'notary_profile = "notary-profile"',
                    'ios_device = "test-device"',
                    extra,
                )
            ),
            encoding="utf-8",
        )
        return profile

    def make_external_inputs(self, directory: Path) -> tuple[Path, Path, Path, Path]:
        root = directory / "deployment-root.pub"
        root.write_text(VALID_ROOT + "\n", encoding="ascii")
        output = directory / "output"
        output.mkdir()
        keystore = directory / "release.jks"
        keystore.write_bytes(b"test fixture only")
        profile = self.make_profile(root, output, keystore)
        return root, output, keystore, profile

    def make_repository(self, directory: Path, version: str = "0.0.1") -> Path:
        repository = directory / "source-repository"
        repository.mkdir()
        run_git(repository, "init")
        run_git(repository, "config", "user.email", "tests@example.invalid")
        run_git(repository, "config", "user.name", "Private Travel Tests")
        (repository / "Cargo.toml").write_text(
            f'[workspace]\n[workspace.package]\nversion = "{version}"\n', encoding="utf-8"
        )
        run_git(repository, "add", "Cargo.toml")
        run_git(repository, "commit", "-m", "fixture")
        return repository

    def test_valid_profile_is_accepted(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root, output, keystore, profile = self.make_external_inputs(Path(temporary))
            loaded = trust.load_profile(profile)
            self.assertEqual(loaded.root.raw, root.read_bytes())
            self.assertEqual(loaded.output_directory, output.resolve())
            self.assertEqual(loaded.android.keystore_file, keystore.resolve())

    def test_check_path_accepts_a_nonexistent_external_destination(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "future" / "release"
            self.assertEqual(
                trust.external_path(os.fspath(destination)), destination.resolve(strict=False)
            )
            self.assertFalse(destination.exists())

    def test_bad_curve_and_private_pem_are_rejected_without_echoing_input(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            bad_curve = (VALID_ROOT[:-1] + ("0" if VALID_ROOT[-1] != "0" else "1")).encode("ascii")
            with self.assertRaises(trust.PackagingError) as curve_error:
                trust.validate_root_bytes(bad_curve)
            self.assertNotIn(bad_curve.decode("ascii"), str(curve_error.exception))
            pem = b"-----BEGIN PRIVATE KEY-----\nfixture\n-----END PRIVATE KEY-----\n"
            with self.assertRaises(trust.PackagingError) as pem_error:
                trust.validate_root_bytes(pem)
            self.assertNotIn("PRIVATE KEY", str(pem_error.exception))
            oversized = directory / "oversized-root.pub"
            oversized.write_bytes(b"0" * (trust.MAX_ROOT_FILE_BYTES + 1))
            with self.assertRaises(trust.PackagingError):
                trust.read_root_snapshot(oversized)
            with self.assertRaises(trust.PackagingError):
                trust.read_root_snapshot(directory / "missing-root.pub")

    def test_unknown_profile_field_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root, output, keystore, _profile = self.make_external_inputs(Path(temporary))
            profile = self.make_profile(root, output, keystore, extra='unexpected = "value"')
            with self.assertRaises(trust.PackagingError):
                trust.load_profile(profile)

    def test_git_worktree_and_metadata_paths_are_rejected_even_when_ignored(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repository = self.make_repository(directory)
            (repository / ".gitignore").write_text("ignored-root.pub\n", encoding="utf-8")
            ignored_root = repository / "ignored-root.pub"
            ignored_root.write_text(VALID_ROOT, encoding="ascii")
            external = directory / "external"
            external.mkdir()
            output = external / "output"
            output.mkdir()
            keystore = external / "release.jks"
            keystore.write_bytes(b"fixture")
            profile = self.make_profile(ignored_root, output, keystore, profile_directory=external)
            with self.assertRaises(trust.PackagingError):
                trust.load_profile(profile)

            outside_root = external / "outside-root.pub"
            outside_root.write_text(VALID_ROOT, encoding="ascii")
            root_link = external / "linked-root.pub"
            root_link.symlink_to(ignored_root)
            linked_profile = self.make_profile(root_link, output, keystore, profile_directory=external)
            with self.assertRaises(trust.PackagingError):
                trust.load_profile(linked_profile)

            clean_profile = self.make_profile(outside_root, output, keystore, profile_directory=external)
            profile_in_repository = repository / "private-profile.toml"
            profile_in_repository.write_bytes(clean_profile.read_bytes())
            with self.assertRaises(trust.PackagingError):
                trust.load_profile(profile_in_repository)
            with self.assertRaises(trust.PackagingError):
                trust.external_path(os.fspath(repository / "new-output"))

    def test_snapshot_freezes_root_before_source_mutation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            source = directory / "source-root.pub"
            source.write_text(VALID_ROOT + "\n", encoding="ascii")
            _path, snapshot = trust.read_root_snapshot(source)
            source.write_bytes(b"not a deployment root")
            destination = directory / "frozen" / "deployment-root.pub"
            trust.write_frozen_root(snapshot, destination)
            self.assertEqual(destination.read_text(encoding="ascii"), VALID_ROOT + "\n")

    def test_copy_and_verify_detect_tampered_artifact(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            source = directory / "source-root.pub"
            source.write_text(VALID_ROOT + "\n", encoding="ascii")
            artifact = directory / "artifact" / "deployment-root.pub"
            trust.copy_root(source, artifact)
            trust.verify_root(source, artifact)
            artifact.write_text(VALID_ROOT.upper() + "\n", encoding="ascii")
            with self.assertRaises(trust.PackagingError):
                trust.verify_root(source, artifact)

    def test_neutral_cleanup_can_remove_only_the_generated_copy(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            source = directory / "source-root.pub"
            source.write_text(VALID_ROOT + "\n", encoding="ascii")
            generated = directory / "app" / "bootstrap" / "deployment-root.pub"
            trust.copy_root(source, generated)
            generated.unlink()
            self.assertFalse(generated.exists())
            self.assertEqual(source.read_text(encoding="ascii"), VALID_ROOT + "\n")

    def test_copy_resource_hook_replaces_then_removes_a_private_root(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            app_directory = directory / "app"
            app_directory.mkdir()
            root_a = directory / "root-a.pub"
            root_b = directory / "root-b.pub"
            root_a.write_text(VALID_ROOT + "\n", encoding="ascii")
            root_b.write_text(alternate_valid_root() + "\n", encoding="ascii")
            resource = app_directory / "Resources" / "bootstrap" / "deployment-root.pub"
            environment = dict(os.environ)
            environment.update(
                {
                    "TARGET_BUILD_DIR": os.fspath(app_directory),
                    "UNLOCALIZED_RESOURCES_FOLDER_PATH": "Resources",
                }
            )
            for root, expected in ((root_a, root_a.read_bytes()), (root_b, root_b.read_bytes())):
                private_environment = dict(environment)
                private_environment["FLOWSPLICE_PRIVATE_TRUST_FILE"] = os.fspath(root)
                result = subprocess.run(
                    ["bash", os.fspath(COPY_TRUST_RESOURCE_SCRIPT)],
                    env=private_environment,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stderr.decode("utf-8", "replace"))
                self.assertEqual(resource.read_bytes(), expected)
            neutral_environment = dict(environment)
            neutral_environment.pop("FLOWSPLICE_PRIVATE_TRUST_FILE", None)
            result = subprocess.run(
                ["bash", os.fspath(COPY_TRUST_RESOURCE_SCRIPT)],
                env=neutral_environment,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr.decode("utf-8", "replace"))
            self.assertFalse(resource.exists())
            self.assertEqual(root_a.read_text(encoding="ascii"), VALID_ROOT + "\n")
            self.assertEqual(root_b.read_text(encoding="ascii"), alternate_valid_root() + "\n")

    def test_copy_rejects_a_destination_in_a_git_worktree(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repository = self.make_repository(directory)
            source = directory / "outside-root.pub"
            source.write_text(VALID_ROOT, encoding="ascii")
            with self.assertRaises(trust.PackagingError):
                trust.copy_root(source, repository / "bootstrap" / "deployment-root.pub")

    def test_bare_git_metadata_and_linked_worktree_git_file_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            bare = directory / "bare.git"
            subprocess.run(
                ["git", "init", "--bare", "--quiet", os.fspath(bare)],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            with self.assertRaises(trust.PackagingError):
                trust.external_path(os.fspath(bare / "objects" / "private-output"))

            repository = self.make_repository(directory)
            linked_worktree = directory / "linked-worktree"
            subprocess.run(
                [
                    "git",
                    "-C",
                    os.fspath(repository),
                    "worktree",
                    "add",
                    "--detach",
                    os.fspath(linked_worktree),
                ],
                check=True,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            self.assertTrue((linked_worktree / ".git").is_file())
            try:
                with self.assertRaises(trust.PackagingError):
                    trust.external_path(os.fspath(linked_worktree / "private-output"))
            finally:
                subprocess.run(
                    ["git", "-C", os.fspath(repository), "worktree", "remove", "--force", os.fspath(linked_worktree)],
                    check=True,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )

    def test_selected_source_rejects_tracked_bootstrap_material(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repository = self.make_repository(directory)
            asset = repository / "assets" / "bootstrap" / "deployment-root.pub"
            asset.parent.mkdir(parents=True)
            asset.write_text("placeholder", encoding="ascii")
            run_git(repository, "add", "assets/bootstrap/deployment-root.pub")
            run_git(repository, "commit", "-m", "unsafe fixture")
            external_root = directory / "outside-root.pub"
            external_root.write_text(VALID_ROOT, encoding="ascii")
            _source, snapshot = trust.read_root_snapshot(external_root)
            with self.assertRaises(trust.PackagingError):
                trust.verify_source_has_no_private_material(
                    repository, trust.resolve_commit(repository, "HEAD"), snapshot
                )

    def test_selected_source_rejects_the_root_text_from_stdin_grep(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            repository = self.make_repository(directory)
            (repository / "README.md").write_text(VALID_ROOT, encoding="ascii")
            run_git(repository, "add", "README.md")
            run_git(repository, "commit", "-m", "unsafe root fixture")
            external_root = directory / "outside-root.pub"
            external_root.write_text(VALID_ROOT, encoding="ascii")
            _source, snapshot = trust.read_root_snapshot(external_root)
            with self.assertRaises(trust.PackagingError):
                trust.verify_source_has_no_private_material(
                    repository, trust.resolve_commit(repository, "HEAD"), snapshot
                )

    def test_android_command_disables_configuration_cache_without_password_values(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root, output, keystore, profile_path = self.make_external_inputs(Path(temporary))
            profile = trust.load_profile(profile_path)
            with mock.patch.dict(
                os.environ,
                {
                    "PRIVATE_TRAVEL_STORE_PASSWORD": "store-secret-value",
                    "PRIVATE_TRAVEL_KEY_PASSWORD": "key-secret-value",
                },
                clear=False,
            ):
                command = trust.android_gradle_command(profile, root)
            self.assertIn("--no-daemon", command)
            self.assertIn("--no-build-cache", command)
            self.assertIn("--no-configuration-cache", command)
            joined = "\n".join(command)
            self.assertNotIn("store-secret-value", joined)
            self.assertNotIn("key-secret-value", joined)
            self.assertIn("-PflowspliceReleaseStorePasswordEnv=PRIVATE_TRAVEL_STORE_PASSWORD", command)
            self.assertIn("-PflowspliceReleaseKeyPasswordEnv=PRIVATE_TRAVEL_KEY_PASSWORD", command)
            self.assertEqual(output.resolve(), profile.output_directory)
            self.assertEqual(keystore.resolve(), profile.android.keystore_file)

    def test_apk_signature_mismatch_is_rejected_without_real_signing_tools(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            root, _output, _keystore, profile_path = self.make_external_inputs(directory)
            profile = trust.load_profile(profile_path)
            apk = directory / "release.apk"
            with zipfile.ZipFile(apk, "w") as archive:
                archive.writestr("assets/bootstrap/deployment-root.pub", root.read_bytes())
            with mock.patch.object(trust, "_fingerprint_from_keytool", return_value="a" * 64), mock.patch.object(
                trust, "_fingerprint_from_apk", return_value="b" * 64
            ):
                with self.assertRaises(trust.PackagingError):
                    trust.verify_apk(apk, profile, profile.root)

    def test_final_checksum_manifest_covers_android_and_every_apple_file(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            publication = Path(temporary) / "publication"
            publication.mkdir()
            version = "0.0.1"
            for name in trust._release_file_names(version):
                (publication / name).write_bytes(name.encode("ascii"))
            with self.assertRaises(trust.PackagingError):
                trust._verify_publication_layout(publication, version)
            trust._refresh_publication_checksums(publication, version)
            trust._verify_publication_layout(publication, version)
            manifest_names = {
                line.rsplit("  ", 1)[1]
                for line in (publication / "SHA256SUMS").read_text(encoding="ascii").splitlines()
            }
            self.assertEqual(manifest_names, trust._release_file_names(version) - {"SHA256SUMS"})
            android = publication / f"flowsplice-android-{version}-arm64.apk"
            android.write_bytes(b"tampered")
            with self.assertRaises(trust.PackagingError):
                trust._verify_publication_layout(publication, version)

    def test_check_only_validates_without_staging_or_running_release_actions(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            external = directory / "external"
            external.mkdir()
            root, output, _keystore, profile = self.make_external_inputs(external)
            repository = self.make_repository(directory)
            with mock.patch.dict(
                os.environ,
                {
                    "PRIVATE_TRAVEL_STORE_PASSWORD": "store-secret-value",
                    "PRIVATE_TRAVEL_KEY_PASSWORD": "key-secret-value",
                },
                clear=False,
            ), mock.patch.object(trust, "preflight_tools") as preflight, mock.patch.object(
                trust, "run_logged"
            ) as run_logged:
                with contextlib.redirect_stdout(io.StringIO()):
                    result = trust.build_package(profile, check_only=True, repository=repository)
            self.assertIsNone(result)
            preflight.assert_called_once()
            run_logged.assert_not_called()
            self.assertEqual(list(output.iterdir()), [])
            self.assertEqual(root.read_text(encoding="ascii"), VALID_ROOT + "\n")

    def test_existing_final_destination_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            external = directory / "external"
            external.mkdir()
            _root, output, _keystore, profile = self.make_external_inputs(external)
            repository = self.make_repository(directory)
            commit = trust.resolve_commit(repository, "HEAD")
            (output / f"0.0.1-{commit[:12]}").mkdir()
            with mock.patch.dict(
                os.environ,
                {
                    "PRIVATE_TRAVEL_STORE_PASSWORD": "store-secret-value",
                    "PRIVATE_TRAVEL_KEY_PASSWORD": "key-secret-value",
                },
                clear=False,
            ), mock.patch.object(trust, "preflight_tools") as preflight:
                with self.assertRaises(trust.PackagingError):
                    trust.build_package(profile, check_only=True, repository=repository)
            preflight.assert_not_called()

    def test_failed_build_cleans_only_owned_staging_and_keeps_prior_output(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            external = directory / "external"
            external.mkdir()
            root, output, _keystore, profile = self.make_external_inputs(external)
            previous = output / "prior-release"
            previous.mkdir()
            sentinel = previous / "keep.txt"
            sentinel.write_text("previous accepted output", encoding="utf-8")
            repository = self.make_repository(directory)
            with mock.patch.dict(os.environ, {
                "PRIVATE_TRAVEL_STORE_PASSWORD": "fixture-password",
                "PRIVATE_TRAVEL_KEY_PASSWORD": "fixture-password",
            }), mock.patch.object(trust, "preflight_tools"), mock.patch.object(
                trust, "run_logged", side_effect=trust.PackagingError("fixture build failure")
            ):
                with self.assertRaises(trust.PackagingError):
                    trust.build_package(profile, repository=repository)
            self.assertEqual(set(output.iterdir()), {previous})
            self.assertEqual(sentinel.read_text(), "previous accepted output")
            self.assertEqual(root.read_text().strip(), VALID_ROOT)
            self.assertEqual(
                subprocess.check_output(["git", "-C", str(repository), "status", "--porcelain"]),
                b"",
            )

    def test_errors_identify_the_failed_guard_without_revealing_input(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "invalid-root.pub"
            root.write_text("private-sensitive-test-value", encoding="ascii")
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                status = trust.main(["copy-root", "--source", str(root), "--destination", str(root.parent / "copy.pub")])
            self.assertEqual(status, 2)
            self.assertIn("deployment root validation failed", stderr.getvalue())
            self.assertNotIn(root.read_text(), stderr.getvalue())

    def test_cleanup_refuses_replaced_directories_and_reports_removal_failure(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            parent = Path(temporary)
            stage = parent / ".flowsplice-private-stage.fixture"
            stage.mkdir()
            identity = trust._directory_identity(stage)
            moved = parent / "retained-original"
            stage.rename(moved)
            stage.mkdir()
            self.assertFalse(trust._remove_owned_directory(stage, parent, stage.name, identity))
            self.assertTrue(stage.is_dir())
            self.assertTrue(moved.is_dir())
            with mock.patch.object(trust.shutil, "rmtree", side_effect=OSError("fixture")):
                self.assertFalse(trust._remove_owned_directory(
                    stage, parent, stage.name, trust._directory_identity(stage)
                ))


if __name__ == "__main__":
    unittest.main()
