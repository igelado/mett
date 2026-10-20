#!/usr/bin/env python3
"""Writes the notices of what the mett and mett-gui binaries for TARGET
are built from.

TARGET may be several, comma-separated, for a binary built for each (a
universal macOS binary): the notices are those of every one.

The binary is linked statically, so the copyright and license notices of
the Rust crates it is built from (as `cargo tree` lists them for TARGET),
and of musl on Linux, go with it. Each crate's own license files are copied; one
without any is listed with its license expression. Bundled C code (SQLite
in libsqlite3-sys, AWS-LC in aws-lc-sys) comes with its crate's files.
mett-gui also has a font in it, Cascadia Mono, under the SIL Open Font
License; and both an English dictionary, SCOWL's en_US (MIT and BSD).

Usage: notices.py TARGET [MUSL_COPYRIGHT_FILE] > THIRD-PARTY-NOTICES

(macOS and Windows binaries link their system's C library, which isn't
redistributed with them: they have no MUSL_COPYRIGHT_FILE.)
"""

import json
import subprocess
import sys
from pathlib import Path

LICENSE_FILES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT", "UNLICENSE")


def main() -> None:
    # UTF-8, whatever the system's encoding (Windows' isn't).
    sys.stdout.reconfigure(encoding="utf-8")
    targets = sys.argv[1].split(",")
    musl = Path(sys.argv[2]) if len(sys.argv) > 2 else None
    linked = set()
    for target in targets:
        tree = subprocess.run(
            # With the default `gui` feature: mett-gui's crates too.
            ["cargo", "tree", "--locked", "-p", "mett-tui", "-e", "normal",
             "--target", target, "--prefix", "none", "-f", "{p}"],
            check=True, capture_output=True, text=True, encoding="utf-8",
        ).stdout
        for line in tree.splitlines():
            name, version = line.split()[:2]
            linked.add((name, version.lstrip("v")))
    metadata = json.loads(subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version", "1"],
        check=True, capture_output=True, text=True, encoding="utf-8",
    ).stdout)
    workspace = set(metadata["workspace_members"])
    out = sys.stdout
    out.write(
        "Notices of the software the mett and mett-gui binaries are built\n"
        "from, as their licenses ask.\n"
        "mett's own license is in LICENSE-MIT and LICENSE-APACHE.\n"
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
            out.write(file.read_text(encoding="utf-8", errors="replace"))
    font = Path(__file__).parent.parent / "crates/mett-tui/assets/fonts/CascadiaOFL.txt"
    out.write(f"\n{'=' * 72}\nCascadia Mono (font, in mett-gui) (OFL-1.1)\n{'=' * 72}\n\n")
    out.write(font.read_text(encoding="utf-8"))
    words = Path(__file__).parent.parent / "crates/mett-tui/assets/dictionaries/en_US-LICENSE"
    out.write(f"\n{'=' * 72}\nen_US Hunspell dictionary, from SCOWL (MIT AND BSD)\n{'=' * 72}\n\n")
    out.write(words.read_text(encoding="utf-8"))
    if musl:
        out.write(f"\n{'=' * 72}\nmusl libc (statically linked)\n{'=' * 72}\n\n")
        out.write(musl.read_text(encoding="utf-8", errors="replace"))


if __name__ == "__main__":
    main()
