#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Puts one image manifest, such as the index that `docker buildx imagetools create
# --dry-run` prints, into the registry under each <ref>: a tag, or the manifest's own
# digest. It checks that the registry stored exactly these bytes and prints the digest.
# release.yml uses it so that the image can be scanned, attested and signed by digest
# before it has any tag, and so that every tag points at exactly the signed bytes.
# (`imagetools create` needs a tag to push, and may write a new index when copying.)
#
# --keep: a <ref> that exists already is left alone if it is this manifest, and is an
# error if it is another one. That is how a tag is pushed once and never moved.
#
# Needs curl, jq and sha256sum, and GITHUB_ACTOR and GH_TOKEN for the registry.
#
# Usage: scripts/put-manifest.sh [--keep] <registry/owner/name> <manifest.json> <ref>...
set -eu

keep=
if [ "${1:-}" = --keep ]; then
    keep=1
    shift
fi
if [ $# -lt 3 ]; then
    echo "usage: $0 [--keep] <registry/owner/name> <manifest.json> <ref>..." >&2
    exit 2
fi
image=$1
file=$2
shift 2
registry=${image%%/*}
repo=${image#*/}
digest="sha256:$(sha256sum "$file" | cut -d' ' -f1)"
media_type=$(jq -er .mediaType "$file")
accept="$media_type, application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json"
headers=$(mktemp)
trap 'rm -f "$headers"' EXIT
trap 'exit 130' INT TERM

# Credentials go to curl on stdin, never on its command line.
token=$(printf 'user = "%s:%s"\n' "$GITHUB_ACTOR" "$GH_TOKEN" \
    | curl --config - -fsS "https://$registry/token?service=$registry&scope=repository:$repo:pull,push" \
    | jq -er .token)

# Sends one request with the token; the response headers land in $headers.
registry_call() {
    printf 'header = "Authorization: Bearer %s"\n' "$token" \
        | curl --config - -sS -D "$headers" -o /dev/null -w '%{http_code}' "$@"
}
stored_digest() {
    tr -d '\r' < "$headers" | awk -F': ' 'tolower($1) == "docker-content-digest" { print $2 }'
}

for ref in "$@"; do
    url="https://$registry/v2/$repo/manifests/$ref"
    if [ -n "$keep" ]; then
        status=$(registry_call --head -H "Accept: $accept" "$url")
        case "$status" in
            200)
                current=$(stored_digest)
                if [ "$current" != "$digest" ]; then
                    echo "$image $ref exists already, as ${current:-an unknown digest}, not $digest" >&2
                    exit 1
                fi
                echo "$image $ref is this manifest already" >&2
                continue
                ;;
            404) ;;
            *)
                echo "$image $ref: the registry answered $status to a check" >&2
                exit 1
                ;;
        esac
    fi
    status=$(registry_call -X PUT -H "Content-Type: $media_type" --data-binary "@$file" "$url")
    stored=$(stored_digest)
    if [ "$status" != 201 ] || [ "$stored" != "$digest" ]; then
        echo "$image $ref: the registry answered $status and stored ${stored:-nothing}, not $digest" >&2
        exit 1
    fi
done
echo "$digest"
