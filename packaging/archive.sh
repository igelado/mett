#!/bin/bash
# Builds mett for macOS or Windows, for each Rust target given, as an
# archive in dist/: a .tar.gz for macOS and a .zip for Windows, holding the
# binaries (mett, and mett-gui, its window), the README, the licenses and
# THIRD-PARTY-NOTICES (and, for Windows, mett's icon, for the installer's
# shortcuts). Linux packages are packaging/package.sh's.
#
# Run it on the system it builds for (a Mac builds both of its
# architectures). Needs: cargo with the targets, and Python 3. On Windows,
# run it in Git Bash; set RUSTFLAGS="-C target-feature=+crt-static" so the
# binary needs no Visual C++ runtime installed. See docs/RELEASING.md.
set -euo pipefail

usage() {
    echo "usage: $0 aarch64-apple-darwin|x86_64-apple-darwin|x86_64-pc-windows-msvc..." >&2
    exit 2
}
[ $# -gt 0 ] || usage

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
version=$(packaging/version.sh)
# Reproducible: everything is dated when the commit was made.
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}
export SOURCE_DATE_EPOCH
dist=$root/dist
work=$root/target/packaging
mkdir -p "$dist" "$work"
# (Windows has `python`, and maybe no `python3`.)
python=$(command -v python3 || command -v python) ||
    { echo "$0: python3 is missing" >&2; exit 1; }

for target in "$@"; do
    case $target in
    aarch64-apple-darwin) name=mett-$version-aarch64-macos exe= ext=tar.gz ;;
    x86_64-apple-darwin) name=mett-$version-x86_64-macos exe= ext=tar.gz ;;
    x86_64-pc-windows-msvc) name=mett-$version-x86_64-windows exe=.exe ext=zip ;;
    *) usage ;;
    esac
    echo "Building mett $version for $target"
    cargo build --release --locked -p mett-tui --target "$target"
    tree=$work/$name
    rm -rf "$tree"
    mkdir -p "$tree"
    for bin in mett mett-gui; do
        cp "${CARGO_TARGET_DIR:-target}/$target/release/$bin$exe" "$tree/"
    done
    if [ "$exe" = .exe ]; then
        cp packaging/icons/mett.ico "$tree/"
    fi
    cp README.md LICENSE-MIT LICENSE-APACHE "$tree/"
    "$python" -I packaging/notices.py "$target" >"$tree/THIRD-PARTY-NOTICES"
    "$python" -I packaging/archive.py "$dist/$name.$ext" "$tree"
    echo "Built dist/$name.$ext"
done
