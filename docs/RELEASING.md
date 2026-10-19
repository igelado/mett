# Releasing mett

Releases are built and published by Forgejo Actions
(`.forgejo/workflows/release.yml`) when a version tag is pushed.

## What a release has

For x86_64 and aarch64:

| File | For |
|------|-----|
| `mett_<version>-1_<amd64\|arm64>.deb` | Debian and Ubuntu: `sudo apt install ./mett_….deb` |
| `mett-<version>-1-<x86_64\|aarch64>.pkg.tar.zst` | Arch Linux: `sudo pacman -U mett-….pkg.tar.zst` |
| `mett-<version>-<arch>-linux.tar.gz` | Any other Linux: the binary, the README and the licenses |
| `SHA256SUMS` | Checking the files: `sha256sum -c SHA256SUMS --ignore-missing` |

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
   release on Forgejo (https://git.gelado.org/igelado/mett/releases). A
   version with a pre-release part (`v0.2.0-rc.1`) is published as a
   pre-release.

If the workflow fails after publishing some files, run it again: it reuses
the release and replaces files of the same name.

## Building the packages without releasing

From the Forgejo web interface (**Actions** › **release** › **Run
workflow**), the workflow builds the packages from a branch, to check that
they build, and publishes nothing. (It doesn't keep them as an artifact:
git.gelado.org's artifact storage fails uploads.) To get the packages
without releasing, build them locally: install the tools and run the
script:

```sh
pipx install cargo-zigbuild       # also installs zig (pip refuses outside a venv on Debian)
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
sudo apt install libarchive-tools zstd    # bsdtar, zstd; dpkg-deb comes with Debian
packaging/package.sh x86_64 aarch64
```

The packages land in `dist/` (which Git ignores). Builds are reproducible:
files are dated when the last commit was made (`SOURCE_DATE_EPOCH`).
