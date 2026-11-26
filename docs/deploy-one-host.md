# Deploy on one host

The one-host stack runs the Knoggin server on a single machine with Docker Compose. It is for trying the server, and for a small group who trust each other and the host. The files are in `deploy/docker/`; its `README.md` is the short version of this guide.

**Know the limit first.** On one host the router, the launcher and every user's container share one kernel. A container escape reaches everything, and whoever runs the host can read every store. For people who must not see each other's data, use [AWS](deploy-aws.md).

## What runs

| Service | Job |
| --- | --- |
| `proxy` | Caddy: TLS on ports 80 and 443. The only service with published ports |
| `router` | Checks every request's credential and forwards it to the user's container. Holds the keys |
| `launcher` | The only service with the Docker socket. Starts one plain container per active user, on a network of its own |
| `lifecycle` | Every 10 minutes: thins old snapshots and erases deleted data |
| `db` | Postgres, the state database. Every record in it is sealed |
| `backup` | A `pg_dump` at 03:00 UTC every night, to `./backups`, one file per weekday |
| `s3`, `s3-init` | A SeaweedFS bucket, for trials only (profile `local-s3`) |

A user's container loads that user's stores from the bucket, serves them, and parks after 10 idle minutes. It has no disk, no credentials and no way out but the router.

## Before you start

- Docker Engine 28 or newer with cgroup v2, and OpenSSL.
- A DNS name for the host, with ports 80 and 443 open.
- An OpenID Connect identity provider, with a web application whose redirect address is `https://<your name>/auth/callback`. See [Sign-in](sign-in.md).
- Set the host's core pattern so that no core dump lands on disk:

  ```sh
  sudo sysctl -w kernel.core_pattern='|/bin/false'
  ```

  Put the same line in a file under `/etc/sysctl.d/` so that it survives a reboot.

## Start

From `deploy/docker/`:

1. `cp .env.example .env`. Set `KNOGGIN_IMAGE` to the release image by digest (the release notes give it) and `KNOGGIN_DOMAIN` to your host name.
2. `cp config.example.toml config.toml`. Fill in `[auth]`: the issuer, the web client ID, and who is an admin (`admin_subjects` is safer than `admin_emails`). Set who may sign in with `allowed_emails` or `allowed_email_domains`; with both empty, everyone your provider admits gets an account. Set `containers.max`: the host's memory less 2 GiB, divided by `containers.memory_mib` (1,024 by default). 6 fits a host with 8 GiB.
3. `./make-secrets.sh`, once. It writes every secret into `./secrets` (folder mode 0700, files 0444) and asks for the web client secret. **Copy `secrets/master_key` and `secrets/state_key` somewhere away from this host.** Without either, the data cannot be read.
4. `docker compose up -d`.
5. Check: `docker compose exec router knoggin admin selftest` tests the database, the key and the bucket.

Then open `https://<your name>` and sign in. Agents connect to `https://<your name>/mcp` ([Connecting agents](connect-agents.md)).

## Secrets

Every secret is a file in `./secrets`, mounted read-only into only the services that need it. Nothing secret is in `config.toml` or `.env`. The folder's mode keeps other accounts on the host out.

| File | Used by | What it is |
| --- | --- | --- |
| `master_key` | router | Wraps every store's key |
| `state_key` | router, lifecycle | Seals every record in Postgres |
| `postgres_url`, `postgres_url_lifecycle`, `postgres_url_backup`, `postgres_superuser_password` | router, lifecycle, backup, db | Database logins. The lifecycle login can only read and delete; the backup login can only read |
| `s3_credentials`, `s3_credentials_lifecycle` | router, lifecycle | Bucket keys |
| `internal_secret`, `internal_cert.pem`, `internal_key.pem` | router | The link between the router and the containers |
| `launcher_token` | router, launcher | The launcher's shared token |
| `web_client_secret` | router | The identity provider's web client secret |

## Your own bucket

The bundled SeaweedFS is for trials: its internal interfaces have no sign-in, and the router's key there can delete. For real use:

