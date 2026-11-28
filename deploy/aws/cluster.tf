# SPDX-License-Identifier: AGPL-3.0-only
# AWS: the cluster, its capacity and the logs.

# Trunking gives each user container its own network interface, so its security group
# applies to it alone. It must be on before any host registers.
resource "aws_ecs_account_setting_default" "trunking" {
  count = local.ec2 ? 1 : 0
  name  = "awsvpcTrunking"
  value = "enabled"
}

resource "aws_ecs_cluster" "this" {
  name = var.name
}

resource "aws_ecs_capacity_provider" "hosts" {
  count = local.ec2 ? 1 : 0
  name  = "${var.name}-hosts"

  auto_scaling_group_provider {
    auto_scaling_group_arn         = one(aws_autoscaling_group.hosts[*].arn)
    managed_termination_protection = "ENABLED"
    managed_draining               = "ENABLED"

    managed_scaling {
      status          = "ENABLED"
      target_capacity = 100
    }
  }
}

# User tasks run on the hosts' capacity provider, or on Fargate.
locals {
  capacity_provider = local.ec2 ? one(aws_ecs_capacity_provider.hosts[*].name) : "FARGATE"
}

resource "aws_ecs_cluster_capacity_providers" "this" {
  cluster_name       = aws_ecs_cluster.this.name
  capacity_providers = [local.capacity_provider]

  default_capacity_provider_strategy {
    capacity_provider = local.capacity_provider
    weight            = 1
  }
}

resource "aws_cloudwatch_log_group" "this" {
  for_each          = toset(["router", "user", "lifecycle"])
  name              = "/${var.name}/${each.key}"
  retention_in_days = 30
}
