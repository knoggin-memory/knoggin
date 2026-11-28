# SPDX-License-Identifier: AGPL-3.0-only
# AWS permissions. The user task has no task role; on Fargate it has an execution role
# (fargate.tf). Every policy here is the module's own, so a test can read each statement.

locals {
  cluster_arn         = aws_ecs_cluster.this.arn
  container_instances = "arn:aws:ecs:${var.region}:${local.account}:container-instance/${var.name}/*"
  tasks               = "arn:aws:ecs:${var.region}:${local.account}:task/${var.name}/*"
  in_cluster          = { ArnEquals = { "ecs:cluster" = local.cluster_arn } }

  ecr_pull = [
    { Sid = "EcrToken", Effect = "Allow", Action = "ecr:GetAuthorizationToken", Resource = "*" },
    {
      Sid      = "EcrPull"
      Effect   = "Allow"
      Action   = ["ecr:BatchCheckLayerAvailability", "ecr:GetDownloadUrlForLayer", "ecr:BatchGetImage"]
      Resource = local.repository_arn
    },
  ]
  log_actions = ["logs:CreateLogStream", "logs:PutLogEvents"]

  # The service roles accept only calls made for this account.
  trust = merge(
    { for k, service in { tasks = "ecs-tasks.amazonaws.com", scheduler = "scheduler.amazonaws.com" } : k => jsonencode({
      Version = "2012-10-17"
      Statement = [{
        Effect    = "Allow"
        Principal = { Service = service }
        Action    = "sts:AssumeRole"
        Condition = { StringEquals = { "aws:SourceAccount" = local.account } }
      }]
    }) },
    {
      hosts = jsonencode({
        Version   = "2012-10-17"
        Statement = [{ Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole" }]
      })
    },
  )
}

# Hosts: register with this cluster, pull the image, write the user containers' logs.
# Without ecs:CreateCluster, ecs:UpdateContainerInstancesState and
# ecs:DeregisterContainerInstance, a host that was taken over cannot drain or remove the
# other hosts. ecs:DiscoverPollEndpoint supports no resource, so it is the one ECS action
# on "*".
resource "aws_iam_role" "hosts" {
  count              = local.ec2 ? 1 : 0
  name_prefix        = "${var.name}-hosts-"
  assume_role_policy = local.trust.hosts
}

resource "aws_iam_instance_profile" "hosts" {
  count       = local.ec2 ? 1 : 0
  name_prefix = "${var.name}-hosts-"
  role        = one(aws_iam_role.hosts[*].name)
}

resource "aws_iam_role_policy" "hosts" {
  count = local.ec2 ? 1 : 0
  name  = "ecs-host"
  role  = one(aws_iam_role.hosts[*].id)
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = concat([
      {
        Sid      = "EcsCluster"
        Effect   = "Allow"
        Action   = ["ecs:RegisterContainerInstance", "ecs:SubmitAttachmentStateChanges", "ecs:SubmitContainerStateChange", "ecs:SubmitTaskStateChange"]
        Resource = local.cluster_arn
      },
      {
        Sid       = "EcsInstance"
        Effect    = "Allow"
        Action    = ["ecs:Poll", "ecs:StartTelemetrySession"]
        Resource  = local.container_instances
        Condition = local.in_cluster
      },
      { Sid = "EcsDiscover", Effect = "Allow", Action = "ecs:DiscoverPollEndpoint", Resource = "*" },
      {
        Sid       = "EcsTagOwnInstance"
        Effect    = "Allow"
        Action    = "ecs:TagResource"
        Resource  = local.container_instances
        Condition = { StringEquals = { "ecs:CreateAction" = "RegisterContainerInstance" } }
      },
      { Sid = "Ec2Tags", Effect = "Allow", Action = "ec2:DescribeTags", Resource = "*" },
      {
        Sid      = "UserLogs"
        Effect   = "Allow"
        Action   = local.log_actions
        Resource = "${aws_cloudwatch_log_group.this["user"].arn}:*"
      },
    ], local.ecr_pull)
  })
}

# Router: start, list and stop user tasks in this cluster; read the bucket and upload
# create-only; use the master key with a store context; read and write the state table.
resource "aws_iam_role" "router" {
  name_prefix        = "${var.name}-router-"
  assume_role_policy = local.trust.tasks
}

