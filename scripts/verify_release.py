#!/usr/bin/env python3
"""Validate a complete ma-demo release without extracting or executing binaries."""

import argparse
import hashlib
from pathlib import Path
import re
import tarfile


TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)


def verify(directory: Path, version: str) -> None:
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
        raise ValueError("release version must be x.y.z")
    archives = [f"ma-demo-{version}-{target}.tar.gz" for target in TARGETS]
    expected = {"install.sh", *archives, *(f"{name}.sha256" for name in archives)}
    if {path.name for path in directory.iterdir()} != expected:
        raise ValueError("release must contain exactly the installer and four archives/checksums")
    for name in expected:
        path = directory / name
        if path.is_symlink() or not path.is_file() or path.stat().st_size == 0:
            raise ValueError(f"release asset must be a nonempty regular file: {name}")
    for target, name in zip(TARGETS, archives):
        archive = directory / name
        checksum = (directory / f"{name}.sha256").read_text().strip()
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        if checksum.split() != [digest, name]:
            raise ValueError(f"invalid checksum for {name}")
        with tarfile.open(archive, "r:gz") as stream:
            members = stream.getmembers()
            if (
                len(members) != 1
                or members[0].name != f"ma-demo-{target}"
                or not members[0].isfile()
                or members[0].size == 0
            ):
                raise ValueError(f"unexpected binary member in {name}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("version")
    args = parser.parse_args()
    verify(args.directory, args.version)
    print("Complete release asset set, checksums, and archive members verified.")
