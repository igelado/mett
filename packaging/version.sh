#!/bin/sh
# Prints mett's version, from the workspace's Cargo.toml.
set -eu
cd "$(dirname "$0")/.."
sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1
