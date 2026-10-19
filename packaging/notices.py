#!/usr/bin/env python3
"""Writes the notices of what the mett binary for TARGET links in.

The binary is linked statically, so the copyright and license notices of
the Rust crates it is built from (as `cargo tree` lists them for TARGET),
and of musl, go with it. Each crate's own license files are copied; one
without any is listed with its license expression. Bundled C code (SQLite
in libsqlite3-sys, AWS-LC in aws-lc-sys) comes with its crate's files.

Usage: notices.py TARGET MUSL_COPYRIGHT_FILE > THIRD-PARTY-NOTICES
"""

import json
import subprocess
import sys
from pathlib import Path

LICENSE_FILES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT", "UNLICENSE")


def main() -> None:
    target, musl = sys.argv[1], Path(sys.argv[2])
    tree = subprocess.run(
        ["cargo", "tree", "--locked", "-p", "mett-tui", "-e", "normal",
         "--target", target, "--prefix", "none", "-f", "{p}"],
        check=True, capture_output=True, text=True,
    ).stdout
    linked = set()
    for line in tree.splitlines():
        name, version = line.split()[:2]
        linked.add((name, version.lstrip("v")))
    metadata = json.loads(subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        check=True, capture_output=True, text=True,
    ).stdout)
    workspace = set(metadata["workspace_members"])
    out = sys.stdout
    out.write(
        "Notices of the software the mett binary is built from, as their\n"
        "licenses ask. mett's own license is in LICENSE-MIT and LICENSE-APACHE.\n"
    )
    packages = sorted(
        (p for p in metadata["packages"]
         if (p["name"], p["version"]) in linked and p["id"] not in workspace),
        key=lambda p: (p["name"], p["version"]),
    )
    for package in packages:
        root = Path(package["manifest_path"]).parent
        files = sorted(
            f for f in root.iterdir()
            if f.is_file() and f.name.upper().startswith(LICENSE_FILES)
        )
        out.write(f"\n{'=' * 72}\n{package['name']} {package['version']}"
                  f" ({package.get('license') or 'see below'})\n{'=' * 72}\n")
        if not files:
            out.write(f"\nLicensed under {package.get('license')}.\n")
        for file in files:
            out.write(f"\n--- {file.name} ---\n\n")
            out.write(file.read_text(errors="replace"))
    out.write(f"\n{'=' * 72}\nmusl libc (statically linked)\n{'=' * 72}\n\n")
    out.write(musl.read_text(errors="replace"))


if __name__ == "__main__":
    main()
