#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
#
# Makes the secrets folder of the one-host stack and writes every value once (tab
# "Deployment", Secrets). Needs only Docker and OpenSSL, and no root. Run it once, from
# this folder, after setting KNOGGIN_IMAGE in .env. A file that already exists is kept,
# so running it again only fills in what is missing.
#
# The web client secret comes from your identity provider: set KNOGGIN_WEB_CLIENT_SECRET,
# or type it when asked.
set -eu
cd "$(dirname "$0")"

dir="${KNOGGIN_SECRETS_DIR:-$(sed -n 's/^KNOGGIN_SECRETS_DIR=//p' .env 2>/dev/null)}"
dir="${dir:-./secrets}"
image="${KNOGGIN_IMAGE:-$(sed -n 's/^KNOGGIN_IMAGE=//p' .env 2>/dev/null)}"
if [ -z "$image" ]; then
    echo "set KNOGGIN_IMAGE in .env first" >&2
    exit 1
fi

umask 077
mkdir -p "$dir"
chmod 0700 "$dir"
dir=$(cd "$dir" && pwd)

# write <name>: the value on standard input, unless the file exists.
write() {
    if [ -e "$dir/$1" ]; then
        cat > /dev/null
        echo "kept    $1"
    else
        cat > "$dir/$1.part"
        mv "$dir/$1.part" "$dir/$1"
        echo "wrote   $1"
    fi
}

# Knoggin commands, run through the image as the deploying user.
knoggin() {
    docker run --rm --user "$(id -u):$(id -g)" -v "$dir:/out" "$image" "$@" > /dev/null
}

hex() { openssl rand -hex "$1"; }

superuser=$(hex 32)
printf '%s\n' "$superuser" | write postgres_superuser_password
printf 'postgres://knoggin:%s@db:5432/knoggin?sslmode=disable\n' "$(hex 32)" | write postgres_url
printf 'postgres://knoggin_lifecycle:%s@db:5432/knoggin?sslmode=disable\n' "$(hex 32)" | write postgres_url_lifecycle
printf 'postgres://knoggin_backup:%s@db:5432/knoggin?sslmode=disable\n' "$(hex 32)" | write postgres_url_backup

for who in s3_credentials s3_credentials_lifecycle; do
    printf '[default]\naws_access_key_id = %s\naws_secret_access_key = %s\n' "$(hex 10)" "$(hex 20)" | write "$who"
done

openssl rand -base64 32 | write internal_secret
hex 32 | write launcher_token

for key in master_key state_key; do
    if [ -e "$dir/$key" ]; then echo "kept    $key"; else knoggin key generate --out "/out/$key"; echo "wrote   $key"; fi
done
if [ -e "$dir/internal_cert.pem" ] && [ -e "$dir/internal_key.pem" ]; then
    echo "kept    internal_cert.pem, internal_key.pem"
else
    rm -f "$dir/internal_cert.pem" "$dir/internal_key.pem"
    knoggin admin internal-cert --out-dir /out
    echo "wrote   internal_cert.pem, internal_key.pem"
fi

if [ ! -e "$dir/web_client_secret" ]; then
    secret="${KNOGGIN_WEB_CLIENT_SECRET:-}"
    if [ -z "$secret" ]; then
        printf 'Web client secret from your identity provider: ' >&2
        stty -echo 2>/dev/null || true
        read -r secret
        stty echo 2>/dev/null || true
        echo >&2
    fi
    printf '%s\n' "$secret" | write web_client_secret
fi

# The folder keeps other accounts out; the files must be readable by every service's
# user, so they are 0444 (tab "Deployment", Secrets).
chmod 0444 "$dir"/*
echo "done: $dir"
