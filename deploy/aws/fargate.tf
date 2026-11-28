# SPDX-License-Identifier: AGPL-3.0-only
# AWS: what user tasks need on user_compute = "fargate". Each user container is its own
# Fargate task. Its execution role pulls the image and ships the logs through interface
# endpoints, so the user security group still reaches nothing outside the VPC.

resource "aws_iam_role" "user_execution" {
  count              = local.ec2 ? 0 : 1
  name_prefix        = "${var.name}-user-exec-"
  assume_role_policy = local.trust.tasks
}

resource "aws_iam_role_policy" "user_execution" {
  count = local.ec2 ? 0 : 1
  name  = "user-execution"
  role  = one(aws_iam_role.user_execution[*].id)
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = concat(local.ecr_pull, [
      { Sid = "Logs", Effect = "Allow", Action = local.log_actions, Resource = "${aws_cloudwatch_log_group.this["user"].arn}:*" },
    ])
  })
}

# Private DNS sends every pull and log write in the VPC through these endpoints, the
# routers' and the lifecycle task's too, so all three groups may reach them.
resource "aws_security_group" "endpoints" {
  count       = local.ec2 ? 0 : 1
  name        = "${var.name}-endpoints"
  description = "Knoggin interface endpoints for image pulls and logs"
  vpc_id      = aws_vpc.this.id
}

resource "aws_vpc_endpoint" "fargate" {
  for_each            = toset(local.ec2 ? [] : ["ecr.api", "ecr.dkr", "logs"])
  vpc_id              = aws_vpc.this.id
  service_name        = "com.amazonaws.${var.region}.${each.key}"
  vpc_endpoint_type   = "Interface"
  subnet_ids          = aws_subnet.private[*].id
  security_group_ids  = [one(aws_security_group.endpoints[*].id)]
  private_dns_enabled = true

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid       = "ThisAccount"
      Effect    = "Allow"
      Principal = "*"
      Action    = "*"
      Resource  = "*"
      Condition = { StringEquals = { "aws:PrincipalAccount" = local.account } }
    }]
  })
}
