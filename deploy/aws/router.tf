# SPDX-License-Identifier: AGPL-3.0-only
# AWS: the routers on Fargate behind a load balancer. They never run on
# the user hosts.

locals {
  router_url = "https://router.${var.name}.internal:8443"

  # The router's config, as the environment variables the config loader reads. Lists are
  # comma-separated.
  router_env = {
    AWS_REGION                               = var.region
    KNOGGIN_SERVER__PUBLIC_URL               = var.public_url
    KNOGGIN_SERVER__TRUSTED_PROXIES          = join(",", aws_subnet.public[*].cidr_block)
    KNOGGIN_SERVER__IMAGE_DIGEST             = local.image_digest
    KNOGGIN_AUTH__ISSUER                     = var.auth_issuer
    KNOGGIN_AUTH__WEB_CLIENT_ID              = var.auth_web_client_id
    KNOGGIN_AUTH__ADMIN_SUBJECTS             = join(",", var.admin_subjects)
    KNOGGIN_AUTH__ADMIN_EMAILS               = join(",", var.admin_emails)
    KNOGGIN_AUTH__ALLOWED_EMAIL_DOMAINS      = join(",", var.allowed_email_domains)
    KNOGGIN_STATE__BACKEND                   = "dynamodb"
    KNOGGIN_STATE__DYNAMODB_TABLE            = aws_dynamodb_table.state.name
    KNOGGIN_STORAGE__BUCKET                  = aws_s3_bucket.snapshots.bucket
    KNOGGIN_STORAGE__REGION                  = var.region
    KNOGGIN_RETENTION__DAYS                  = tostring(var.retention_days)
    KNOGGIN_KEYS__PROVIDER                   = "kms"
    KNOGGIN_KEYS__KMS_KEY_ID                 = aws_kms_key.master.arn
    KNOGGIN_LAUNCHER__KIND                   = "ecs"
    KNOGGIN_LAUNCHER__ECS__CLUSTER           = aws_ecs_cluster.this.arn
    KNOGGIN_LAUNCHER__ECS__TASK_DEFINITION   = aws_ecs_task_definition.user.arn
    KNOGGIN_LAUNCHER__ECS__SUBNETS           = join(",", aws_subnet.private[*].id)
    KNOGGIN_LAUNCHER__ECS__SECURITY_GROUPS   = aws_security_group.user.id
    KNOGGIN_LAUNCHER__ECS__CAPACITY_PROVIDER = local.capacity_provider
    KNOGGIN_CONTAINERS__MEMORY_MIB           = tostring(var.container_memory_mib)
    KNOGGIN_CONTAINERS__POOL_TARGET          = tostring(var.pool_target)
    KNOGGIN_CONTAINERS__MAX                  = tostring(local.containers_max)
    KNOGGIN_INTERNAL__ROUTER_URL             = local.router_url
  }
}

resource "aws_ecs_task_definition" "router" {
  family                   = "${var.name}-router"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = 512
  memory                   = 1024
  task_role_arn            = aws_iam_role.router.arn
  execution_role_arn       = aws_iam_role.router_execution.arn

  runtime_platform {
    cpu_architecture        = "ARM64"
    operating_system_family = "LINUX"
  }

  container_definitions = jsonencode([{
    name                   = "router"
    image                  = var.image
    command                = ["router"]
    essential              = true
    user                   = "65532:65532"
    readonlyRootFilesystem = true
    linuxParameters        = { capabilities = { drop = ["ALL"] } }
    portMappings = [
      { containerPort = 8080, protocol = "tcp" },
      { containerPort = 8443, protocol = "tcp" },
    ]
    environment = [for k, v in local.router_env : { name = k, value = v }]
    secrets     = [for k, v in local.router_secrets : { name = k, valueFrom = v }]
    stopTimeout = 120
    logConfiguration = {
      logDriver = "awslogs"
      options = {
        awslogs-group         = aws_cloudwatch_log_group.this["router"].name
        awslogs-region        = var.region
        awslogs-stream-prefix = "r"
      }
    }
  }])

  lifecycle {
    precondition {
      condition     = length(var.admin_subjects) + length(var.admin_emails) > 0
      error_message = "Set admin_subjects (recommended) or admin_emails."
    }
    precondition {
      condition     = !local.ec2 || local.containers_per_host > 0
      error_message = "instance_type is not in the module's table (m8g.large, m8g.xlarge, m8g.2xlarge): set containers_per_host."
    }
  }
}

resource "aws_lb" "public" {
  name                       = "${var.name}-router"
  load_balancer_type         = "application"
  subnets                    = aws_subnet.public[*].id
  security_groups            = [aws_security_group.alb.id]
  idle_timeout               = 300
  drop_invalid_header_fields = true
  # No access_logs block: a request line can carry a search text or a one-time code.
}

resource "aws_lb_target_group" "router" {
  name        = "${var.name}-router"
  port        = 8080
  protocol    = "HTTP"
  target_type = "ip"
  vpc_id      = aws_vpc.this.id

  health_check {
    path    = "/readyz"
    matcher = "200"
  }
}

resource "aws_lb_listener" "https" {
  load_balancer_arn = aws_lb.public.arn
  port              = 443
  protocol          = "HTTPS"
  ssl_policy        = "ELBSecurityPolicy-TLS13-1-2-2021-06"
  certificate_arn   = var.certificate_arn

  default_action {
    type             = "forward"
    target_group_arn = aws_lb_target_group.router.arn
  }
}

resource "aws_ecs_service" "router" {
  name            = "router"
  cluster         = aws_ecs_cluster.this.id
  task_definition = aws_ecs_task_definition.router.arn
  desired_count   = 2
  launch_type     = "FARGATE"
  # The router checks its settings, the database, the provider and the bucket first.
  health_check_grace_period_seconds = 120

  network_configuration {
    subnets          = aws_subnet.private[*].id
    security_groups  = [aws_security_group.router.id]
    assign_public_ip = false
  }

  load_balancer {
    target_group_arn = aws_lb_target_group.router.arn
    container_name   = "router"
    container_port   = 8080
  }

  service_registries {
    registry_arn = aws_service_discovery_service.router.arn
  }

  depends_on = [aws_lb_listener.https]
}
