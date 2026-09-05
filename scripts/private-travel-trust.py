#!/usr/bin/env python3
"""Private Travel package trust-input validation and orchestration.

The helper deliberately keeps deployment roots, certificate fingerprints, and
password values out of stdout, stderr, and command-line arguments.  It is also
used by the Apple Xcode resource phase, so its small file-copy commands have no
release-tool dependencies.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import signal
import stat
import subprocess
import sys
import tarfile
import tempfile
from typing import Mapping, Sequence
import zipfile


SCRIPT_PATH = Path(__file__).resolve()
REPOSITORY_ROOT = SCRIPT_PATH.parent.parent
MAX_ROOT_FILE_BYTES = 256
P256_PRIME = 0xFFFFFFFF00000001000000000000000000000000FFFFFFFFFFFFFFFFFFFFFFFF
P256_A = P256_PRIME - 3
P256_B = 0x5AC635D8AA3A93E7B3EBBD55769886BC651D06B0CC53B0F63BCE3C3E27D2604B
ROOT_HEX_RE = re.compile(rb"[0-9a-fA-F]{130}\Z")
SAFE_VERSION_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+-]*\Z")
ENVIRONMENT_NAME_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*\Z")


class PackagingError(RuntimeError):
    """An intentionally non-sensitive packaging failure."""


@dataclass(frozen=True)
class RootSnapshot:
    raw: bytes
    point_hex: bytes


@dataclass(frozen=True)
class AndroidSigning:
    keystore_file: Path
    key_alias: str
    store_password_env: str
    key_password_env: str


@dataclass(frozen=True)
class AppleSigning:
    team_id: str
    notary_profile: str
    ios_device: str


@dataclass(frozen=True)
class PrivateProfile:
    root_file: Path
    root: RootSnapshot
    output_directory: Path
    android: AndroidSigning
    apple: AppleSigning


class SafeArgumentParser(argparse.ArgumentParser):
    def error(self, _message: str) -> None:
        raise PackagingError("invalid command")


def _failure(message: str) -> None:
    raise PackagingError(message)


def _absolute_path(value: str) -> Path:
    if not isinstance(value, str) or not value or any(ord(character) < 32 for character in value):
        _failure("an absolute path without control characters is required")
    try:
        path = Path(value)
    except (TypeError, ValueError):
        _failure("an absolute path is required")
    if not path.is_absolute():
        _failure("an absolute path is required")
    return Path(os.path.abspath(os.fspath(path)))


def _nearest_existing(path: Path) -> Path:
    candidate = path
    while not os.path.lexists(candidate):
        parent = candidate.parent
        if parent == candidate:
            _failure("external path validation failed")
        candidate = parent
    if candidate.is_dir():
        return candidate
    return candidate.parent


def _clear_git_environment() -> dict[str, str]:
    environment = dict(os.environ)
    for name in ("GIT_DIR", "GIT_WORK_TREE", "GIT_COMMON_DIR"):
        environment.pop(name, None)
    return environment


def _git_marks_path(directory: Path) -> bool:
    """Return whether an existing directory belongs to a worktree or Git data."""
    try:
        for option in ("--is-inside-work-tree", "--is-inside-git-dir"):
            result = subprocess.run(
                ["git", "-C", os.fspath(directory), "rev-parse", option],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                env=_clear_git_environment(),
                check=False,
                timeout=5,
            )
            if result.returncode == 0 and result.stdout.strip() == b"true":
                return True
    except (OSError, subprocess.TimeoutExpired):
        _failure("external path validation failed")
    return False


def _has_git_marker(path: Path) -> bool:
    """Catch worktree .git files as well as ordinary repository directories."""
    current = _nearest_existing(path)
    while True:
        if os.path.lexists(current / ".git"):
            return True
        parent = current.parent
        if parent == current:
            return False
        current = parent


def external_path(
    value: str,
    *,
    must_exist: bool = False,
    require_file: bool = False,
    require_directory: bool = False,
) -> Path:
    """Return a canonical external path, rejecting every Git containment mode."""
    if require_file and require_directory:
        _failure("external path validation failed")
    lexical = _absolute_path(value)
    try:
        resolved = lexical.resolve(strict=False)
    except (OSError, RuntimeError):
        _failure("external path validation failed")
    for candidate in (lexical, resolved):
        if _has_git_marker(candidate) or _git_marks_path(_nearest_existing(candidate)):
            _failure("private inputs and outputs must be outside Git")
    if must_exist and not resolved.exists():
        _failure("required external path is unavailable")
    if require_file:
        try:
            mode = resolved.stat().st_mode
        except OSError:
            _failure("required external file is unavailable")
        if not stat.S_ISREG(mode):
            _failure("required external file is unavailable")
    if require_directory:
        try:
            mode = resolved.stat().st_mode
        except OSError:
            _failure("required external directory is unavailable")
        if not stat.S_ISDIR(mode):
            _failure("required external directory is unavailable")
    return resolved


def validate_root_bytes(raw: bytes) -> RootSnapshot:
    if not isinstance(raw, bytes) or len(raw) == 0 or len(raw) > MAX_ROOT_FILE_BYTES:
        _failure("deployment root validation failed")
    point_hex = raw.strip()
    if not ROOT_HEX_RE.fullmatch(point_hex):
        _failure("deployment root validation failed")
    try:
        point = bytes.fromhex(point_hex.decode("ascii"))
    except (UnicodeDecodeError, ValueError):
        _failure("deployment root validation failed")
    if len(point) != 65 or point[0] != 0x04:
        _failure("deployment root validation failed")
    x = int.from_bytes(point[1:33], "big")
    y = int.from_bytes(point[33:65], "big")
    if x >= P256_PRIME or y >= P256_PRIME:
        _failure("deployment root validation failed")
    if (y * y - (x * x * x + P256_A * x + P256_B)) % P256_PRIME != 0:
        _failure("deployment root validation failed")
    return RootSnapshot(raw=raw, point_hex=point_hex.lower())


def read_root_snapshot(source: str | Path) -> tuple[Path, RootSnapshot]:
    source_path = external_path(os.fspath(source), must_exist=True, require_file=True)
    try:
        with source_path.open("rb") as handle:
            raw = handle.read(MAX_ROOT_FILE_BYTES + 1)
    except OSError:
        _failure("deployment root validation failed")
    return source_path, validate_root_bytes(raw)


def _remove_file_if_present(path: Path) -> None:
    try:
        path.unlink()
    except FileNotFoundError:
        return


def write_frozen_root(snapshot: RootSnapshot, destination: str | Path) -> Path:
    """Atomically write an already-read root without re-reading its source."""
    target = external_path(os.fspath(destination))
    try:
        target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    except OSError:
        _failure("deployment root copy failed")
    target = external_path(os.fspath(target))
    if target.exists() and target.is_dir():
        _failure("deployment root copy failed")
    temporary: Path | None = None
    try:
        descriptor, temporary_name = tempfile.mkstemp(
            prefix=".flowsplice-root-", dir=os.fspath(target.parent)
        )
        temporary = Path(temporary_name)
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(snapshot.raw)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, target)
    except OSError:
        _failure("deployment root copy failed")
    finally:
        if temporary is not None:
            _remove_file_if_present(temporary)
    return target


def copy_root(source: str | Path, destination: str | Path) -> Path:
    _source_path, snapshot = read_root_snapshot(source)
    return write_frozen_root(snapshot, destination)


def verify_root(source: str | Path, artifact_resource: str | Path) -> None:
    _source_path, expected = read_root_snapshot(source)
    _artifact_path, actual = read_root_snapshot(artifact_resource)
    if expected.raw != actual.raw:
        _failure("deployment root verification failed")


def _strict_table(payload: object, expected_keys: set[str]) -> dict[str, object]:
    if not isinstance(payload, dict) or set(payload) != expected_keys:
        _failure("private profile validation failed")
    return payload


def _profile_text(table: Mapping[str, object], name: str) -> str:
    value = table.get(name)
    if not isinstance(value, str) or not value or "\x00" in value:
        _failure("private profile validation failed")
    if any(ord(character) < 32 for character in value):
        _failure("private profile validation failed")
    return value


def _profile_path(table: Mapping[str, object], name: str, *, directory: bool = False) -> Path:
    value = _profile_text(table, name)
    return external_path(
        value,
        must_exist=True,
        require_file=not directory,
        require_directory=directory,
    )


def load_profile(profile_file: str | Path) -> PrivateProfile:
    # Xcode can invoke copy-root with Apple's Python 3.9. Only profile parsing
    # needs Python 3.11's TOML library.
    try:
        import tomllib
    except ImportError:
        _failure("Python 3.11 or newer is required for private profiles")
    canonical_profile = external_path(
        os.fspath(profile_file), must_exist=True, require_file=True
    )
    try:
        with canonical_profile.open("rb") as handle:
            profile_bytes = handle.read(65_537)
        if len(profile_bytes) > 65_536:
            _failure("private profile is too large")
        profile = tomllib.loads(profile_bytes.decode("utf-8"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError):
        _failure("private profile validation failed")
    top = _strict_table(profile, {"trust", "output", "android", "apple"})
    trust = _strict_table(top["trust"], {"deployment_root_public_key"})
    output = _strict_table(top["output"], {"directory"})
    android = _strict_table(
        top["android"],
        {"keystore_file", "key_alias", "store_password_env", "key_password_env"},
    )
    apple = _strict_table(top["apple"], {"team_id", "notary_profile", "ios_device"})

    root_file = _profile_path(trust, "deployment_root_public_key")
    _root_path, root = read_root_snapshot(root_file)
    output_directory = _profile_path(output, "directory", directory=True)
    keystore_file = _profile_path(android, "keystore_file")
    key_alias = _profile_text(android, "key_alias")
    store_password_env = _profile_text(android, "store_password_env")
    key_password_env = _profile_text(android, "key_password_env")
    if not ENVIRONMENT_NAME_RE.fullmatch(store_password_env) or not ENVIRONMENT_NAME_RE.fullmatch(
        key_password_env
    ):
        _failure("private profile validation failed")
    team_id = _profile_text(apple, "team_id")
    notary_profile = _profile_text(apple, "notary_profile")
    ios_device = _profile_text(apple, "ios_device")
    return PrivateProfile(
        root_file=root_file,
        root=root,
        output_directory=output_directory,
        android=AndroidSigning(
            keystore_file=keystore_file,
            key_alias=key_alias,
            store_password_env=store_password_env,
            key_password_env=key_password_env,
        ),
        apple=AppleSigning(
            team_id=team_id,
            notary_profile=notary_profile,
            ios_device=ios_device,
        ),
    )


def _git_result(
    repository: Path,
    arguments: Sequence[str],
    *,
    input_data: bytes | None = None,
    timeout: int = 30,
) -> subprocess.CompletedProcess[bytes]:
    try:
        return subprocess.run(
            ["git", "-C", os.fspath(repository), *arguments],
            input=input_data,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            check=False,
            env=_clear_git_environment(),
            timeout=timeout,
        )
    except (OSError, subprocess.TimeoutExpired):
        _failure("source repository validation failed")


def resolve_commit(repository: Path, ref: str) -> str:
    if not isinstance(ref, str) or not ref or "\x00" in ref:
        _failure("source reference validation failed")
    result = _git_result(
        repository, ["rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}"]
    )
    try:
        commit = result.stdout.decode("ascii").strip()
    except UnicodeDecodeError:
        _failure("source reference validation failed")
    if result.returncode != 0 or not re.fullmatch(r"[0-9a-f]{40,64}", commit):
        _failure("source reference validation failed")
    return commit


def version_at_commit(repository: Path, commit: str) -> str:
    result = _git_result(repository, ["show", f"{commit}:Cargo.toml"])
    if result.returncode != 0:
        _failure("source version validation failed")
    try:
        manifest = result.stdout.decode("utf-8")
    except UnicodeDecodeError:
        _failure("source version validation failed")
    match = re.search(r'^version\s*=\s*"([^"]+)"\s*$', manifest, re.MULTILINE)
    if match is None or not SAFE_VERSION_RE.fullmatch(match.group(1)):
        _failure("source version validation failed")
    return match.group(1)


def _nul_paths(result: subprocess.CompletedProcess[bytes]) -> list[str]:
    if result.returncode != 0:
        _failure("source repository validation failed")
    paths: list[str] = []
    for entry in result.stdout.split(b"\0"):
        if entry:
            paths.append(entry.decode("utf-8", "surrogateescape"))
    return paths


def _sensitive_tracked_path(path: str) -> bool:
    normalized = PurePosixPath(path)
    parts = tuple(part.lower() for part in normalized.parts)
    if not parts or any(part in ("", ".", "..") for part in parts):
        return True
    filename = parts[-1]
    if filename in {"agents.md", "claude.md"}:
        return True
    if filename.endswith(
        (".key", ".pem", ".crt", ".cer", ".p12", ".pfx", ".jks", ".keystore", ".mobileprovision", ".pub")
    ):
        return True
    if any(part in {"key", "keys", "cert", "certs", "secret", "secrets", "credential", "credentials"} for part in parts[:-1]):
        return True
    if any(part in {"asset", "assets", "bootstrap"} for part in parts[:-1]) and any(
        token in filename for token in ("deployment", "root", "trust", "key", "cert", "private")
    ):
        return True
    return False


def verify_source_has_no_private_material(repository: Path, commit: str, root: RootSnapshot) -> None:
    root_pattern = root.point_hex + b"\n"
    checks = (
        ["grep", "--cached", "--quiet", "--ignore-case", "--fixed-strings", "-f", "-"],
        ["grep", "--quiet", "--ignore-case", "--fixed-strings", "-f", "-", commit],
    )
    for command in checks:
        result = _git_result(repository, command, input_data=root_pattern)
        if result.returncode == 0:
            _failure("private deployment root is present in source control")
        if result.returncode != 1:
            _failure("source repository validation failed")
    index_paths = _nul_paths(_git_result(repository, ["ls-files", "-z"]))
    commit_paths = _nul_paths(
        _git_result(repository, ["ls-tree", "-r", "-z", "--name-only", commit])
    )
    if any(_sensitive_tracked_path(path) for path in [*index_paths, *commit_paths]):
        _failure("private material path is present in source control")


def _verify_source_entry(
    repository: Path, commit: str, path: str, *, executable: bool = False
) -> None:
    result = _git_result(repository, ["cat-file", "-e", f"{commit}:{path}"])
    if result.returncode != 0:
        _failure("selected source is missing required packaging input")
    if executable:
        mode_result = _git_result(repository, ["ls-tree", commit, "--", path])
        if mode_result.returncode != 0 or not mode_result.stdout.startswith(b"100755 "):
            _failure("selected source is missing required packaging input")


REQUIRED_TOOLS = (
    "apksigner",
    "bash",
    "cargo",
    "codesign",
    "ditto",
    "file",
    "git",
    "hdiutil",
    "keytool",
    "lipo",
    "ln",
    "npm",
    "otool",
    "plutil",
    "python3",
    "security",
    "shasum",
    "spctl",
    "unzip",
    "xattr",
    "xcodebuild",
    "xcrun",
)


def preflight_tools(repository: Path, commit: str) -> None:
    missing = [tool for tool in REQUIRED_TOOLS if shutil.which(tool) is None]
    if missing:
        _failure("required packaging tools are unavailable: " + ", ".join(missing))
    _verify_source_entry(repository, commit, "travel-android/gradlew", executable=True)
    _verify_source_entry(repository, commit, "scripts/build-apple-products.sh")
    _verify_source_entry(repository, commit, "scripts/private-travel-trust.py")


def require_password_environment(profile: PrivateProfile) -> None:
    for name in (profile.android.store_password_env, profile.android.key_password_env):
        if not os.environ.get(name):
            _failure("required password environment is unavailable")


def final_release_path(profile: PrivateProfile, version: str, commit: str) -> Path:
    return profile.output_directory / f"{version}-{commit[:12]}"


def _path_lexists(path: Path) -> bool:
    return os.path.lexists(os.fspath(path))


def ensure_final_absent(path: Path) -> None:
    if _path_lexists(path):
        _failure("private release destination already exists")


def _safe_extract_archive(repository: Path, commit: str, source_directory: Path) -> None:
    try:
        process = subprocess.Popen(
            ["git", "-C", os.fspath(repository), "archive", "--format=tar", commit],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=_clear_git_environment(),
        )
    except OSError:
        _failure("source archive creation failed")
    assert process.stdout is not None
    try:
        with tarfile.open(fileobj=process.stdout, mode="r|") as archive:
            for member in archive:
                relative = PurePosixPath(member.name)
                if relative.is_absolute() or ".." in relative.parts or not relative.parts:
                    _failure("source archive validation failed")
                destination = source_directory.joinpath(*relative.parts)
                try:
                    destination.relative_to(source_directory)
                except ValueError:
                    _failure("source archive validation failed")
                if member.isdir():
                    destination.mkdir(mode=0o700, parents=True, exist_ok=True)
                    continue
                if not member.isreg():
                    _failure("source archive validation failed")
                destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
                extracted = archive.extractfile(member)
                if extracted is None:
                    _failure("source archive validation failed")
                with extracted, destination.open("wb") as handle:
                    shutil.copyfileobj(extracted, handle)
                os.chmod(destination, member.mode & 0o777)
    except (OSError, tarfile.TarError):
        _failure("source archive extraction failed")
    finally:
        process.stdout.close()
    if process.wait() != 0 or os.path.lexists(source_directory / ".git"):
        _failure("source archive validation failed")


def _stage_environment(stage_root: Path) -> dict[str, str]:
    temporary = stage_root / "tmp"
    cargo_target = stage_root / "cargo-target"
    gradle_home = stage_root / "gradle-home"
    for directory in (temporary, cargo_target, gradle_home, stage_root / "logs", stage_root / "temp"):
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    environment = dict(os.environ)
    environment.update(
        {
            "TMPDIR": os.fspath(temporary),
            "CARGO_TARGET_DIR": os.fspath(cargo_target),
            "GRADLE_USER_HOME": os.fspath(gradle_home),
        }
    )
    return environment


def run_logged(
    arguments: Sequence[str],
    *,
    cwd: Path,
    environment: Mapping[str, str],
    log_path: Path,
) -> None:
    """Run a release command without writing its arguments or output to the console."""
    try:
        descriptor = os.open(os.fspath(log_path), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "wb") as log:
            result = subprocess.run(
                list(arguments),
                cwd=os.fspath(cwd),
                env=dict(environment),
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
                check=False,
            )
    except OSError:
        _failure("private packaging command could not run")
    if result.returncode != 0:
        _failure("private packaging command failed")


def android_gradle_command(profile: PrivateProfile, frozen_root: Path) -> tuple[str, ...]:
    return (
        "./gradlew",
        "--no-daemon",
        "--no-build-cache",
        "--no-configuration-cache",
        f"-PflowspliceDeploymentRootFile={frozen_root}",
        f"-PflowspliceReleaseKeystoreFile={profile.android.keystore_file}",
        f"-PflowspliceReleaseKeyAlias={profile.android.key_alias}",
        f"-PflowspliceReleaseStorePasswordEnv={profile.android.store_password_env}",
        f"-PflowspliceReleaseKeyPasswordEnv={profile.android.key_password_env}",
        ":app:assembleRelease",
    )


def _fingerprint_from_keytool(profile: PrivateProfile) -> str:
    password = os.environ.get(profile.android.store_password_env)
    if not password:
        _failure("required password environment is unavailable")
    try:
        result = subprocess.run(
            [
                "keytool",
                "-list",
                "-v",
                "-keystore",
                os.fspath(profile.android.keystore_file),
                "-alias",
                profile.android.key_alias,
            ],
            input=(password + "\n").encode("utf-8"),
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
    except OSError:
        _failure("keystore verification failed")
    if result.returncode != 0:
        _failure("keystore verification failed")
    return _extract_sha256_fingerprint(result.stdout)


def _extract_sha256_fingerprint(output: bytes) -> str:
    match = re.search(
        rb"SHA(?:-| )?256(?:[ -]+(?:certificate[ -]+)?digest)?\s*:\s*([0-9A-Fa-f:]{32,})",
        output,
        re.IGNORECASE,
    )
    if match is None:
        _failure("signature verification failed")
    return match.group(1).replace(b":", b"").lower().decode("ascii")


def _fingerprint_from_apk(apk: Path) -> str:
    try:
        result = subprocess.run(
            ["apksigner", "verify", "--print-certs", os.fspath(apk)],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
    except OSError:
        _failure("APK signature verification failed")
    if result.returncode != 0:
        _failure("APK signature verification failed")
    return _extract_sha256_fingerprint(result.stdout)


def verify_apk(apk: Path, profile: PrivateProfile, root: RootSnapshot) -> None:
    try:
        with zipfile.ZipFile(apk) as archive:
            resources = [
                member
                for member in archive.infolist()
                if member.filename == "assets/bootstrap/deployment-root.pub"
            ]
            if len(resources) != 1 or resources[0].file_size > MAX_ROOT_FILE_BYTES:
                _failure("APK deployment root verification failed")
            artifact_root = archive.read(resources[0])
    except (OSError, zipfile.BadZipFile, KeyError):
        _failure("APK deployment root verification failed")
    actual = validate_root_bytes(artifact_root)
    if actual.raw != root.raw:
        _failure("APK deployment root verification failed")
    if _fingerprint_from_keytool(profile) != _fingerprint_from_apk(apk):
        _failure("APK signature verification failed")


def _copy_android_apk(source_directory: Path, publication_directory: Path, version: str) -> Path:
    release_directory = source_directory / "travel-android" / "app" / "build" / "outputs" / "apk" / "release"
    candidates = sorted(path for path in release_directory.glob("*.apk") if path.is_file())
    if len(candidates) != 1:
        _failure("Android release output validation failed")
    target = publication_directory / f"flowsplice-android-{version}-arm64.apk"
    try:
        shutil.copyfile(candidates[0], target)
        os.chmod(target, 0o600)
    except OSError:
        _failure("Android release output validation failed")
    return target


def _apple_output_names(version: str) -> set[str]:
    cli_name = f"flowsplice-cli-{version}-macos-arm64"
    mac_name = f"flowsplice-macos-{version}-arm64"
    ios_name = f"flowsplice-ios-{version}-arm64"
    return {
        f"{cli_name}.zip",
        f"{cli_name}.notary.json",
        f"{cli_name}.notary-log.json",
        f"{mac_name}.dmg",
        f"{mac_name}.notary.json",
        f"{mac_name}.notary-log.json",
        f"{ios_name}.ipa",
        f"{ios_name}.provisioning.json",
        "SHA256SUMS",
    }


def _move_apple_publication(
    apple_directory: Path, publication_directory: Path, version: str
) -> None:
    expected = _apple_output_names(version)
    try:
        entries = list(apple_directory.iterdir())
    except OSError:
        _failure("Apple release output validation failed")
    if {entry.name for entry in entries} != expected or any(not entry.is_file() for entry in entries):
        _failure("Apple release output validation failed")
    try:
        for name in sorted(expected):
            os.replace(apple_directory / name, publication_directory / name)
        apple_directory.rmdir()
    except OSError:
        _failure("Apple release output validation failed")


def _release_file_names(version: str) -> set[str]:
    return _apple_output_names(version) | {f"flowsplice-android-{version}-arm64.apk"}


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    try:
        with path.open("rb") as handle:
            while chunk := handle.read(1024 * 1024):
                digest.update(chunk)
    except OSError:
        _failure("private publication validation failed")
    return digest.hexdigest()


def _refresh_publication_checksums(publication_directory: Path, version: str) -> None:
    """Replace the Apple-only manifest after adding the verified Android APK."""
    names = sorted(_release_file_names(version) - {"SHA256SUMS"})
    checksum_file = publication_directory / "SHA256SUMS"
    temporary: Path | None = None
    try:
        descriptor, temporary_name = tempfile.mkstemp(
            prefix=".flowsplice-sha256-", dir=os.fspath(publication_directory)
        )
        temporary = Path(temporary_name)
        os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "w", encoding="ascii", newline="\n") as handle:
            for name in names:
                handle.write(f"{_sha256(publication_directory / name)}  {name}\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, checksum_file)
    except OSError:
        _failure("private publication validation failed")
    finally:
        if temporary is not None:
            _remove_file_if_present(temporary)


def _verify_publication_checksums(publication_directory: Path, version: str) -> None:
    expected_names = _release_file_names(version) - {"SHA256SUMS"}
    checksum_file = publication_directory / "SHA256SUMS"
    try:
        lines = checksum_file.read_text(encoding="ascii").splitlines()
    except (OSError, UnicodeDecodeError):
        _failure("private publication validation failed")
    recorded: dict[str, str] = {}
    for line in lines:
        match = re.fullmatch(r"([0-9a-f]{64})  ([^/\\]+)", line)
        if match is None or match.group(2) in recorded:
            _failure("private publication validation failed")
        recorded[match.group(2)] = match.group(1)
    if set(recorded) != expected_names:
        _failure("private publication validation failed")
    for name, digest in recorded.items():
        if digest != _sha256(publication_directory / name):
            _failure("private publication validation failed")


def _verify_publication_layout(publication_directory: Path, version: str) -> None:
    try:
        entries = list(publication_directory.iterdir())
    except OSError:
        _failure("private publication validation failed")
    if {entry.name for entry in entries} != _release_file_names(version) or any(
        not entry.is_file() for entry in entries
    ):
        _failure("private publication validation failed")
    _verify_publication_checksums(publication_directory, version)


def _directory_identity(path: Path) -> tuple[int, int]:
    try:
        details = path.lstat()
    except OSError:
        _failure("private package staging failed")
    if not stat.S_ISDIR(details.st_mode):
        _failure("private package staging failed")
    return details.st_dev, details.st_ino


def _remove_owned_directory(
    path: Path, parent: Path, prefix: str, identity: tuple[int, int] | None
) -> bool:
    if identity is None:
        return True
    if path.parent != parent or not path.name.startswith(prefix):
        return False
    try:
        details = path.lstat()
        if (details.st_dev, details.st_ino) != identity:
            return False
        if not stat.S_ISDIR(details.st_mode):
            return False
        shutil.rmtree(path)
        return True
    except FileNotFoundError:
        return True
    except OSError:
        return False


def _install_cleanup_signals() -> tuple[object, object]:
    def interrupted(_signum: int, _frame: object) -> None:
        raise PackagingError("private packaging interrupted")

    old_int = signal.getsignal(signal.SIGINT)
    old_term = signal.getsignal(signal.SIGTERM)
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGTERM, interrupted)
    return old_int, old_term


def _restore_cleanup_signals(previous: tuple[object, object]) -> None:
    signal.signal(signal.SIGINT, previous[0])
    signal.signal(signal.SIGTERM, previous[1])


def build_package(
    profile_file: str | Path,
    *,
    ref: str = "HEAD",
    check_only: bool = False,
    repository: Path = REPOSITORY_ROOT,
) -> Path | None:
    """Validate and build an isolated private release from one committed tree."""
    profile = load_profile(profile_file)
    repository = Path(repository).resolve()
    commit = resolve_commit(repository, ref)
    version = version_at_commit(repository, commit)
    verify_source_has_no_private_material(repository, commit, profile.root)
    release = final_release_path(profile, version, commit)
    ensure_final_absent(release)
    require_password_environment(profile)
    preflight_tools(repository, commit)
    if check_only:
        print(
            "Private Travel package check passed; build, signing, notarization, device installation, and device launch remain pending."
        )
        return None

    lock = profile.output_directory / ".flowsplice-private-package.lock"
    stage_root: Path | None = None
    stage_identity: tuple[int, int] | None = None
    lock_identity: tuple[int, int] | None = None
    signals = _install_cleanup_signals()
    try:
        try:
            os.mkdir(lock, 0o700)
            lock_identity = _directory_identity(lock)
        except FileExistsError:
            _failure("another private package build is active")
        except OSError:
            _failure("private package output lock failed")
        ensure_final_absent(release)
        try:
            stage_root = Path(
                tempfile.mkdtemp(
                    prefix=".flowsplice-private-stage.", dir=os.fspath(profile.output_directory)
                )
            )
            os.chmod(stage_root, 0o700)
            stage_identity = _directory_identity(stage_root)
        except OSError:
            _failure("private package staging failed")
        source_directory = stage_root / "source"
        publication_directory = stage_root / "publication"
        source_directory.mkdir(mode=0o700)
        publication_directory.mkdir(mode=0o700)
        _safe_extract_archive(repository, commit, source_directory)
        environment = _stage_environment(stage_root)
        frozen_root = write_frozen_root(profile.root, stage_root / "trust" / "deployment-root.pub")

        run_logged(
            android_gradle_command(profile, frozen_root),
            cwd=source_directory / "travel-android",
            environment=environment,
            log_path=stage_root / "logs" / "android.log",
        )
        apk = _copy_android_apk(source_directory, publication_directory, version)
        verify_apk(apk, profile, profile.root)

        apple_output = publication_directory / "apple"
        apple_environment = dict(environment)
        apple_environment.update(
            {
                "FLOWSPLICE_PRIVATE_TRUST_FILE": os.fspath(frozen_root),
                "FLOWSPLICE_APPLE_OUTPUT_DIR": os.fspath(apple_output),
                "FLOWSPLICE_APPLE_STAGE_PARENT": os.fspath(stage_root / "temp"),
                "FLOWSPLICE_APPLE_TEAM_ID": profile.apple.team_id,
                "FLOWSPLICE_NOTARY_PROFILE": profile.apple.notary_profile,
                "FLOWSPLICE_IOS_DEVICE": profile.apple.ios_device,
            }
        )
        run_logged(
            ["bash", os.fspath(source_directory / "scripts" / "build-apple-products.sh")],
            cwd=source_directory,
            environment=apple_environment,
            log_path=stage_root / "logs" / "apple.log",
        )
        _move_apple_publication(apple_output, publication_directory, version)
        _refresh_publication_checksums(publication_directory, version)
        _verify_publication_layout(publication_directory, version)
        verify_source_has_no_private_material(repository, commit, profile.root)
        ensure_final_absent(release)
        try:
            os.rename(publication_directory, release)
        except OSError:
            _failure("private release publication failed")
        return release
    finally:
        _restore_cleanup_signals(signals)
        cleaned = True
        if stage_root is not None:
            cleaned = _remove_owned_directory(
                stage_root,
                profile.output_directory,
                ".flowsplice-private-stage.",
                stage_identity,
            )
        lock_cleaned = _remove_owned_directory(
            lock,
            profile.output_directory,
            ".flowsplice-private-package.lock",
            lock_identity,
        )
        if not cleaned or not lock_cleaned:
            _failure("private staging cleanup is incomplete; inspect the external output directory")


def _build_parser() -> SafeArgumentParser:
    parser = SafeArgumentParser(prog="private-travel-trust.py", add_help=True)
    subcommands = parser.add_subparsers(dest="command", required=True)
    check_path = subcommands.add_parser("check-path")
    check_path.add_argument("--path", required=True)
    copy = subcommands.add_parser("copy-root")
    copy.add_argument("--source", required=True)
    copy.add_argument("--destination", required=True)
    verify = subcommands.add_parser("verify-root")
    verify.add_argument("--source", required=True)
    verify.add_argument("--artifact-resource", required=True)
    profile = subcommands.add_parser("check-profile")
    profile.add_argument("--profile-file", required=True)
    build = subcommands.add_parser("build")
    build.add_argument("--profile-file", required=True)
    build.add_argument("--ref", default="HEAD")
    build.add_argument("--check-only", action="store_true")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    parser = _build_parser()
    try:
        arguments = parser.parse_args(argv)
        if arguments.command == "check-path":
            external_path(arguments.path)
            print("External path check passed.")
        elif arguments.command == "copy-root":
            copy_root(arguments.source, arguments.destination)
            print("Deployment root copy passed.")
        elif arguments.command == "verify-root":
            verify_root(arguments.source, arguments.artifact_resource)
            print("Deployment root verification passed.")
        elif arguments.command == "check-profile":
            load_profile(arguments.profile_file)
            print("Private Travel profile check passed.")
        elif arguments.command == "build":
            release = build_package(
                arguments.profile_file, ref=arguments.ref, check_only=arguments.check_only
            )
            if release is not None:
                print("Private Travel package build passed.")
        else:
            _failure("invalid command")
    except PackagingError as error:
        # PackagingError messages are fixed, non-sensitive explanations, never input values.
        print(f"Private Travel packaging failed: {error}.", file=sys.stderr)
        return 2
    except (argparse.ArgumentError, OSError, ValueError):
        print("Private Travel packaging validation failed.", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
