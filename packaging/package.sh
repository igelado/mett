#!/bin/bash
# Builds mett's release packages for each architecture given (x86_64,
# aarch64): a Debian/Ubuntu package (.deb), an Arch Linux package
# (.pkg.tar.zst) and a plain tarball, all in dist/, with their SHA-256 sums.
#
# mett is linked statically against musl with cargo-zigbuild, so the same
# binary runs on every distribution release and needs no dependencies.
# mett-gui, mett in a window, loads the system's graphics libraries (X11 or
# Wayland, Vulkan or OpenGL), which a static binary can't: it is linked
# against glibc 2.28 (Debian 10, RHEL 8 and later). The packages recommend
# those libraries, and install a launcher for it (mett.desktop) and its
# icon.
#
# Needs: cargo-zigbuild (and zig), the Rust musl and gnu targets, dpkg-deb,
# bsdtar (libarchive-tools), zstd, gzip and sha256sum. See
# docs/RELEASING.md.
# pipefail: an archiver that fails fails the build, even piped into a
# compressor that doesn't.
set -euo pipefail

usage() {
    echo "usage: $0 x86_64|aarch64..." >&2
    exit 2
}
[ $# -gt 0 ] || usage

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
version=$(packaging/version.sh)
release=1
maintainer=${METT_MAINTAINER:-"Isaac Gelado <igelado@nvidia.com>"}
homepage=https://github.com/igelado/mett
summary="mutt-like mail client for Office365, in a terminal or a window"
# Reproducible: everything is dated when the commit was made.
SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}
export SOURCE_DATE_EPOCH
dist=$root/dist
work=$root/target/packaging
# Only this build's packages are in dist/, to sum and publish.
rm -rf "$dist"
mkdir -p "$dist"

for tool in cargo-zigbuild dpkg-deb bsdtar zstd gzip sha256sum python3; do
    command -v "$tool" >/dev/null || { echo "$0: $tool is missing" >&2; exit 1; }
done

