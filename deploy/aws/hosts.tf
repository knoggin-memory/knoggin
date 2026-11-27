# SPDX-License-Identifier: AGPL-3.0-only
# Tab "Deployment", AWS: the user hosts. Routers never run here.

locals {
  # ECS_TASK_PIDS_LIMIT caps every user task at 256 processes and threads. The core
  # pattern pipes core dumps to a program that drops them, so none lands on the disk.
  user_data = <<-EOT
    #!/bin/bash
    set -euo pipefail
    cat > /etc/sysctl.d/90-knoggin.conf <<'EOF'
    kernel.core_pattern=|/bin/false
    EOF
    sysctl -p /etc/sysctl.d/90-knoggin.conf
    cat > /etc/ecs/ecs.config <<'EOF'
    ECS_CLUSTER=${aws_ecs_cluster.this.name}
    ECS_AWSVPC_BLOCK_IMDS=true
    ECS_DISABLE_PRIVILEGED=true
    ECS_TASK_PIDS_LIMIT=256
    ECS_IMAGE_PULL_BEHAVIOR=prefer-cached
    ECS_RESERVED_MEMORY=512
    ECS_CONTAINER_STOP_TIMEOUT=120s
    EOF
  EOT
}

resource "aws_launch_template" "hosts" {
  name_prefix   = "${var.name}-hosts-"
  image_id      = "resolve:ssm:/aws/service/ecs/optimized-ami/amazon-linux-2023/arm64/recommended/image_id"
  instance_type = var.instance_type
  user_data     = base64encode(local.user_data)

  iam_instance_profile {
    arn = aws_iam_instance_profile.hosts.arn
  }

  vpc_security_group_ids = [aws_security_group.hosts.id]

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  block_device_mappings {
    device_name = "/dev/xvda"

    ebs {
      volume_type           = "gp3"
      volume_size           = 30
      encrypted             = true
      delete_on_termination = true
    }
  }
}

resource "aws_autoscaling_group" "hosts" {
  name_prefix           = "${var.name}-hosts-"
  vpc_zone_identifier   = aws_subnet.private[*].id
  min_size              = var.min_hosts
  max_size              = var.max_hosts
  protect_from_scale_in = true

  launch_template {
    id      = aws_launch_template.hosts.id
    version = aws_launch_template.hosts.latest_version
  }

  tag {
    key                 = "AmazonECSManaged"
    value               = "true"
    propagate_at_launch = true
  }

  tag {
    key                 = "Name"
    value               = "${var.name}-host"
    propagate_at_launch = true
  }

  # The capacity provider sets the size; trunking must be on before a host registers.
  lifecycle {
    ignore_changes = [desired_capacity]
  }

  depends_on = [aws_ecs_account_setting_default.trunking]
}
