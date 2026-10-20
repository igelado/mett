#!/bin/bash
# Builds the Windows installer, dist/mett-<version>-x86_64-windows-setup.exe,
# with Inno Setup (packaging/windows/mett.iss): it installs mett.exe,
# mett-gui.exe (mett in a window, which Start's menu has a shortcut to), the
# README, the licenses and THIRD-PARTY-NOTICES, adds mett to the PATH, and
# can be uninstalled from Windows' settings.
#
# Run it in Git Bash after `packaging/archive.sh x86_64-pc-windows-msvc`,
# whose files it installs. Needs: Inno Setup 6 (ISCC.exe on the PATH or in
# its usual place). The installer isn't signed. See docs/RELEASING.md.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
version=$(packaging/version.sh)
tree=$root/target/packaging/mett-$version-x86_64-windows
[ -f "$tree/mett.exe" ] && [ -f "$tree/mett-gui.exe" ] || { echo "$0: run packaging/archive.sh x86_64-pc-windows-msvc first" >&2; exit 1; }
iscc=$(command -v iscc || command -v ISCC ||
    { f="/c/Program Files (x86)/Inno Setup 6/ISCC.exe"; [ -f "$f" ] && echo "$f"; }) ||
    { echo "$0: Inno Setup's ISCC.exe is missing" >&2; exit 1; }
mkdir -p dist
# Windows' file version is numbers only: no pre-release part.
numeric=${version%%[-+]*}
# Git Bash would take ISCC's /options for paths and convert them.
MSYS2_ARG_CONV_EXCL='*' "$iscc" /Q \
    "/DVersion=$version" "/DNumericVersion=$numeric" \
    "/DSource=$(cygpath -w "$tree")" \
    "/O$(cygpath -w "$root/dist")" "/Fmett-$version-x86_64-windows-setup" \
    "$(cygpath -w packaging/windows/mett.iss)"
echo "Built dist/mett-$version-x86_64-windows-setup.exe"