resource "aws_iam_role_policy" "router" {
  name = "router"
  role = aws_iam_role.router.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = concat([
      {
        Sid       = "EcsRunUserTask"
        Effect    = "Allow"
        Action    = "ecs:RunTask"
        Resource  = "${aws_ecs_task_definition.user.arn_without_revision}:*"
        Condition = local.in_cluster
      },
      {
        # TagResource on a task offers no ecs:cluster key (only daemons do), so that
        # condition would never match; the task ARN names this cluster instead.
        Sid       = "EcsTagStartedTask"
        Effect    = "Allow"
        Action    = "ecs:TagResource"
        Resource  = local.tasks
        Condition = { StringEquals = { "ecs:CreateAction" = "RunTask" } }
      },
      {
        Sid       = "EcsUserTasks"
        Effect    = "Allow"
        Action    = ["ecs:StopTask", "ecs:DescribeTasks", "ecs:ListTasks", "ecs:DescribeContainerInstances"]
        Resource  = "*"
        Condition = local.in_cluster
      },
      { Sid = "S3List", Effect = "Allow", Action = "s3:ListBucket", Resource = aws_s3_bucket.snapshots.arn },
      { Sid = "S3Read", Effect = "Allow", Action = "s3:GetObject", Resource = "${aws_s3_bucket.snapshots.arn}/*" },
      {
        Sid      = "S3CreateOnly"
        Effect   = "Allow"
        Action   = "s3:PutObject"
        Resource = "${aws_s3_bucket.snapshots.arn}/*"
        # Only with If-None-Match: an upload can never overwrite.
        Condition = { Null = { "s3:if-none-match" = "false" } }
      },
      {
        Sid       = "Kms"
        Effect    = "Allow"
        Action    = ["kms:GenerateDataKey", "kms:Decrypt"]
        Resource  = aws_kms_key.master.arn
        Condition = local.kms_store_context
      },
      {
        # No PutItem: the code never calls it. A transaction needs no action of its own.
        Sid      = "State"
        Effect   = "Allow"
        Action   = ["dynamodb:GetItem", "dynamodb:Query", "dynamodb:UpdateItem", "dynamodb:DeleteItem", "dynamodb:ConditionCheckItem"]
        Resource = aws_dynamodb_table.state.arn
      },
      ], [
      # On Fargate, RunTask hands ECS the user task's execution role.
      for s in [{
        Sid       = "PassUserExecutionRole"
        Effect    = "Allow"
        Action    = "iam:PassRole"
        Resource  = one(aws_iam_role.user_execution[*].arn)
        Condition = { StringEquals = { "iam:PassedToService" = "ecs-tasks.amazonaws.com" } }
      }] : s if !local.ec2
    ])
  })
}

resource "aws_iam_role" "router_execution" {
  name_prefix        = "${var.name}-router-exec-"
  assume_role_policy = local.trust.tasks
}

resource "aws_iam_role_policy" "router_execution" {
  name = "router-execution"
  role = aws_iam_role.router_execution.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = concat(local.ecr_pull, [
      { Sid = "Logs", Effect = "Allow", Action = local.log_actions, Resource = "${aws_cloudwatch_log_group.this["router"].arn}:*" },
      { Sid = "Secrets", Effect = "Allow", Action = "secretsmanager:GetSecretValue", Resource = values(local.router_secrets) },
    ])
  })
}

# Lifecycle: list and delete objects; read and delete records, never change or create
# one, and never delete in the partition meta (the field key, the schema and the rollout
# record). No KMS.
resource "aws_iam_role" "lifecycle" {
  name_prefix        = "${var.name}-lifecycle-"
  assume_role_policy = local.trust.tasks
}

resource "aws_iam_role_policy" "lifecycle" {
  name = "lifecycle"
  role = aws_iam_role.lifecycle.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      { Sid = "S3List", Effect = "Allow", Action = "s3:ListBucket", Resource = aws_s3_bucket.snapshots.arn },
      { Sid = "S3Delete", Effect = "Allow", Action = "s3:DeleteObject", Resource = "${aws_s3_bucket.snapshots.arn}/*" },
      {
        Sid      = "State"
        Effect   = "Allow"
        Action   = ["dynamodb:GetItem", "dynamodb:Query", "dynamodb:DeleteItem"]
        Resource = aws_dynamodb_table.state.arn
      },
      {
        Sid       = "KeepMeta"
        Effect    = "Deny"
        Action    = "dynamodb:DeleteItem"
        Resource  = aws_dynamodb_table.state.arn
        Condition = { "ForAnyValue:StringEquals" = { "dynamodb:LeadingKeys" = ["meta"] } }
      },
    ]
  })
}

resource "aws_iam_role" "lifecycle_execution" {
  name_prefix        = "${var.name}-lifecycle-exec-"
  assume_role_policy = local.trust.tasks
}

resource "aws_iam_role_policy" "lifecycle_execution" {
  name = "lifecycle-execution"
  role = aws_iam_role.lifecycle_execution.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = concat(local.ecr_pull, [
      { Sid = "Logs", Effect = "Allow", Action = local.log_actions, Resource = "${aws_cloudwatch_log_group.this["lifecycle"].arn}:*" },
    ])
  })
}

# The scheduler is the one place that passes a role: the lifecycle task's two roles, to
# ECS tasks only.
resource "aws_iam_role" "scheduler" {
  name_prefix        = "${var.name}-scheduler-"
  assume_role_policy = local.trust.scheduler
}

resource "aws_iam_role_policy" "scheduler" {
  name = "run-lifecycle"
  role = aws_iam_role.scheduler.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "RunLifecycle"
        Effect    = "Allow"
        Action    = "ecs:RunTask"
        Resource  = "${aws_ecs_task_definition.lifecycle.arn_without_revision}:*"
        Condition = local.in_cluster
      },
      {
        Sid       = "PassLifecycleRoles"
        Effect    = "Allow"
        Action    = "iam:PassRole"
        Resource  = [aws_iam_role.lifecycle.arn, aws_iam_role.lifecycle_execution.arn]
        Condition = { StringEquals = { "iam:PassedToService" = "ecs-tasks.amazonaws.com" } }
      },
    ]
  })
}
