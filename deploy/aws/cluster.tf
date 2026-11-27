# SPDX-License-Identifier: AGPL-3.0-only
# Tab "Deployment", AWS: the cluster, its capacity and the logs.

# Trunking gives each user container its own network interface, so its security group
# applies to it alone. It must be on before any host registers.
resource "aws_ecs_account_setting_default" "trunking" {
  name  = "awsvpcTrunking"
  value = "enabled"
}

resource "aws_ecs_cluster" "this" {
  name = var.name
}

resource "aws_ecs_capacity_provider" "hosts" {
  name = "${var.name}-hosts"

  auto_scaling_group_provider {
    auto_scaling_group_arn         = aws_autoscaling_group.hosts.arn
    managed_termination_protection = "ENABLED"
    managed_draining               = "ENABLED"

    managed_scaling {
      status          = "ENABLED"
      target_capacity = 100
    }
  }
}

resource "aws_ecs_cluster_capacity_providers" "this" {
  cluster_name       = aws_ecs_cluster.this.name
  capacity_providers = [aws_ecs_capacity_provider.hosts.name]

  default_capacity_provider_strategy {
    capacity_provider = aws_ecs_capacity_provider.hosts.name
    weight            = 1
  }
}

resource "aws_cloudwatch_log_group" "this" {
  for_each          = toset(["router", "user", "lifecycle"])
  name              = "/${var.name}/${each.key}"
  retention_in_days = 30
}
