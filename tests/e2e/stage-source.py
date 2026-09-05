#!/usr/bin/env python3
"""Copy current source files to an empty external directory for native E2E builds."""

from pathlib import Path
import shutil
import subprocess
import sys


def stage_source(repository: Path, destination: Path) -> None:
    repository = repository.resolve(strict=True)
    subprocess.run(
        [
            sys.executable,
            str(repository / "scripts/private-travel-trust.py"),
            "check-path", "--path", str(destination),
        ],
        check=True,
    )
    destination.mkdir(mode=0o700, parents=True, exist_ok=False)
    files = subprocess.check_output(
        ["git", "-C", str(repository), "ls-files", "--cached", "--others", "--exclude-standard", "-z"]
    )
    excluded = {".git", ".idea", ".kotlin", ".llmwiki", ".agents", ".codex", "build", "target", "dist", "node_modules"}
    for entry in set(files.split(b"\0")):
        if not entry:
            continue
        relative = Path(entry.decode("utf-8"))
        if relative.is_absolute() or ".." in relative.parts:
            raise ValueError("invalid source path")
        if excluded.intersection(relative.parts) or relative.name in {"AGENTS.md", "CLAUDE.md", "local.properties"}:
            continue
        source = repository / relative
        if not source.exists():
            continue  # Preserve deletions in the current checkout.
        if source.is_symlink() or not source.resolve().is_relative_to(repository):
            raise ValueError("E2E source must not contain symlinks")
        if source.is_file():
            target = destination / relative
            target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            shutil.copy2(source, target)


if __name__ == "__main__":
    stage_source(Path(sys.argv[1]), Path(sys.argv[2]))
