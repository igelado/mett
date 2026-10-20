#!/bin/sh
# Publishes the packages in dist/ (every file there, with SHA256SUMS) as
# the GitHub release of tag $1: creates the release (or reuses it, when the
# workflow runs again) and uploads every file, replacing one of the same
# name.
#
# Needs: gh, and in the environment GH_TOKEN, a token that may write the
# repository's releases, and GH_REPO (igelado/mett) outside a clone of it.
set -eu

[ $# -eq 1 ] || { echo "usage: $0 TAG" >&2; exit 2; }
tag=$1
cd "$(dirname "$0")/../dist"

# A version with a pre-release part (0.2.0-rc.1) is a pre-release; build
# metadata (0.2.0+build-1), after the `+`, doesn't count.
case ${tag%%+*} in
*-*) prerelease=--prerelease ;;
*) prerelease= ;;
esac

if ! gh release view "$tag" >/dev/null 2>&1; then
    # --verify-tag: the tag must have been pushed; never create one.
    gh release create "$tag" --verify-tag $prerelease \
        --title "mett ${tag#v}" \
        --notes 'Packages for Linux: Debian and Ubuntu (`.deb`), Arch Linux (`.pkg.tar.zst`) and other distributions (`-linux.tar.gz`), for x86_64 and aarch64. For macOS: an installer (`-macos.dmg`) for Apple silicon and Intel, or the binary alone (`-aarch64-macos.tar.gz`, `-x86_64-macos.tar.gz`). For Windows on x86_64: an installer (`-windows-setup.exe`), or the binary alone (`-windows.zip`). The installers are not signed yet: see the README. Check them against `SHA256SUMS`.'
fi
[ -f SHA256SUMS ] || { echo "$0: dist/SHA256SUMS is missing" >&2; exit 1; }
gh release upload "$tag" --clobber -- *
echo "Published $(gh release view "$tag" --json url --jq .url)"
