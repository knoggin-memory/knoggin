# Deploy on AWS

The OpenTofu module in `deploy/aws/` builds the Knoggin server in one AWS region. It is the target for people who must not see each other's memories. This guide copies the image, fills the secrets, applies the module, and covers sizing, upgrades and restores. The module's own `README.md` lists every part it makes.

**What it protects, and what not.** Each user's container runs alone on its own network interface, with no role and no way to other containers. Routers run on separate machines. Whoever operates the AWS account can still read every store; see [Security](security.md).

## What it makes

- Its own VPC, with two private subnets, two public subnets for the load balancer, one NAT gateway, S3 and DynamoDB endpoints, and a DNS firewall.
- Two routers on Fargate behind an Application Load Balancer.
- An ECS cluster of Graviton4 hosts (`m8g.xlarge` by default) for the user containers, one task per active user.
- A DynamoDB table (the state database), a KMS key (the master key) and an S3 bucket (encrypted snapshots).
- A lifecycle task every 10 minutes, and one CloudWatch log group per role, kept 30 days.

## Before you start

- OpenTofu 1.13, the AWS CLI, and `crane` (to copy the image).
- An ACM certificate for your host name, in the same region.
- An OpenID Connect identity provider, with a web application whose redirect address is `https://<your name>/auth/callback` ([Sign-in](sign-in.md)).
- The `knoggin` program on your machine, for one command ([Getting started](getting-started.md)).

## 1. Copy the image into your account

The hosts pull only from an ECR repository in the deployment's own account. Verify the release image, then copy it by digest:

```sh
cosign verify ghcr.io/knoggin-memory/knoggin@sha256:<digest> \
  --certificate-identity https://github.com/knoggin-memory/knoggin/.github/workflows/release.yml@refs/heads/main \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

aws ecr create-repository --repository-name knoggin --image-tag-mutability IMMUTABLE
aws ecr get-login-password | crane auth login <account>.dkr.ecr.<region>.amazonaws.com -u AWS --password-stdin
crane copy ghcr.io/knoggin-memory/knoggin@sha256:<digest> <account>.dkr.ecr.<region>.amazonaws.com/knoggin:<version>
```

`crane copy` keeps the digest, so the image is `<account>.dkr.ecr.<region>.amazonaws.com/knoggin@sha256:<digest>`.

## 2. Fill the secrets

Four Secrets Manager entries are filled by hand, so that no secret value ever sits in the OpenTofu state. Each value goes through a file of mode 0600, never a command argument, and the file is deleted afterwards:

```sh
umask 077
mkdir knoggin-secrets && cd knoggin-secrets

openssl rand -base64 32 > internal_secret
knoggin admin internal-cert --out-dir .          # writes internal_cert.pem and internal_key.pem
printf 'Web client secret: '; read -rs s; printf '%s' "$s" > web_client_secret; unset s; echo

aws secretsmanager create-secret --name knoggin/internal-secret --secret-string file://internal_secret
aws secretsmanager create-secret --name knoggin/internal-cert --secret-string file://internal_cert.pem
aws secretsmanager create-secret --name knoggin/internal-key --secret-string file://internal_key.pem
aws secretsmanager create-secret --name knoggin/web-client-secret --secret-string file://web_client_secret

cd .. && rm -r knoggin-secrets
```

Note the four ARNs. Leave the entries on the account's default key (`aws/secretsmanager`): the router's execution role has no permission for another.

## 3. Check your provider's hosts

The DNS firewall answers only names under `amazonaws.com`, the internal zone, the host of `auth_issuer`, and the names in `dns_allow`. If your provider serves its keys or tokens from another host, list it:

```sh
curl -s <issuer>/.well-known/openid-configuration | jq -r '.jwks_uri, .token_endpoint'
```

Put each host that differs from the issuer's in `dns_allow`. The firewall covers the whole VPC, so anything else you run there falls under the same list.

## 4. Apply

```sh
cd deploy/aws/examples/complete
cp terraform.tfvars.example terraform.tfvars    # fill it in
tofu init
tofu apply
```

Required variables:

| Variable | What |
| --- | --- |
| `region`, `name` | The region; a short name (default `knoggin`) that prefixes every resource |
| `public_url` | `https://<your name>`, no path |
| `certificate_arn` | The ACM certificate |
| `image` | The ECR image by digest, from step 1 |
| `auth_issuer`, `auth_web_client_id` | Your identity provider |
| `auth_web_client_secret_arn`, `internal_secret_arn`, `internal_cert_arn`, `internal_key_arn` | The four ARNs from step 2 |
| `admin_subjects` or `admin_emails` | At least one. Subjects are recommended |

