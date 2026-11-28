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
| `knoggin_claims_total`, `knoggin_claim_seconds` | Claims by `outcome`. Failed claims after an upgrade point at a bad build. `user_cap` and `user_churn` are claims refused by `containers.max_per_user` (below) |
| `knoggin_container_ends_total` | Containers ending, by `reason`; `memory` means a container ran out of memory |
| `knoggin_rollout_stalled` | 1 when no container of a new build becomes ready |
| `knoggin_container_snapshot_failures_total`, `knoggin_upload_budget_refused_total` | Snapshots that could not be saved |
| `knoggin_container_embed_queue_depth`, `knoggin_container_open_stores` | Load on the containers |
| `knoggin_reconcile_seconds`, `knoggin_hosts_draining` | The router's own loop |

Route labels are route patterns, never real paths, so no ID reaches a metric.

## Containers per user

A user's own container serves their personal store and every app of theirs. An isolated or shared app gets a container of its own, which can reach only that app's key, and counts toward its owner's limit.

`containers.max_per_user` (6) is the most containers one user may have at once, their own included, so at most 5 isolated or shared apps of one owner run at a time. One user can therefore hold 6 times `containers.memory_mib`; size the host or `max_hosts` for that. `0` turns the limit off; the smallest limit is 2 (a user's own container and one app), so 1 is refused at start.

When an owner's apps would pass it, a request to one more app is refused with 429 `rate_limited`, reason `containers_per_user`, and `Retry-After: 30`. The personal store and normal apps are never refused by it. An app container is counted again each time it is claimed: more than `max(6, 3 × (max_per_user − 1))` claims for one owner's apps in 10 minutes are refused the same way with `Retry-After: 60`. The limit is checked by each router from the container records, so two routers that claim at the same moment can each pass it by one.

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

## Sizing

Count containers, not users: one for each active user, one more for each active isolated app, and one more for each active shared app. Size for up to 6 per heavy user. A container parks after 10 idle minutes.

| | AWS on EC2 hosts (`user_compute = "ec2"`, default) | AWS on Fargate (`user_compute = "fargate"`) |
| --- | --- | --- |
| Memory per container | 1,536 MiB | 2,048 MiB recommended |
| CPU per container | A share of the host; bursts to the host | 0.5 vCPU by default, a hard cap |
| `containers.max` | `max_hosts` times the containers per host | `max_containers` |
| Memory for a container's open stores, `(memory - 928 MiB) × 0.4` | 243 MiB | 448 MiB at 2,048 MiB |

[Deploy on AWS](deploy-aws.md#hosts-or-fargate) explains the choice and the host sizes.

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

## Bring a deleted snapshot back from bucket versions

Only on AWS with `undelete_days` above 0 ([Deploy on AWS](deploy-aws.md#restore)), and only for about that many days after the delete. Use operator credentials: the bucket policy keeps both Knoggin roles away from old versions.

1. **Find the delete marker** of the snapshot: `aws s3api list-object-versions --bucket <bucket> --prefix stores/<store id>/snap/`. The snapshot's key has a `DeleteMarkers` entry with `IsLatest: true`.
2. **Remove the marker:** `aws s3api delete-object --bucket <bucket> --key <key> --version-id <the marker's VersionId>`. The snapshot is the current object again.
3. **Restore the store** from it, as in [Restoring a store](#restoring-a-store). The lifecycle task keeps a snapshot that a restore waits on, so do this right away. For a store that was erased, there is no store to restore: read the snapshot as in [Recovering a store from the bucket](#recovering-a-store-from-the-bucket).

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