1. Point `[storage]` in `config.toml` at your S3-compatible bucket (`bucket`, `region`, `endpoint_url`, `force_path_style`).
2. Give the router a key that can read, list and create objects but not delete them, in `secrets/s3_credentials`. Give the lifecycle task a key that can also delete, in `secrets/s3_credentials_lifecycle`. Without scoped keys the router can delete too.
3. Leave the bucket's versioning and its own backups off, or accept that they keep an erased store readable for as long as they keep its objects.
4. Drop `local-s3` from `COMPOSE_PROFILES` in `.env`.
5. Run `docker compose exec router knoggin admin selftest`. The router also runs it at every start and refuses a bucket that fails.

## Upgrade

```sh
docker compose pull && docker compose up -d
```

Set the new image digest in `.env` first. On stop the router parks every user, so users see `unavailable` for about a minute; the next request starts a container of the new build. Do not skip a release whose notes say it changes the internal protocol.

## Backups

- **Snapshots** in the bucket are encrypted. The lifecycle task keeps the newest one, then one per 10 minutes for an hour, one per hour for a day, and one per day for `retention.days` (7) days.
- **The database** is dumped every night to `./backups/knoggin-<weekday>.sql`. A dump shows only record keys (IDs and hashes), version numbers and expiry times; every document in it is sealed. Keep the folder private all the same, and copy it off the host.
- **The keys:** keep copies of `secrets/master_key` and `secrets/state_key` away from the host. A dump without the state key, or a snapshot without the master key, is unreadable.

**Restoring the database** from a dump, with the router and the lifecycle task stopped:

```sh
docker compose stop router lifecycle
docker compose rm -sf db
docker volume rm <project>_db-data          # the project is the folder name unless you set one
docker compose up -d db
docker compose exec -T db psql -v ON_ERROR_STOP=1 -U postgres -d knoggin < backups/knoggin-<weekday>.sql
docker compose up -d
```

A restored database brings back what was true when the dump was made: API keys revoked since then work again, ended sessions come back, and users removed since then return. Revoke those keys and remove those users again. To restore one store from an older snapshot, see [Operations](operations.md).

## Rotating the master key

It is the one admin command that needs the router stopped:

```sh
docker compose stop router
docker compose run --rm --user "$(id -u):$(id -g)" -v ./secrets:/rotate \
  -e KNOGGIN_KEYS__MASTER_KEY_FILE=/rotate/master_key router admin rotate-master-key
docker compose start router
```

It writes the new key set to `secrets/master_key` and wraps every store's key again. Copy the new file away from the host. Rotation does not help against a key that has already leaked; see [Security](security.md).

## Networks, metrics and logs

- The five networks take their ranges from `KNOGGIN_NET_PREFIX` (default `10.200`). A second stack on the same host needs another prefix.
- Only the proxy is a trusted proxy. To scrape the router's metrics (port 9091), give the scraper a fixed address in the lower half of the `edge` network and add it to `KNOGGIN_SERVER__TRUSTED_PROXIES` in `compose.yaml`. That address is then trusted like the proxy.
- **Nothing in front of Knoggin may log request addresses.** A request line can carry a search text, a tag or a one-time code. The bundled Caddy logs none. A proxy or tunnel of your own must not either.
- `docker compose logs router` shows the router's log. Logs never hold memory text, keys or tokens.

## The source offer

Knoggin is licensed under the AGPL, which gives the users of a network service the right to its source. Every deployment links to it: the setting `source_url` (default `https://github.com/knoggin-memory/knoggin`) appears in the web UI, in the session route and at the end of the MCP server instructions. **If you deploy a changed version, set `source_url` to where your changed source is**, in `config.toml`.

## Limits of one host

- The router, the launcher and the containers share one kernel. A container escape reaches everything.
- The router holds the bucket key. Without a scoped key it can delete snapshots.
- Anyone who can use the Docker socket can read each container's launch secret.
- The database's lifecycle login can delete any record. The nightly dump is the way back.
- One host has a fixed amount of memory. `containers.max` bounds how many users are served at once.

More in [Security](security.md). Day-to-day work is in [Operations](operations.md).
