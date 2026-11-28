# SPDX-License-Identifier: AGPL-3.0-only
# The module's variables and its sizing table.

variable "name" {
  description = "Name of the deployment. Prefixes every resource; the internal zone is <name>.internal."
  type        = string

  validation {
    # The router's load balancer and target group names (<name>-router) must fit in 32
    # characters, and a capacity provider name may not start with aws, ecs or fargate.
    condition     = can(regex("^[a-z][a-z0-9-]{0,24}$", var.name)) && !can(regex("^(aws|ecs|fargate)", var.name))
    error_message = "name: 1 to 25 lower-case letters, digits and hyphens, starting with a letter, not with aws, ecs or fargate."
  }
}

variable "region" {
  description = "The AWS region. Must be the provider's region."
  type        = string
}

variable "public_url" {
  description = "The address users and agents use, such as https://memory.example.com (server.public_url)."
  type        = string

  validation {
    condition     = can(regex("^https://[^/]+$", var.public_url))
    error_message = "public_url must be https://<host> with no path."
  }
}

variable "certificate_arn" {
  description = "ACM certificate for the load balancer's HTTPS listener."
  type        = string
}

variable "image" {
  description = "The release image copied into an ECR repository of this account and region, by digest: <account>.dkr.ecr.<region>.amazonaws.com/<repository>@sha256:<digest>."
  type        = string

  validation {
    condition     = can(regex("^[0-9]{12}\\.dkr\\.ecr\\.[a-z0-9-]+\\.amazonaws\\.com/[a-z0-9][a-z0-9._/-]*@sha256:[0-9a-f]{64}$", var.image))
    error_message = "image must be an ECR image of this account pinned by digest: <account>.dkr.ecr.<region>.amazonaws.com/<repository>@sha256:<64 hex>."
  }
}

variable "auth_issuer" {
  description = "The identity provider's issuer URL (auth.issuer). Its host is allowed by the DNS firewall."
  type        = string

  validation {
    condition     = can(regex("^https://[^/:]+", var.auth_issuer))
    error_message = "auth_issuer must be an https URL."
  }
}

variable "auth_web_client_id" {
  description = "The web UI's client ID at the provider (auth.web_client_id)."
  type        = string
}

variable "auth_web_client_secret_arn" {
  description = "Secrets Manager entry that holds the web client secret, filled by hand."
  type        = string
}

variable "internal_secret_arn" {
  description = "Secrets Manager entry that holds internal.secret (32 random bytes, base64), filled by hand."
  type        = string
}

variable "internal_cert_arn" {
  description = "Secrets Manager entry that holds the internal certificate (PEM), filled by hand."
  type        = string
}

variable "internal_key_arn" {
  description = "Secrets Manager entry that holds the internal certificate's key (PEM), filled by hand."
  type        = string
}

variable "admin_subjects" {
  description = "Sign-in subjects of the admins (auth.admin_subjects). Set this or admin_emails; subjects are recommended."
  type        = list(string)
  default     = []
}

variable "admin_emails" {
  description = "Emails of the admins (auth.admin_emails). Set this or admin_subjects."
  type        = list(string)
  default     = []
}

variable "user_compute" {
  description = "Where user containers run: \"ec2\" on shared Graviton4 hosts, or \"fargate\" with one Fargate task (its own microVM) per container. Choose at the first apply."
  type        = string
  default     = "ec2"

  validation {
    condition     = contains(["ec2", "fargate"], var.user_compute)
    error_message = "user_compute must be \"ec2\" or \"fargate\"."
  }
}

variable "instance_type" {
  description = "Instance type of the user hosts (Graviton4). ec2 only."
  type        = string
  default     = "m8g.xlarge"
}

variable "min_hosts" {
  description = "Least number of user hosts. ec2 only."
  type        = number
  default     = 1
}

variable "max_hosts" {
  description = "Most user hosts. containers.max is this times the containers per host. ec2 only."
  type        = number
  default     = 4
}

variable "container_memory_mib" {
  description = "Memory of one user container: the task definition's limit and containers.memory_mib. On fargate it must be a size Fargate offers for user_task_cpu; 2048 is recommended."
  type        = number
  default     = 1536
}

variable "user_task_cpu" {
  description = "CPU units of one user task on fargate (1024 is one vCPU): 256, 512 or 1024. A hard cap. fargate only."
  type        = number
  default     = 512

  validation {
    condition     = contains([256, 512, 1024], var.user_task_cpu)
    error_message = "user_task_cpu must be 256, 512 or 1024."
  }
}

variable "max_containers" {
  description = "Most user containers at once (containers.max). Required on fargate; on ec2 it is max_hosts times the containers per host, and this stays unset."
  type        = number
  default     = null
}

variable "pool_target" {
  description = "Ready pool containers to keep (containers.pool_target)."
  type        = number
  default     = 2
}

variable "retention_days" {
  description = "Days of daily snapshots to keep (retention.days)."
  type        = number
  default     = 7
}

variable "allowed_email_domains" {
  description = "If set, only people with an email in these domains may use Knoggin (auth.allowed_email_domains)."
  type        = list(string)
  default     = []
}

variable "dns_allow" {
  description = "More host names the DNS firewall allows, such as the provider's key or token host when it differs from the issuer's."
  type        = list(string)
  default     = []
}

variable "containers_per_host" {
  description = "User containers that fit on one host. Needed only when instance_type is not in the module's table. ec2 only."
  type        = number
  default     = null
}

locals {
  # Memory and the network interface cap with trunking, from AWS documentation. A
  # measurement on a real account may correct these.
  hosts_table = {
    "m8g.large"   = { memory_gib = 8, interfaces = 10 }
    "m8g.xlarge"  = { memory_gib = 16, interfaces = 20 }
    "m8g.2xlarge" = { memory_gib = 32, interfaces = 40 }
  }
  host = lookup(local.hosts_table, var.instance_type, null)

  # The host's memory less 1 GiB for every 16 GiB (at least 1 GiB), divided by the
  # container memory, or the interface cap if that is smaller. ECS_RESERVED_MEMORY comes
  # out of that reserve.
  fit = local.host == null ? null : min(
    local.host.interfaces,
    floor((local.host.memory_gib - max(1, floor(local.host.memory_gib / 16))) * 1024 / var.container_memory_mib),
  )
  containers_per_host = coalesce(var.containers_per_host, local.fit, 0)

  ec2            = var.user_compute == "ec2"
  containers_max = local.ec2 ? var.max_hosts * local.containers_per_host : var.max_containers

  # The task memory sizes Fargate offers for each CPU size.
  fargate_memory = {
    256  = [512, 1024, 2048]
    512  = [1024, 2048, 3072, 4096]
    1024 = [2048, 3072, 4096, 5120, 6144, 7168, 8192]
  }
}
