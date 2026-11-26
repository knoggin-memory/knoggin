# Operations

Day-to-day work on a Knoggin server: health, metrics, logs, admin commands, upgrades, restores, key rotation, and recovering a store when the state database is lost. For a local install, `knoggin status` and [Local install](local-install.md) cover the same ground.

## Health

- `GET /healthz` answers when the process is up.
- `GET /readyz` answers when it is ready to serve. The load balancer and the Compose health check call it. `knoggin healthcheck --url http://127.0.0.1:8080/readyz` does the same from a script and exits 0 or 1.
- `knoggin admin selftest` checks the state database (one read), the key provider (wraps and unwraps a test key) and the bucket (five checks of create-only uploads, signed links and listing). The router runs the bucket checks at every start and refuses to start if one fails.

The web UI's **Server status** page, for admins, shows the build, the containers, the snapshot list of any store, and the rollout.

## Metrics

`GET /metrics` serves Prometheus text on `metrics.bind` (`0.0.0.0:9091` on a server; off on a local install). It needs no credential, so never publish that port. On one host only trusted proxy addresses reach it ([Deploy on one host](deploy-one-host.md)).

| Metric | Watch for |
| --- | --- |
| `knoggin_requests_total`, `knoggin_request_seconds` | Errors and latency, by route |
| `knoggin_rate_limited_total` | Clients hitting limits, by `kind` |
| `knoggin_containers` | Containers by `state` |
| `knoggin_claims_total`, `knoggin_claim_seconds` | Claims by `outcome`. Failed claims after an upgrade point at a bad build |
| `knoggin_container_ends_total` | Containers ending, by `reason`; `memory` means a container ran out of memory |
| `knoggin_rollout_stalled` | 1 when no container of a new build becomes ready |
| `knoggin_container_snapshot_failures_total`, `knoggin_upload_budget_refused_total` | Snapshots that could not be saved |
| `knoggin_container_embed_queue_depth`, `knoggin_container_open_stores` | Load on the containers |
| `knoggin_reconcile_seconds`, `knoggin_hosts_draining` | The router's own loop |

Route labels are route patterns, never real paths, so no ID reaches a metric.

## Logs

The server logs JSON lines to standard output: `docker compose logs router` on one host, the log groups `/<name>/router`, `/<name>/user` and `/<name>/lifecycle` in CloudWatch on AWS (kept 30 days). A local install logs to `~/.knoggin/logs/daemon.log`. Set the level with `log.level` (`info` by default).

Logs never hold memory text, keys, tokens, emails or names: only IDs, route and tool names, codes, counts, sizes and durations. Keep it so in front of Knoggin: no proxy, load balancer or tunnel may log request addresses.

## Admin commands

Admin commands run where the router runs, with its config, and act on the state database, the bucket and the key directly. When a running router must act, for example to park a container, they leave a request that the router carries out within 30 seconds.

- **One host:** `docker compose exec router knoggin admin <command>`.
- **AWS:** a one-off task with the router's task definition ([Deploy on AWS](deploy-aws.md#admin-commands)).

| Command | What it does |
| --- | --- |
| `admin selftest` | Checks the database, the key and the bucket |
| `admin snapshots --store <id>` | Lists a store's snapshots, with why each is kept |
| `admin restore --store <id> --snapshot <generation>-<sequence>` | Restores a store from a snapshot. `--cancel` takes a restore back |
| `admin retire-before now` | Parks the containers started before now, a few at a time. Also an RFC 3339 time, or `clear` |
| `admin rotate-master-key` | Wraps every key again under a new master key (one host) |
| `admin internal-cert --out-dir <dir>` | Makes the routers' shared internal certificate and key |
| `admin snapshot inspect <file>`, `admin snapshot export <file> --out <file>` | Reads a downloaded snapshot (below) |

Store IDs are in the web UI's Users and Server status pages. `--json` prints any command's output as JSON.

## Upgrades and rollout

A user's container never changes build in place. It parks, and the user's next request gets a container of the new build.

- **One host:** set the new digest in `.env`, then `docker compose pull && docker compose up -d`. Everyone is parked; users see `unavailable` for about a minute.
- **AWS:** copy the new image into ECR, set `image` and run `tofu apply`. Routers roll one at a time. New claims switch to the new build as soon as one of its containers is ready. Containers of the old build park when idle, and at the latest after `containers.max_lifetime_hours` (24).

During a rollout on AWS, watch `knoggin_claims_total` with a failed `outcome`. A build that starts but fails its first claim takes every new claim until you roll back, and `knoggin_rollout_stalled` stays 0 because its containers do become ready. To roll back, apply the previous digest. To move everyone to a new build sooner, run `admin retire-before now`.

Do not skip a release whose notes say it changes the internal protocol.

## Restoring a store

1. Find the store's ID, and pick a snapshot from `admin snapshots --store <id>` or the Server status page. Names look like `0000000003-0000000007`.
2. `admin restore --store <id> --snapshot <name>`. The user's container is asked to park; the command waits up to 150 seconds. If it times out, the restore happens at the container's next park.
3. At the next open the store loads that snapshot and saves it again as the newest. Until then, writes to it answer `unavailable`.

Nothing is deleted, so you can undo a restore by restoring the snapshot that was newest before. If the chosen snapshot cannot be read, the store is locked; run `admin restore --store <id> --cancel` and pick another.

## Rotating keys

- **Master key, one host:** with the router stopped; [Deploy on one host](deploy-one-host.md#rotating-the-master-key) has the three commands.
- **Master key, AWS:** KMS rotates its own key material every year. `admin rotate-master-key` says so and changes nothing.
- **Master key, local install:** `knoggin key rotate`, with the daemon stopped.
- **API keys:** users rotate their own in the web UI ([Apps and API keys](apps-and-keys.md)).

Rotation does not help against a key that has already leaked: the store keys stay the same. Export each affected store, import it into a new store, and delete the old one.

## Recovering a store from the bucket

If the state database is lost and cannot be restored, every store can still be read from its snapshots with the master key alone.

1. **Find the newest snapshot** of the store: the object with the highest name under `stores/<store id>/snap/` in the bucket.
2. **Download it** with the bucket's own tools, for example `aws s3 cp s3://<bucket>/stores/<store id>/snap/<name>.kgs .`
3. **Look at it:** `knoggin admin snapshot inspect <name>.kgs` prints its header and needs no key.
4. **Export it:** `knoggin admin snapshot export <name>.kgs --out store.jsonl`. This unwraps the snapshot's key with the configured key provider, so it needs the master key:
   - **One host:** run it in a one-off container of the router, with the download in a mounted folder:

     ```sh
     docker compose run --rm --user "$(id -u):$(id -g)" -v ./recover:/recover \
       router admin snapshot export /recover/<name>.kgs --out /recover/store.jsonl
     ```

   - **AWS:** only the router's task role may use the KMS key. Run the export where that role applies, or have a key administrator add your role to the key policy for the time of the recovery and take it out afterwards.
5. **Import it** into a store of a working deployment, with the SDK's `import_`, or `knoggin import` on a local install. The file is plain text: delete it when you are done.

If the newest snapshot cannot be read, try the one before it.

## See also

- [Deploy on one host](deploy-one-host.md), [Deploy on AWS](deploy-aws.md)
- [Security](security.md)
