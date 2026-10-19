#!/bin/sh
# Publishes the packages in dist/ as the Forgejo release of tag $1: creates
# the release (or reuses it, when the workflow runs again) and attaches every
# file, replacing one of the same name.
#
# Needs: curl and jq, and in the environment FORGEJO_URL (such as
# https://git.gelado.org), FORGEJO_REPOSITORY (igelado/mett) and
# FORGEJO_TOKEN, a token that may write the repository's releases.
set -eu

[ $# -eq 1 ] || { echo "usage: $0 TAG" >&2; exit 2; }
tag=$1
api=$FORGEJO_URL/api/v1/repos/$FORGEJO_REPOSITORY
cd "$(dirname "$0")/../dist"

call() {
    curl -sSf -H "Authorization: token $FORGEJO_TOKEN" "$@"
}

# A version with a pre-release part (0.2.0-rc.1) is a pre-release; build
# metadata (0.2.0+build-1), after the `+`, doesn't count.
case ${tag%%+*} in
*-*) prerelease=true ;;
*) prerelease=false ;;
esac

release=$(call "$api/releases/tags/$tag" 2>/dev/null || true)
if [ -z "$release" ]; then
    body=$(jq -n --arg tag "$tag" --argjson pre "$prerelease" '{
        tag_name: $tag,
        name: ("mett " + ($tag | ltrimstr("v"))),
        body: "Packages for Debian and Ubuntu (`.deb`), Arch Linux (`.pkg.tar.zst`) and other distributions (`.tar.gz`), for x86_64 and aarch64. Check them against `SHA256SUMS`.",
        prerelease: $pre
    }')
    release=$(call -X POST -H "Content-Type: application/json" -d "$body" "$api/releases")
fi
id=$(echo "$release" | jq -r .id)

for file in *.deb *.pkg.tar.zst *.tar.gz SHA256SUMS; do
    existing=$(echo "$release" | jq -r --arg name "$file" '.assets[]? | select(.name == $name) | .id')
    for asset in $existing; do
        call -X DELETE "$api/releases/$id/assets/$asset" >/dev/null
    done
    # Encoded: a `+` (build metadata) would read as a space.
    name=$(jq -rn --arg name "$file" '$name | @uri')
    call -X POST -F "attachment=@$file" "$api/releases/$id/assets?name=$name" >/dev/null
    echo "Uploaded $file"
done
echo "Published $(echo "$release" | jq -r .html_url)"
