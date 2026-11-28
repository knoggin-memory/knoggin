# Knoggin on AWS

One module for OpenTofu 1.13 or Terraform 1.13 (provider `hashicorp/aws` 6.73.0) that
builds the Knoggin server in one region: routers on Fargate behind a load balancer, user
containers as ECS tasks on shared Graviton4 hosts or on Fargate, DynamoDB for the state
database, a KMS key for the master key and an S3 bucket for encrypted snapshots. `docs/deploy-aws.md` is the guide: it
copies the release image into your ECR repository, fills the four Secrets Manager
entries and applies `examples/complete`.

**Who this is for.** A server for people who should not see each other's memories.

## What it makes

| File | Parts |
| --- | --- |
| `network.tf` | Its own VPC (it never takes an existing one): two private /20 subnets, two public subnets for the load balancer, one NAT gateway, S3 and DynamoDB gateway endpoints, and every security group rule |
| `dns.tf` | The routers' name `router.<name>.internal` (Cloud Map) and the DNS firewall |
| `storage.tf` | The snapshot bucket (versioning off unless `undelete_days` is set, and a bucket policy of denies only), the state table, the master key and its policy |
| `cluster.tf` | `awsvpcTrunking` and the hosts' capacity provider (`ec2`), the cluster and the three log groups |
| `hosts.tf` | `ec2` only: the launch template (ECS-optimized Amazon Linux 2023 arm64, tokens required, hop limit 1, encrypted root, no SSH key) and the Auto Scaling group |
| `fargate.tf` | `fargate` only: the user execution role, and interface endpoints for ECR and CloudWatch Logs with their security group |
| `user_task.tf` | The user task definition: no task role, read-only root, no capabilities; on `ec2` also no swap and no core dumps |
| `router.tf` | The router's task definition, its service and the load balancer |
| `lifecycle.tf` | The lifecycle task and its schedule (every 10 minutes) |
| `iam.tf` | Every role and its policy |
| `secrets.tf` | Which Secrets Manager entry becomes which router setting |

## Hosts or Fargate

`user_compute` chooses where user containers run, for all of them:

| | `"ec2"` (default) | `"fargate"` |
| --- | --- | --- |
| Isolation | One network interface per container on hosts shared by many users | One Fargate task, its own microVM, per container |
| CPU | A share of the host; bursts | `user_task_cpu` (256, 512 or 1024; default 512), a hard cap |
| Memory | `container_memory_mib` (default 1536) | `container_memory_mib`, a size Fargate offers for `user_task_cpu`; 2048 is recommended |
| `containers.max` | `max_hosts` times the containers per host | `max_containers`, required |
| Variables used | `instance_type`, `min_hosts`, `max_hosts`, `containers_per_host` | `user_task_cpu`, `max_containers` |

A choice that does not fit fails at plan, with a message that names the fix. Choose at
the first apply; switching later is a planned outage (`docs/deploy-aws.md`).

## Read before you apply

- **The DNS firewall covers the whole VPC.** It allows only names under `amazonaws.com`,
  `<name>.internal`, the host of `auth_issuer` and the names in `dns_allow`; every other
  lookup gets no answer. Anything else you run in this VPC falls under the same list. If
  your identity provider serves its keys or its token endpoint from another host than
  the issuer's, put that host in `dns_allow` (`docs/deploy-aws.md` shows how to read the
  hosts from the discovery document).
- **The S3 endpoint lets through only this deployment's bucket** (the router's role may
  list, read and upload; the lifecycle task's role may list and delete) and the
  registry's layer bucket. Nothing in this VPC can reach any other bucket.
- **The bucket policy holds denies only.** The router can never delete or overwrite a
  snapshot, the lifecycle task can never read, upload or touch old versions, and neither
  can change the bucket's versioning, lifecycle rules, policy or ACLs, whatever their IAM
  policies say.
- **`undelete_days`** (0 to 30, default 0) turns on bucket versioning: a deleted snapshot
  stays as an old version for about that many days to one more. Once above 0 it cannot
  go back to 0, because S3 cannot return a bucket to unversioned; set it to 1 instead.
- **The account setting `awsvpcTrunking`** is set as the account's default in the region
  on `ec2`.
- **On `fargate`, the interface endpoints for ECR and CloudWatch Logs have private DNS.**
  Every image pull and log write in this VPC goes through them, the routers' and the
  lifecycle task's too, and they let through only principals of this account.
- **The load balancer keeps no access logs**, because a request line can carry a search
  text or a one-time code. Do not turn them on.
- The Secrets Manager entries are encrypted with the account's default key
  (`aws/secretsmanager`); the router's execution role has no KMS permission for another.

## Checking it without an account

```sh
tofu fmt -check -recursive
tofu init -backend=false -lockfile=readonly && tofu validate && tofu test
(cd examples/complete && tofu init -backend=false -lockfile=readonly && tofu validate)
```

With Terraform: `terraform init -backend=false && terraform validate && terraform test`.
The committed lock file is OpenTofu's, so Terraform adds its own entry.

The tests apply the module against a mock provider: they send nothing to AWS and need no
credentials.