# Debian orders `~` before anything; pacman orders letters right after a
# number before the number alone (vercmp: 1.0rc < 1.0 < 1.0.1). Both put a
# pre-release such as 0.2.0-rc.1 (0.2.0~rc.1, 0.2.0rc.1) before 0.2.0.
deb_version=$(echo "$version" | sed 's/-/~/')-$release
# (Arch versions can't hold a hyphen at all: 1.0.0-rc-2 is 1.0.0rc2.)
arch_version=${version//-/}

# musl's copyright, from the zig that links it in.
# (Installed with pip, zig is a Python module; with pipx, it is one of the
# Python beside cargo-zigbuild, in pipx's environment for it.)
zig_env() {
    local beside
    beside=$(dirname "$(readlink -f "$(command -v cargo-zigbuild)")")/python3
    python3 -m ziglang env 2>/dev/null ||
        { [ -x "$beside" ] && "$beside" -m ziglang env 2>/dev/null; } ||
        zig env
}
# (`zig env` prints JSON, or ZON in newer zigs: `.lib_dir = "..."`.)
musl_copyright=$(zig_env | python3 -I -c '
import re, sys
print(re.search(r"lib_dir\"?\s*[:=]\s*\"([^\"]+)\"", sys.stdin.read()).group(1))
')/libc/musl/COPYRIGHT
[ -f "$musl_copyright" ] || { echo "$0: musl's COPYRIGHT isn't in zig's lib directory" >&2; exit 1; }

# Copies the files every package installs below $1. The README links to
# the rest of the documentation online.
install_files() {
    install -Dm755 "$binary" "$1/usr/bin/mett"
    install -Dm755 "$gui" "$1/usr/bin/mett-gui"
    install -Dm644 packaging/linux/mett.desktop "$1/usr/share/applications/mett.desktop"
    install -Dm644 packaging/icons/mett-256.png "$1/usr/share/icons/hicolor/256x256/apps/mett.png"
    # The binary links its dependencies in: their notices go with it.
    install -Dm644 "$notices" "$1/usr/share/doc/mett/THIRD-PARTY-NOTICES"
    for doc in README.md docs/CONFIG-OFFICE365.md docs/MUTTRC.md; do
        install -Dm644 "$doc" "$1/usr/share/doc/mett/$(basename "$doc")"
    done
    # The guides' links into the source tree (../crates/..., ../LICENSE...)
    # point at this version's online.
    sed -i "s#](\.\./#]($homepage/blob/v$version/#g" \
        "$1/usr/share/doc/mett/CONFIG-OFFICE365.md" "$1/usr/share/doc/mett/MUTTRC.md"
}

# Dates every file below $1 to the commit.
stamp() {
    find "$1" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +
}

deb() {
    tree=$work/deb-$arch
    rm -rf "$tree"
    install_files "$tree"
    {
        echo "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"
        echo "Upstream-Name: mett"
        echo "Source: $homepage"
        echo
        echo "Files: *"
        echo "Copyright: 2026 Isaac Gelado"
        echo "License: MIT or Apache-2.0"
        echo "Comment: the software mett is built from, and linked with, is"
        echo " listed with its notices in /usr/share/doc/mett/THIRD-PARTY-NOTICES."
        echo
        echo "License: MIT"
        sed -e '1,/^Permission/{/^Permission/!d}' -e 's/^$/./' -e 's/^/ /' LICENSE-MIT
        echo
        echo "License: Apache-2.0"
        echo " On Debian systems, the full text of the Apache License, Version 2.0,"
        echo " is in /usr/share/common-licenses/Apache-2.0."
    } >"$tree/usr/share/doc/mett/copyright"
    chmod 644 "$tree/usr/share/doc/mett/copyright"
    mkdir -p "$tree/DEBIAN"
    cat >"$tree/DEBIAN/control" <<EOF
Package: mett
Version: $deb_version
Architecture: $deb_arch
Maintainer: $maintainer
Installed-Size: $(du -sk --apparent-size "$tree/usr" | cut -f1)
Depends: ca-certificates, libc6 (>= 2.28)
Recommends: libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libx11-6,
 libx11-xcb1, libxcursor1, libxi6, libxrandr2, libvulkan1 | libegl1
Section: mail
Priority: optional
Homepage: $homepage
Description: $summary
 mett brings mutt's keyboard-driven, terminal-first way of reading mail
 to Office365 accounts, through Microsoft Graph or Exchange Web Services.
 mett-gui shows the same screens in a window of its own; the recommended
 libraries are its.
EOF
    stamp "$tree"
    dpkg-deb --root-owner-group -Zxz --build "$tree" \
        "$dist/mett_${deb_version}_${deb_arch}.deb" >/dev/null
}

pacman() {
    tree=$work/arch-$arch
    rm -rf "$tree"
    install_files "$tree"
    install -Dm644 LICENSE-MIT "$tree/usr/share/licenses/mett/LICENSE-MIT"
    install -Dm644 LICENSE-APACHE "$tree/usr/share/licenses/mett/LICENSE-APACHE"
    install -Dm644 "$notices" "$tree/usr/share/licenses/mett/THIRD-PARTY-NOTICES"
    cat >"$tree/.PKGINFO" <<EOF
# Generated by packaging/package.sh
pkgname = mett
pkgbase = mett
pkgver = $arch_version-$release
pkgdesc = $summary
url = $homepage
builddate = $SOURCE_DATE_EPOCH
packager = $maintainer
size = $(du -sb --apparent-size "$tree/usr" | cut -f1)
arch = $arch
license = MIT
license = Apache-2.0
depend = ca-certificates
depend = glibc
optdepend = libxkbcommon: mett-gui, mett in a window
optdepend = libxkbcommon-x11: mett-gui on X11
optdepend = wayland: mett-gui on Wayland
optdepend = libx11: mett-gui on X11
optdepend = libxcursor: mett-gui on X11
optdepend = libxi: mett-gui on X11
optdepend = libxrandr: mett-gui on X11
optdepend = vulkan-icd-loader: mett-gui's drawing (or libglvnd, for OpenGL)
EOF
    stamp "$tree"
    # As makepkg does: an mtree of the package, then the package itself,
    # its metadata first, owned by root.
    (
        cd "$tree"
        owner="--uid 0 --gid 0 --uname root --gname root"
        files() { find .PKGINFO usr -print0 | LC_ALL=C sort -z; }
        # shellcheck disable=SC2086
        files | LANG=C bsdtar $owner -cnf - --format=mtree --null --files-from - \
            --options='!all,use-set,type,uid,gid,mode,time,size,md5,sha256,link' |
            gzip -c -f -n >.MTREE
        touch -d "@$SOURCE_DATE_EPOCH" .MTREE
        # shellcheck disable=SC2086
        { printf '.MTREE\0'; files; } | LANG=C bsdtar $owner -cnf - --null --files-from - |
            zstd -q -c -T0 -19 >"$dist/mett-$arch_version-$release-$arch.pkg.tar.zst"
    )
}

tarball() {
    tree=$work/tar-$arch/mett-$version-$arch-linux
    rm -rf "$work/tar-$arch"
    install -Dm755 "$binary" "$tree/mett"
    install -Dm755 "$gui" "$tree/mett-gui"
    install -m644 packaging/linux/mett.desktop README.md LICENSE-MIT LICENSE-APACHE "$tree/"
    # Named as mett.desktop asks for it (in ~/.local/share/icons/hicolor/256x256/apps/).
    install -m644 packaging/icons/mett-256.png "$tree/mett.png"
    install -m644 "$notices" "$tree/THIRD-PARTY-NOTICES"
    stamp "$tree"
    tar -C "$work/tar-$arch" --owner=0 --group=0 --numeric-owner --sort=name \
        -cf - "$(basename "$tree")" | gzip -n -9 >"$dist/$(basename "$tree").tar.gz"
}

for arch in "$@"; do
    case $arch in
    x86_64) deb_arch=amd64 ;;
    aarch64) deb_arch=arm64 ;;
    *) usage ;;
    esac
    target=$arch-unknown-linux-musl
    gnu=$arch-unknown-linux-gnu
    echo "Building mett $version for $arch"
    # mett, static: without the `gui` feature, which is mett-gui's.
    cargo zigbuild --release --locked -p mett-tui --no-default-features --target "$target"
    binary=${CARGO_TARGET_DIR:-target}/$target/release/mett
    cargo zigbuild --release --locked -p mett-tui --bin mett-gui --target "$gnu.2.28"
    gui=${CARGO_TARGET_DIR:-target}/$gnu/release/mett-gui
    notices=$work/THIRD-PARTY-NOTICES-$arch
    mkdir -p "$work"
    python3 -I packaging/notices.py "$target,$gnu" "$musl_copyright" >"$notices"
    deb
    pacman
    tarball
done

cd "$dist"
rm -f SHA256SUMS
sha256sum -- *.deb *.pkg.tar.zst *.tar.gz >SHA256SUMS
cat SHA256SUMS
