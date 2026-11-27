# SPDX-License-Identifier: AGPL-3.0-only
# The module's outputs.

output "load_balancer_dns_name" {
  description = "Point the public name (public_url) at this."
  value       = aws_lb.public.dns_name
}

output "bucket_name" {
  description = "The snapshot bucket."
  value       = aws_s3_bucket.snapshots.bucket
}

output "table_name" {
  description = "The state database."
  value       = aws_dynamodb_table.state.name
}

output "key_arn" {
  description = "The master key."
  value       = aws_kms_key.master.arn
}

output "cluster_name" {
  description = "The ECS cluster."
  value       = aws_ecs_cluster.this.name
}

output "log_group_names" {
  description = "One log group per role: router, user and lifecycle."
  value       = { for k, g in aws_cloudwatch_log_group.this : k => g.name }
}
