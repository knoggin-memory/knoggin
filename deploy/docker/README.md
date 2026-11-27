# Knoggin on one host

Runs the Knoggin server on one machine with Docker Compose: a TLS proxy (Caddy), the
router, a launcher that starts one plain container per user, Postgres, a nightly dump, a
lifecycle task and, for trials, a SeaweedFS bucket.

**Who this is for.** Trying the server, or a small group who trust each other and the
host.

## Needs

- Docker Engine 28 or newer with cgroup v2, and OpenSSL.
- A DNS name for the host, ports 80 and 443 open, and an OpenID Connect provider with a
  web client whose redirect address is `https://<your name>/auth/callback`.
- Set the host's core pattern so no core dump lands on disk (a core limit of 0 does not
  hold where core dumps are piped to a program):
  `sudo sysctl -w kernel.core_pattern='|/bin/false'`, and the same in `/etc/sysctl.d/`.

## Start

1. `cp .env.example .env` and set `KNOGGIN_IMAGE` (the release's image, by digest) and
   `KNOGGIN_DOMAIN`.
2. `cp config.example.toml config.toml` and fill in `[auth]` and `containers.max`.
3. `./make-secrets.sh` once. It writes `./secrets` (mode 0700, files 0444) and asks for
   the web client secret. **Keep a copy of `secrets/master_key` and `secrets/state_key`
   away from this host**: without either, the data cannot be read.
4. `docker compose up -d`.

Admin commands run in the router: `docker compose exec router knoggin admin selftest`.
Upgrade with `docker compose pull && docker compose up -d`: everyone is parked, and users
see `unavailable` for about a minute.

## What is on the disk

- Knoggin seals every record in Postgres, so the database volume and the dumps in
  `./backups` (one per weekday, written at 03:00 UTC) show only record keys (IDs and
  hashes), version numbers and expiry times. Keep `./backups` private all the same.
- Snapshots in the bucket are encrypted. Leave versioning and bucket-side backups off,
  or accept that they keep an erased store readable for as long as they keep its objects.
- Containers' launch secrets are readable by anyone who can use the Docker socket.

## The bundled bucket

The `local-s3` profile runs SeaweedFS for trials. Its S3 interface needs the keys from
`./secrets`, but its other interfaces (master, volume, filer) have no sign-in, so any
service on the `store` network could change objects past the S3 identities. Its write
permission includes delete, so the router's key can delete snapshots here. For real use,
point `[storage]` at your own S3-compatible bucket, give the router a key that cannot
delete and the lifecycle task (`s3_credentials_lifecycle`) its own, and drop `local-s3`
from `COMPOSE_PROFILES`.

## Networks and metrics

The five networks take their ranges from `KNOGGIN_NET_PREFIX` (default `10.200`); a
second stack on the same host needs another prefix. Only the proxy, at `<prefix>.0.2`,
is a trusted proxy. Nothing else can reach the router's metrics (port 9091). To scrape
them, give the scraper a fixed address in the lower half of `edge` and add it to
`KNOGGIN_SERVER__TRUSTED_PROXIES` in `compose.yaml`; that address is then trusted like
the proxy, also for the client addresses it names on the public port.

## Logs

Nothing in front of Knoggin may log request addresses: a request line can carry a search
text, a tag or a one-time code. The `Caddyfile` logs none. A proxy or tunnel of your own
must not either.
