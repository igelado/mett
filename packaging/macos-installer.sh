#!/bin/bash
# Builds the macOS installer, dist/mett-<version>-macos.dmg: a disk image
# with an installer package (Install mett.pkg), the README and the
# licenses. The package installs, universal (Apple silicon and Intel):
# mett as /usr/local/bin/mett, which is on the PATH; mett-gui, mett in a
# window, as /Applications/mett.app; and the README, licenses and
# THIRD-PARTY-NOTICES in /usr/local/share/doc/mett/.
#
# Run it on a Mac after `packaging/archive.sh aarch64-apple-darwin
# x86_64-apple-darwin`, whose binaries it uses. Needs: lipo, sips,
# iconutil, plutil, pkgbuild and hdiutil (macOS and its command line
# tools), and Python 3. Neither the package nor the image is signed. See
# docs/RELEASING.md.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
version=$(packaging/version.sh)
dist=$root/dist
work=$root/target/packaging/macos-installer
binaries=${CARGO_TARGET_DIR:-target}
rm -rf "$work"
app=$work/root/Applications/mett.app/Contents
mkdir -p "$dist" "$work/root/usr/local/bin" "$work/root/usr/local/share/doc/mett" "$work/image" \
    "$app/MacOS" "$app/Resources"

# One binary for both architectures, of each program.
universal() {
    lipo -create -output "$2" \
        "$binaries/aarch64-apple-darwin/release/$1" "$binaries/x86_64-apple-darwin/release/$1"
    chmod 755 "$2"
}
universal mett "$work/root/usr/local/bin/mett"
universal mett-gui "$app/MacOS/mett-gui"

# mett.app: mett-gui, its icon (every size macOS asks for, from one), and
# what Finder needs to know of it.
iconset=$work/mett.iconset
mkdir -p "$iconset"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" packaging/icons/mett.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
    double=$((size * 2))
    sips -z "$double" "$double" packaging/icons/mett.png \
        --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Resources/mett.icns"
cat >"$app/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>mett</string>
    <key>CFBundleDisplayName</key>
    <string>mett</string>
    <key>CFBundleIdentifier</key>
    <string>io.github.igelado.mett.gui</string>
    <key>CFBundleExecutable</key>
    <string>mett-gui</string>
    <key>CFBundleIconFile</key>
    <string>mett</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${version%%[-+]*}</string>
    <key>CFBundleVersion</key>
    <string>${version%%[-+]*}</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.productivity</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF
chmod 644 "$app/Info.plist" "$app/Resources/mett.icns"
cp README.md LICENSE-MIT LICENSE-APACHE "$work/root/usr/local/share/doc/mett/"
# The notices of what either architecture is built from.
python3 -I packaging/notices.py aarch64-apple-darwin,x86_64-apple-darwin \
    >"$work/root/usr/local/share/doc/mett/THIRD-PARTY-NOTICES"
chmod 644 "$work/root/usr/local/share/doc/mett/"*

# mett.app goes in /Applications, even if one was moved elsewhere
# (pkgbuild would update that one).
pkgbuild --analyze --root "$work/root" "$work/components.plist"
plutil -replace 0.BundleIsRelocatable -bool NO "$work/components.plist"
# Installer versions are numbers only: no pre-release part.
pkgbuild --root "$work/root" --component-plist "$work/components.plist" \
    --identifier io.github.igelado.mett \
    --version "${version%%[-+]*}" --install-location / "$work/image/Install mett.pkg"
cp README.md LICENSE-MIT LICENSE-APACHE "$work/image/"
dmg=$dist/mett-$version-macos.dmg
rm -f "$dmg"
hdiutil create -volname "mett $version" -srcfolder "$work/image" -format UDZO "$dmg"
echo "Built dist/$(basename "$dmg")"
