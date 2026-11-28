# SPDX-License-Identifier: AGPL-3.0-only
# The lifecycle task: one Fargate task every 10 minutes. It thins snapshots
# and erases deleted stores and users. It holds no key.

resource "aws_ecs_task_definition" "lifecycle" {
  family                   = "${var.name}-lifecycle"
  network_mode             = "awsvpc"
  requires_compatibilities = ["FARGATE"]
  cpu                      = 256
  memory                   = 512
  task_role_arn            = aws_iam_role.lifecycle.arn
  execution_role_arn       = aws_iam_role.lifecycle_execution.arn

  runtime_platform {
    cpu_architecture        = "ARM64"
    operating_system_family = "LINUX"
  }

  container_definitions = jsonencode([{
    name                   = "lifecycle"
    image                  = var.image
    command                = ["lifecycle"]
    essential              = true
    user                   = "65532:65532"
    readonlyRootFilesystem = true
    linuxParameters        = { capabilities = { drop = ["ALL"] } }
    environment = [for k, v in {
      AWS_REGION                    = var.region
      KNOGGIN_STATE__BACKEND        = "dynamodb"
      KNOGGIN_STATE__DYNAMODB_TABLE = aws_dynamodb_table.state.name
      KNOGGIN_STORAGE__BUCKET       = aws_s3_bucket.snapshots.bucket
      KNOGGIN_STORAGE__REGION       = var.region
      KNOGGIN_RETENTION__DAYS       = tostring(var.retention_days)
    } : { name = k, value = v }]
    logConfiguration = {
      logDriver = "awslogs"
      options = {
        awslogs-group         = aws_cloudwatch_log_group.this["lifecycle"].name
        awslogs-region        = var.region
        awslogs-stream-prefix = "l"
      }
    }
  }])
}

resource "aws_scheduler_schedule" "lifecycle" {
  name                = "${var.name}-lifecycle"
  schedule_expression = "rate(10 minutes)"

  flexible_time_window {
    mode = "OFF"
  }

  target {
    arn      = aws_ecs_cluster.this.arn
    role_arn = aws_iam_role.scheduler.arn

    ecs_parameters {
      task_definition_arn = aws_ecs_task_definition.lifecycle.arn
      launch_type         = "FARGATE"
      task_count          = 1

      network_configuration {
        subnets          = aws_subnet.private[*].id
        security_groups  = [aws_security_group.lifecycle.id]
        assign_public_ip = false
      }
    }
  }
}
