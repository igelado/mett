# Releasing mett

Releases are built and published by GitHub Actions
(`.github/workflows/release.yml`) when a version tag is pushed.

## What a release has

| File | For |
|------|-----|
| `mett_<version>-1_<amd64\|arm64>.deb` | Debian and Ubuntu: `sudo apt install ./mett_….deb` |
| `mett-<version>-1-<x86_64\|aarch64>.pkg.tar.zst` | Arch Linux: `sudo pacman -U mett-….pkg.tar.zst` |
| `mett-<version>-<x86_64\|aarch64>-linux.tar.gz` | Any other Linux: the binaries, `mett.desktop` and its icon, the README and the licenses |
| `mett-<version>-macos.dmg` | macOS: an installer (`Install mett.pkg`) of universal binaries, as `/usr/local/bin/mett` and `/Applications/mett.app` |
| `mett-<version>-<aarch64\|x86_64>-macos.tar.gz` | macOS on Apple silicon or Intel: the binaries, the README and the licenses |
| `mett-<version>-x86_64-windows-setup.exe` | Windows on x86_64: an installer, which adds mett to the `PATH` and mett-gui to Start's menu |
| `mett-<version>-x86_64-windows.zip` | Windows on x86_64: `mett.exe`, `mett-gui.exe`, mett's icon, the README and the licenses |

Every package has both programs: `mett`, and `mett-gui`, mett in a
window of its own, with its launcher (`mett.app`, Start's menu,
`mett.desktop`) and icon (`packaging/icons/`).
| `SHA256SUMS` | Checking the files: `sha256sum -c SHA256SUMS --ignore-missing` |

The macOS and Windows binaries are built on GitHub's macOS and Windows
runners by `packaging/archive.sh` (the archives by `packaging/archive.py`,
reproducibly). They link their system's C library: macOS's, and on
Windows the C runtime statically, so nothing needs installing. Then
`packaging/macos-installer.sh` joins both macOS builds of each binary
into one (`lipo`), wraps mett-gui in `mett.app` (its icon made with
`sips` and `iconutil`), and both in an installer package (`pkgbuild`) in
a disk image (`hdiutil`), and `packaging/windows-installer.sh` builds the
Windows installer with Inno Setup (`packaging/windows/mett.iss`).

Nothing is signed or notarized yet, so macOS and Windows warn before
running the installers (the README says how to go ahead). Signing needs
an Apple Developer ID (and notarization) and a Windows code-signing
certificate, kept as repository secrets.

The Linux packages:

`mett` is linked statically against musl, so the same binary runs on
every distribution release, and needs nothing. `mett-gui` loads the
system's graphics libraries (X11 or Wayland, Vulkan or OpenGL), which a
static binary can't: it is linked against glibc 2.28 (Debian 10, RHEL 8
and later), and the packages recommend (Debian) or suggest (Arch) those
libraries. Both are built with
[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild), whose zig
linker builds both architectures, and for that glibc, on any runner.

The packages install `/usr/bin/mett`, `/usr/bin/mett-gui`, its launcher
(`/usr/share/applications/mett.desktop`) and icon, and the documentation in
`/usr/share/doc/mett/`; the Arch package also installs the licenses in
`/usr/share/licenses/mett/`, and the Debian package states them in its
`copyright` file. As the binary links its dependencies in, every package
also has `THIRD-PARTY-NOTICES`: the license files of each crate it is
built from (as `cargo tree` lists them; `packaging/notices.py`), the
license of the font in mett-gui (Cascadia Mono, OFL), and musl's
copyright.

## Making a release

1. On a branch, set the new version in the workspace's `Cargo.toml`
   (`[workspace.package] version`), and run `cargo check` so `Cargo.lock`
   follows. Merge it into `main` with a pull request, as usual.
2. Tag the merge on `main` with the version, prefixed with `v`, and push the
   tag:

   ```sh
   git switch main && git pull --ff-only
   git tag -a v0.2.0 -m "mett 0.2.0"
   git push origin v0.2.0
   ```

3. The `release` workflow checks that the tag is the version in
   `Cargo.toml` and that it is on `main` (only what was merged is
   released), builds the packages, and publishes them as the tag's
   release on GitHub (https://github.com/igelado/mett/releases), with
   `packaging/release.sh` and `gh`. A version with a pre-release part
   (`v0.2.0-rc.1`) is published as a pre-release.

If the workflow fails after publishing some files, run it again: it reuses
the release and replaces files of the same name.

## Building the packages without releasing

Run by hand, the workflow builds the packages from a branch, to check that
they build, and publishes nothing: it keeps them as the run's
`packages-linux`, `packages-macos` and `packages-windows` artifacts.

```sh
gh workflow run release --ref igelado/my-branch
gh run watch                        # pick the run
gh run download <run-id> -D dist/   # a directory for each artifact
```

(Or from the web interface: **Actions** › **release** › **Run workflow**.)
To build the Linux packages locally instead, install the tools and run
the script:

```sh
pipx install cargo-zigbuild       # also installs zig (pip refuses outside a venv on Debian)
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
sudo apt install libarchive-tools zstd    # bsdtar, zstd; dpkg-deb comes with Debian
packaging/package.sh x86_64 aarch64
```

The packages land in `dist/` (which Git ignores). Builds are reproducible:
files are dated when the last commit was made (`SOURCE_DATE_EPOCH`).
