# Knoggin on AWS

One OpenTofu module (OpenTofu 1.13, provider `hashicorp/aws` 6.73.0) that builds the
Knoggin server in one region: routers on Fargate behind a load balancer, one ECS task
per user on Graviton4 hosts, DynamoDB for the state database, a KMS key for the master
key and an S3 bucket for encrypted snapshots. `docs/deploy-aws.md` is the guide: it
copies the release image into your ECR repository, fills the four Secrets Manager
entries and applies `examples/complete`.

**Who this is for.** A server for people who should not see each other's memories.

## What it makes

| File | Parts |
| --- | --- |
| `network.tf` | Its own VPC (it never takes an existing one): two private /20 subnets, two public subnets for the load balancer, one NAT gateway, S3 and DynamoDB gateway endpoints, and every security group rule |
| `dns.tf` | The routers' name `router.<name>.internal` (Cloud Map) and the DNS firewall |
| `storage.tf` | The snapshot bucket (versioning off), the state table, the master key and its policy |
| `cluster.tf` | `awsvpcTrunking`, the cluster, its capacity provider and the three log groups |
| `hosts.tf` | The launch template (ECS-optimized Amazon Linux 2023 arm64, tokens required, hop limit 1, encrypted root, no SSH key) and the Auto Scaling group |
| `user_task.tf` | The user task definition: no role, read-only root, no capabilities, no swap, no core dumps |
| `router.tf` | The router's task definition, its service and the load balancer |
| `lifecycle.tf` | The lifecycle task and its schedule (every 10 minutes) |
| `iam.tf` | Every role and its policy |
| `secrets.tf` | Which Secrets Manager entry becomes which router setting |

## Read before you apply

- **The DNS firewall covers the whole VPC.** It allows only names under `amazonaws.com`,
  `<name>.internal`, the host of `auth_issuer` and the names in `dns_allow`; every other
  lookup gets no answer. Anything else you run in this VPC falls under the same list. If
  your identity provider serves its keys or its token endpoint from another host than
  the issuer's, put that host in `dns_allow` (`docs/deploy-aws.md` shows how to read the
  hosts from the discovery document).
- **The S3 endpoint lets through only this deployment's bucket** (for the router's and
  the lifecycle task's roles) and the registry's layer bucket. Nothing in this VPC can
  reach any other bucket.
- **The account setting `awsvpcTrunking`** is set as the account's default in the region.
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

`tofu test` applies the module against a mock provider: it sends nothing to AWS and needs
no credentials.
