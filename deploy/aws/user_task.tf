# SPDX-License-Identifier: AGPL-3.0-only
# The user task definition, and the rules for every user container.

locals {
  # <account>.dkr.ecr.<region>.amazonaws.com/<repository>@sha256:<digest>
  image_parts    = regex("^([0-9]{12})\\.dkr\\.ecr\\.([a-z0-9-]+)\\.amazonaws\\.com/([^@]+)@(sha256:[0-9a-f]{64})$", var.image)
  image_digest   = local.image_parts[3]
  repository_arn = "arn:aws:ecr:${local.image_parts[1]}:${local.image_parts[0]}:repository/${local.image_parts[2]}"
}

# No task role: the container holds no credential. On ec2 there is no execution role
# either: the host's instance role pulls the image and ships the logs. On fargate the
# user execution role (fargate.tf) does both. The router fills in the five environment
# values with a container override when it runs a task.
locals {
  user_container = {
    name                   = "store"
    image                  = var.image
    command                = ["serve"]
    essential              = true
    memory                 = var.container_memory_mib
    user                   = "65532:65532"
    privileged             = false
    readonlyRootFilesystem = true
    portMappings           = [{ containerPort = 8443, protocol = "tcp" }]
    stopTimeout            = 120
    logConfiguration = {
      logDriver = "awslogs"
      options = {
        awslogs-group         = aws_cloudwatch_log_group.this["user"].name
        awslogs-region        = var.region
        awslogs-stream-prefix = "u"
      }
    }
  }
  nofile = { name = "nofile", softLimit = 8192, hardLimit = 8192 }
}

resource "aws_ecs_task_definition" "user" {
  family                   = "${var.name}-user"
  network_mode             = "awsvpc"
  requires_compatibilities = local.ec2 ? ["EC2"] : ["FARGATE"]
  cpu                      = local.ec2 ? null : var.user_task_cpu
  memory                   = local.ec2 ? null : var.container_memory_mib
  execution_role_arn       = one(aws_iam_role.user_execution[*].arn)

  runtime_platform {
    cpu_architecture        = "ARM64"
    operating_system_family = "LINUX"
  }

  # On Fargate the task's CPU is the cap; it takes no swap setting and no Docker
  # security options, and nofile is the one limit set.
  container_definitions = local.ec2 ? jsonencode([merge(local.user_container, {
    cpu             = 128
    linuxParameters = { capabilities = { drop = ["ALL"] }, maxSwap = 0 }
    # Unconfirmed on ECS: if a task cannot start with this, remove this one line.
    dockerSecurityOptions = ["no-new-privileges"]
    ulimits               = [{ name = "core", softLimit = 0, hardLimit = 0 }, local.nofile]
    })]) : jsonencode([merge(local.user_container, {
    linuxParameters = { capabilities = { drop = ["ALL"] } }
    ulimits         = [local.nofile]
  })])

  lifecycle {
    precondition {
      condition     = local.image_parts[0] == local.account && local.image_parts[1] == var.region
      error_message = "image must be in an ECR repository of this account and region: the user tasks can pull from nowhere else."
    }
    precondition {
      condition     = !local.ec2 || var.max_containers == null
      error_message = "On user_compute = \"ec2\", containers.max is max_hosts times the containers per host: leave max_containers unset."
    }
    precondition {
      condition     = local.ec2 || var.max_containers != null
      error_message = "user_compute = \"fargate\" needs max_containers: the most user containers at once (containers.max)."
    }
    precondition {
      condition     = local.ec2 || contains(local.fargate_memory[var.user_task_cpu], var.container_memory_mib)
      error_message = "On user_compute = \"fargate\", container_memory_mib must be a size Fargate offers for user_task_cpu = ${var.user_task_cpu}: ${join(", ", local.fargate_memory[var.user_task_cpu])}. 2048 is recommended."
    }
  }
}
