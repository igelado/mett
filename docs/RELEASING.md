# Releasing mett

Releases are built and published by GitHub Actions
(`.github/workflows/release.yml`) when a version tag is pushed.

## What a release has

| File | For |
|------|-----|
| `mett_<version>-1_<amd64\|arm64>.deb` | Debian and Ubuntu: `sudo apt install ./mett_….deb` |
| `mett-<version>-1-<x86_64\|aarch64>.pkg.tar.zst` | Arch Linux: `sudo pacman -U mett-….pkg.tar.zst` |
| `mett-<version>-<x86_64\|aarch64>-linux.tar.gz` | Any other Linux: the binary, the README and the licenses |
| `mett-<version>-<aarch64\|x86_64>-macos.tar.gz` | macOS on Apple silicon or Intel: the same, for macOS |
| `mett-<version>-x86_64-windows.zip` | Windows on x86_64: `mett.exe`, the README and the licenses |
| `SHA256SUMS` | Checking the files: `sha256sum -c SHA256SUMS --ignore-missing` |

The macOS and Windows binaries are built on GitHub's macOS and Windows
runners by `packaging/archive.sh` (the archives by `packaging/archive.py`,
reproducibly). They link their system's C library: macOS's, and on
Windows the C runtime statically, so nothing needs installing. They
aren't signed or notarized.

The Linux packages:

The binary is linked statically against musl, so the same one runs on
every distribution release, and the packages depend on nothing. It is
built with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild),
whose zig linker builds both architectures on any runner.

The packages install `/usr/bin/mett` and the documentation in
`/usr/share/doc/mett/`; the Arch package also installs the licenses in
`/usr/share/licenses/mett/`, and the Debian package states them in its
`copyright` file. As the binary links its dependencies in, every package
also has `THIRD-PARTY-NOTICES`: the license files of each crate it is
built from (as `cargo tree` lists them; `packaging/notices.py`), and
musl's copyright.

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