Optional, with defaults: `instance_type` (`m8g.xlarge`), `min_hosts` (1), `max_hosts` (4), `container_memory_mib` (1536), `pool_target` (2), `retention_days` (7), `allowed_email_domains` (empty), `dns_allow` (empty), and `containers_per_host` (needed only for an instance type outside the module's table). The account setting `awsvpcTrunking` is turned on for the region.

When it is done, point your host name at the output `load_balancer_dns_name`, open `https://<your name>` and sign in. Agents connect to `https://<your name>/mcp` ([Connecting agents](connect-agents.md)).

**Nothing in front of Knoggin may log request addresses.** A request line can carry a search text, a tag or a one-time code. The module leaves the load balancer's access logs off. A proxy, CDN or tunnel of your own in front of it must not log them either.

**Check the module without an account:** `tofu fmt -check -recursive`, then `tofu init -backend=false -lockfile=readonly && tofu validate && tofu test` in `deploy/aws`. The tests use a mock provider and send nothing to AWS.

## Sizing

Each user's container gets `container_memory_mib` (1,536 MiB) and a share of the host's CPU. Two things cap the containers on one host: its memory, less a reserve, and its limit on network interfaces.

| Host | Memory | Containers at 1,536 MiB | Price per hour (us-east-1) | Per container-hour |
| --- | --- | --- | --- | --- |
| `m8g.large` | 8 GiB | 4 | $0.090 | $0.023 |
| `m8g.xlarge` | 16 GiB | 10 | $0.180 | $0.018 |
| `m8g.2xlarge` | 32 GiB | 20 | $0.359 | $0.018 |

These are estimates from AWS's documentation and prices of November 2026; they have not been measured on AWS yet. The module sets `containers.max` to `max_hosts` times the containers per host. Only active users hold a container: one parks after 10 idle minutes. CPU is shared, not reserved, so one busy user can slow the others on the same host.

A user's open stores must fit in the container's memory. Raise `container_memory_mib` if users with very large stores see `resource_exhausted`.

## Upgrade

1. Copy the new release image into ECR (step 1).
2. Set `image` to the new digest and run `tofu apply`.

The routers roll over one at a time. Running containers keep their build until they park; new claims use the new build once one of its containers is ready. Watch `knoggin_claims_total` by `outcome` and `knoggin_rollout_stalled` during a rollout ([Operations](operations.md)). To move everyone sooner, run `admin retire-before now` (below). Do not skip a release whose notes say it changes the internal protocol.

To roll back, apply the previous digest. An older router does not take the rollout back by itself; set `rollout.allow_downgrade` only if you mean to.

## Admin commands

Admin commands run as a one-off task with the router's task definition, so they have its role and settings:

```sh
aws ecs run-task --cluster <cluster_name> --task-definition <name>-router --launch-type FARGATE \
  --network-configuration "$(aws ecs describe-services --cluster <cluster_name> --services router \
      --query 'services[0].networkConfiguration' --output json)" \
  --overrides '{"containerOverrides":[{"name":"router","command":["admin","selftest"]}]}'
```

Replace `["admin","selftest"]` with the command you need. The output is in the router's log group. [Operations](operations.md) lists the commands.

## Restore

**One store from an older snapshot:** list its snapshots with `admin snapshots --store <id>` (or the web UI's Server status page), then run `admin restore --store <id> --snapshot <generation>-<sequence>`. The user's container parks, and the next open loads that snapshot. Nothing is deleted, so a restore can be undone by restoring the snapshot that was newest before.

**The state database:** DynamoDB point-in-time recovery is on, so AWS can restore the table as of a past moment, into a new table. Putting that table in place of the module's is a manual step the module does not automate. A restored table brings back what was true then: API keys revoked since then work again, ended sessions and removed users return. Revoke those keys and remove those users again afterwards.

**The bucket** has versioning off on purpose: an erased store must stay erased. Snapshots are the backup.

## The source offer

Knoggin is licensed under the AGPL, which gives the users of a network service the right to its source. Every deployment links to it: the setting `source_url` (default `https://github.com/knoggin-memory/knoggin`) appears in the web UI, in the session route and at the end of the MCP server instructions. **If you deploy a changed version, set `source_url` to where your changed source is.** The module has no variable for it: add `KNOGGIN_SOURCE_URL` to `router_env` in `deploy/aws/router.tf`.

## Checking isolation on a live stack

The isolation tests (`crates/knoggin-server/tests/stack_isolation.rs`) run against a deployed stack when `KNOGGIN_STACK_URL` and `KNOGGIN_STACK_CREDENTIALS` are set. Run them once before you announce a deployment. Without the two variables, the same test starts the one-host stack on Docker instead.

**What the run needs:**

- Two test identities, A and B, that your provider lets in, and one admin identity. The run removes B at the end. It leaves A two apps and their keys, named `appx…`, `appy…`, `keyx…` and `keyy…`.
- For A and B: the value of the `__Host-session` cookie of a browser signed in as that person, and an OAuth access token that your provider issued for this server, for instance to an MCP client that signed in as that person. For the admin: the cookie only. Get the tokens just before the run: they expire.
- A bucket in another AWS account, in the deployment's region, and keys of that account that may upload to it. The run leaves one small object `knoggin-probe/<id>-outside` there.
- Keys of the deployment's account that may describe task definitions, list and describe tasks, register and deregister a task definition, and run a task.
- The pinned `busybox` image in the deployment's ECR repository. The hosts can pull from nowhere else. Copy it by the digest in `tests/images.toml`:

  ```sh
  crane copy busybox@sha256:<digest from tests/images.toml> <account>.dkr.ecr.<region>.amazonaws.com/knoggin:probe-busybox-1.37.0
  ```

**Write the credentials file** with mode 0600; the test refuses a file that others can read:

```json
{
  "name": "knoggin",
  "a": { "session": "<A's cookie value>", "token": "<A's access token>" },
  "b": { "session": "<B's cookie value>", "token": "<B's access token>" },
  "admin": { "session": "<the admin's cookie value>" },
  "outside": {
    "bucket": "<bucket in another account>",
    "access_key_id": "<key ID of that account>",
    "secret_access_key": "<its secret>",
    "session_token": "<its session token, if the keys are temporary>"
  }
}
```

`name` is the module's `name` variable. It names the cluster and the task definitions.

**Run it** from a checkout, with `cargo-nextest`:

```sh
export KNOGGIN_STACK_URL=https://<your name>
export KNOGGIN_STACK_CREDENTIALS=$PWD/stack-test.json
export AWS_REGION=<region>
export AWS_ACCESS_KEY_ID=<key ID> AWS_SECRET_ACCESS_KEY=<secret> AWS_SESSION_TOKEN=<session token>
cargo nextest run -p knoggin-server --run-ignored only -E 'binary(stack_isolation)'
```

Set all three key variables. The repository's `.cargo/config.toml` turns off the shared AWS config and credentials files and the instance metadata for every test, and fills a key variable that is not set with a dummy value.

**What it checks.** Every isolation check that goes through the public address runs as on one host: each user and app reaches only its own stores, a revoked key is refused at once, an access token reaches only memories and the session route, and a removed user's credentials stop working. The network check (a user container reaches only the router's internal port) runs as a one-off probe task, because ECS cannot join a task's network namespace. The test registers a task definition `<name>-isolation-probe` for the `busybox` copy, runs it once on the user hosts with the EC2 launch type, the user subnets, the user security group and no role, and deregisters it. From there, another user container, the router's public and metrics ports, the metadata address, an outside address, a lookup of an outside name, and an upload to the outside bucket must all fail; only the router's internal port answers. The probe reports through its exit code, so it needs no log group. The test first makes the same upload from your machine, which must work, so that the refusal inside means something.

**What it skips, and why.** These steps need the Docker daemon of the one-host stack:

- the checks of the Compose file and the Caddyfile: they check files of the repository, not a stack (the module has `tofu test` for its own);
- the probes from inside a container's network namespace and from the other services, the description of a user container, and the second stack beside the first: the probe task above takes their place;
- the park of an idle container: it is seen in Docker's list of containers, and on AWS a container parks only after 10 idle minutes;
- the checks that nothing is stored or logged in plain text: they read the database dump, the stand-in bucket's files and the Compose logs. On AWS the data is in DynamoDB, S3 and CloudWatch, which the test does not read.

## See also

- [Operations](operations.md): health, metrics, logs, key rotation
- [Sign-in](sign-in.md)
- [Security](security.md)
