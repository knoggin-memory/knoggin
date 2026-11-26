# Tab "Deployment", "User task definition", and the rules for every user container.

locals {
  # <account>.dkr.ecr.<region>.amazonaws.com/<repository>@sha256:<digest>
  image_parts    = regex("^([0-9]{12})\\.dkr\\.ecr\\.([a-z0-9-]+)\\.amazonaws\\.com/([^@]+)@(sha256:[0-9a-f]{64})$", var.image)
  image_digest   = local.image_parts[3]
  repository_arn = "arn:aws:ecr:${local.image_parts[1]}:${local.image_parts[0]}:repository/${local.image_parts[2]}"
}

# No task role and no execution role: the host's instance role pulls the image and ships
# the logs, and the container holds no credential. The router fills in the five
# environment values with a container override when it runs a task.
resource "aws_ecs_task_definition" "user" {
  family                   = "${var.name}-user"
  network_mode             = "awsvpc"
  requires_compatibilities = ["EC2"]

  runtime_platform {
    cpu_architecture        = "ARM64"
    operating_system_family = "LINUX"
  }

  container_definitions = jsonencode([{
    name                   = "store"
    image                  = var.image
    command                = ["serve"]
    essential              = true
    cpu                    = 128
    memory                 = var.container_memory_mib
    user                   = "65532:65532"
    privileged             = false
    readonlyRootFilesystem = true
    portMappings           = [{ containerPort = 8443, protocol = "tcp" }]
    linuxParameters        = { capabilities = { drop = ["ALL"] }, maxSwap = 0 }
    # Measurement M11: if a task cannot start with this on ECS, remove this one line.
    dockerSecurityOptions = ["no-new-privileges"]
    ulimits = [
      { name = "core", softLimit = 0, hardLimit = 0 },
      { name = "nofile", softLimit = 8192, hardLimit = 8192 },
    ]
    stopTimeout = 120
    logConfiguration = {
      logDriver = "awslogs"
      options = {
        awslogs-group         = aws_cloudwatch_log_group.this["user"].name
        awslogs-region        = var.region
        awslogs-stream-prefix = "u"
      }
    }
  }])

  lifecycle {
    precondition {
      condition     = local.image_parts[0] == local.account && local.image_parts[1] == var.region
      error_message = "image must be in an ECR repository of this account and region: the hosts can pull from nowhere else."
    }
  }
}
