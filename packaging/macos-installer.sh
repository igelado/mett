#!/bin/bash
# Builds the macOS installer, dist/mett-<version>-macos.dmg: a disk image
# with an installer package (Install mett.pkg), the README and the
# licenses. The package installs a universal mett (Apple silicon and
# Intel) as /usr/local/bin/mett, which is on the PATH, and its README,
# licenses and THIRD-PARTY-NOTICES in /usr/local/share/doc/mett/.
#
# Run it on a Mac after `packaging/archive.sh aarch64-apple-darwin
# x86_64-apple-darwin`, whose binaries it uses. Needs: lipo, pkgbuild and
# hdiutil (macOS and its command line tools), and Python 3. Neither the
# package nor the image is signed. See docs/RELEASING.md.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
version=$(packaging/version.sh)
dist=$root/dist
work=$root/target/packaging/macos-installer
binaries=${CARGO_TARGET_DIR:-target}
rm -rf "$work"
mkdir -p "$dist" "$work/root/usr/local/bin" "$work/root/usr/local/share/doc/mett" "$work/image"

# One binary for both architectures.
lipo -create -output "$work/root/usr/local/bin/mett" \
    "$binaries/aarch64-apple-darwin/release/mett" "$binaries/x86_64-apple-darwin/release/mett"
chmod 755 "$work/root/usr/local/bin/mett"
cp README.md LICENSE-MIT LICENSE-APACHE "$work/root/usr/local/share/doc/mett/"
# The notices of what either architecture is built from.
python3 -I packaging/notices.py aarch64-apple-darwin,x86_64-apple-darwin \
    >"$work/root/usr/local/share/doc/mett/THIRD-PARTY-NOTICES"
chmod 644 "$work/root/usr/local/share/doc/mett/"*

# Installer versions are numbers only: no pre-release part.
pkgbuild --root "$work/root" --identifier io.github.igelado.mett \
    --version "${version%%[-+]*}" --install-location / "$work/image/Install mett.pkg"
cp README.md LICENSE-MIT LICENSE-APACHE "$work/image/"
dmg=$dist/mett-$version-macos.dmg
rm -f "$dmg"
hdiutil create -volname "mett $version" -srcfolder "$work/image" -format UDZO "$dmg"
echo "Built dist/$(basename "$dmg")"
