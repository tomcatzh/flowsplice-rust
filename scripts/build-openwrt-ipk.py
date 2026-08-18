#!/usr/bin/env python3
"""Build a deterministic opkg-compatible FlowSplice OpenWrt IPK."""

from __future__ import annotations

import argparse
import gzip
import io
import os
from pathlib import Path
import re
import tarfile
import tempfile

from po2lmo import compile_po


PACKAGE = "flowsplice-openwrt"
EXECUTABLE_PATHS = {
    "etc/init.d/flowsplice",
    "usr/libexec/flowsplice/render-config",
}


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", required=True, type=Path)
    parser.add_argument("--relay", required=True, type=Path)
    parser.add_argument("--architecture", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--release", default="1")
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--source-date-epoch", type=int, default=None)
    return parser.parse_args()


def validate_token(value: str, label: str) -> None:
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+~-]*", value):
        raise ValueError(f"invalid {label}: {value!r}")


def normalized_info(
    name: str, size: int, mode: int, epoch: int, *, is_directory: bool = False
) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.size = size
    info.mode = mode
    if is_directory:
        info.type = tarfile.DIRTYPE
    info.uid = 0
    info.gid = 0
    info.uname = "root"
    info.gname = "root"
    info.mtime = epoch
    return info


def gzip_tar(entries: list[tuple[str, bytes | None, int]], epoch: int) -> bytes:
    output = io.BytesIO()
    with gzip.GzipFile(filename="", fileobj=output, mode="wb", mtime=epoch) as compressed:
        with tarfile.open(fileobj=compressed, mode="w", format=tarfile.GNU_FORMAT) as archive:
            for name, data, mode in entries:
                if data is None:
                    archive.addfile(normalized_info(name, 0, mode, epoch, is_directory=True))
                else:
                    archive.addfile(normalized_info(name, len(data), mode, epoch), io.BytesIO(data))
    return output.getvalue()


def collect_data(
    root: Path, server: Path, relay: Path, license_file: Path, translation: Path
) -> list[tuple[str, bytes | None, int]]:
    files: dict[str, tuple[bytes, int]] = {}
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        relative = path.relative_to(root).as_posix()
        if path.suffix.lower() in {".crt", ".key", ".pem"}:
            raise ValueError(f"refusing to package credential material: {relative}")
        mode = 0o755 if relative in EXECUTABLE_PATHS else 0o644
        files[relative] = (path.read_bytes(), mode)
    for relative, source in {
        "usr/bin/flowsplice-server": server,
        "usr/bin/flowsplice-relay": relay,
    }.items():
        if not source.is_file():
            raise ValueError(f"missing binary: {source}")
        files[relative] = (source.read_bytes(), 0o755)
    files["usr/share/licenses/flowsplice-openwrt/LICENSE"] = (license_file.read_bytes(), 0o644)
    files["usr/lib/lua/luci/i18n/flowsplice.zh-cn.lmo"] = (compile_po(translation), 0o644)
    directories: set[str] = set()
    for name in files:
        parent = Path(name).parent
        while parent != Path("."):
            directories.add(parent.as_posix())
            parent = parent.parent
    entries: list[tuple[str, bytes | None, int]] = [
        (f"./{name}/", None, 0o755) for name in sorted(directories)
    ]
    entries.extend(
        (f"./{name}", data, mode) for name, (data, mode) in sorted(files.items())
    )
    return entries


def control_entries(
    control_dir: Path,
    architecture: str,
    version: str,
    release: str,
    installed_size: int,
) -> list[tuple[str, bytes | None, int]]:
    control = (
        f"Package: {PACKAGE}\n"
        f"Version: {version}-{release}\n"
        f"Architecture: {architecture}\n"
        "Maintainer: FlowSplice\n"
        "Depends: luci-base\n"
        "Section: net\n"
        "Priority: optional\n"
        f"Installed-Size: {installed_size}\n"
        "Description: FlowSplice Server, multi-instance Relay, procd, UCI, and LuCI integration\n"
    ).encode()
    entries = [("./control", control, 0o644)]
    for name in ("conffiles", "postinst", "prerm"):
        path = control_dir / name
        mode = 0o755 if name in {"postinst", "prerm"} else 0o644
        entries.append((f"./{name}", path.read_bytes(), mode))
    return entries


def build(args: argparse.Namespace) -> Path:
    validate_token(args.architecture, "architecture")
    validate_token(args.version, "version")
    validate_token(args.release, "release")
    if args.source_date_epoch is not None:
        epoch = args.source_date_epoch
    else:
        epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    if epoch < 0:
        raise ValueError("source date epoch must not be negative")

    repository = Path(__file__).resolve().parents[1]
    data_entries = collect_data(
        repository / "openwrt/root",
        args.server,
        args.relay,
        repository / "LICENSE",
        repository / "openwrt/po/zh_Hans/flowsplice.po",
    )
    installed_size = (
        sum(len(data) for _, data, _ in data_entries if data is not None) + 1023
    ) // 1024
    data_archive = gzip_tar(data_entries, epoch)
    control_archive = gzip_tar(
        control_entries(
            repository / "openwrt/control",
            args.architecture,
            args.version,
            args.release,
            installed_size,
        ),
        epoch,
    )
    outer = gzip_tar(
        [
            ("./debian-binary", b"2.0\n", 0o644),
            ("./data.tar.gz", data_archive, 0o644),
            ("./control.tar.gz", control_archive, 0o644),
        ],
        epoch,
    )
    args.output_dir.mkdir(parents=True, exist_ok=True)
    destination = args.output_dir / f"{PACKAGE}_{args.version}-{args.release}_{args.architecture}.ipk"
    with tempfile.NamedTemporaryFile(dir=args.output_dir, delete=False) as temporary:
        temporary.write(outer)
        temporary_path = Path(temporary.name)
    temporary_path.chmod(0o644)
    temporary_path.replace(destination)
    return destination


def main() -> None:
    destination = build(arguments())
    print(destination)


if __name__ == "__main__":
    main()
