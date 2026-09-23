#!/usr/bin/env python3
"""Install CASS through immutable executables so live self-spawns survive updates.

The one-time conversion from a regular destination requires an idle service.
Stop the timer and let any running CASS process exit before --bootstrap-regular.
Subsequent installs only replace the symlink and retain prior targets.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import uuid
from pathlib import Path


def digest(path: Path) -> str:
    hasher = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            hasher.update(chunk)
    return hasher.hexdigest()


def running_executables(path: Path) -> list[int]:
    identity = path.stat()
    owners = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            executable = (entry / "exe").stat()
        except (OSError, PermissionError):
            continue
        if (executable.st_dev, executable.st_ino) == (identity.st_dev, identity.st_ino):
            owners.append(int(entry.name))
    return owners


def fsync_directory(directory: Path) -> None:
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def install(source: Path, directory: Path, name: str, bootstrap_regular: bool) -> dict:
    if name in {"", ".", ".."} or Path(name).name != name:
        raise ValueError("name must be one filename")
    source = source.resolve(strict=True)
    if not source.is_file():
        raise ValueError("source must be a regular file")
    directory.mkdir(parents=True, exist_ok=True)
    link = directory / name
    previous_target = None
    regular = link.exists() and not link.is_symlink()
    if regular:
        if not bootstrap_regular:
            raise ValueError("regular destination requires --bootstrap-regular while idle")
        if directory.resolve() == Path("/warp/codex/runtime/codex-memory-hub").resolve():
            for unit in ("cass-incremental.timer", "cass-incremental.service"):
                state = subprocess.run(
                    ["systemctl", "--user", "is-active", unit],
                    capture_output=True,
                    text=True,
                    check=False,
                ).stdout.strip()
                if state in {"active", "activating", "reloading"}:
                    raise ValueError(f"stop {unit} before converting a regular destination")
        owners = running_executables(link)
        if owners:
            raise ValueError(f"regular destination is executing in PIDs {owners}")
        previous_digest = digest(link)
        previous_target = directory / f"{name}.sha256-{previous_digest}"
        try:
            os.link(link, previous_target)
        except FileExistsError:
            if previous_target.is_symlink() or digest(previous_target) != previous_digest:
                raise ValueError("retained previous target has the wrong digest") from None
    elif link.is_symlink():
        old_name = os.readlink(link)
        if Path(old_name).name != old_name:
            raise ValueError("current symlink must target a file in its own directory")
        previous_target = directory / old_name
        if not previous_target.is_file():
            raise ValueError("current symlink target is unavailable")

    source_digest = digest(source)
    target = directory / f"{name}.sha256-{source_digest}"
    if target.exists():
        if target.is_symlink() or not target.is_file() or digest(target) != source_digest:
            raise ValueError("immutable target has the wrong digest")
        if not os.access(target, os.X_OK):
            raise ValueError("immutable target is not executable")
    else:
        staged = directory / f".{name}.stage-{uuid.uuid4().hex}"
        try:
            with source.open("rb") as input_file, staged.open("xb") as output_file:
                shutil.copyfileobj(input_file, output_file, 1024 * 1024)
                os.fchmod(output_file.fileno(), 0o755)
                output_file.flush()
                os.fsync(output_file.fileno())
            if digest(staged) != source_digest:
                raise ValueError("staged binary digest differs from source")
            try:
                os.link(staged, target)
            except FileExistsError:
                if digest(target) != source_digest:
                    raise ValueError("immutable target has the wrong digest") from None
        finally:
            staged.unlink(missing_ok=True)
    fsync_directory(directory)

    if regular:
        owners = running_executables(link)
        if owners:
            raise ValueError(f"regular destination started executing in PIDs {owners}")
    staged_link = directory / f".{name}.link-{uuid.uuid4().hex}"
    try:
        staged_link.symlink_to(target.name)
        os.replace(staged_link, link)
    finally:
        staged_link.unlink(missing_ok=True)
    fsync_directory(directory)
    return {
        "schema": "cass.immutable-shared-install.v1",
        "status": "INSTALLED",
        "link": str(link),
        "target": str(target),
        "sha256": source_digest,
        "previous_target_retained": str(previous_target) if previous_target else None,
        "bootstrapped_regular_destination": regular,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path, help="built CASS executable to install")
    parser.add_argument(
        "--install-dir",
        type=Path,
        default=Path("/warp/codex/runtime/codex-memory-hub"),
    )
    parser.add_argument("--name", default="cass-real")
    parser.add_argument(
        "--bootstrap-regular",
        action="store_true",
        help="one-time regular-file conversion; timer and running CASS must be stopped",
    )
    args = parser.parse_args()
    print(
        json.dumps(
            install(args.source, args.install_dir, args.name, args.bootstrap_regular),
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
