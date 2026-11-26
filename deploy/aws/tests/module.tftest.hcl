# Tab "Deployment", "Checking the module without an account". Every run applies the module
# against a mock provider: nothing is sent to AWS and no credential is needed.

mock_provider "aws" {
  mock_data "aws_caller_identity" {
    defaults = { account_id = "111122223333" }
  }
  mock_data "aws_region" {
    defaults = { region = "eu-west-1", name = "eu-west-1" }
  }
  mock_data "aws_availability_zones" {
    defaults = { names = ["eu-west-1a", "eu-west-1b", "eu-west-1c"] }
  }
}

# The provider checks the form of ARNs it is handed, so the resources whose ARN another
# one takes get real-looking, distinct ones.
override_resource {
  target = aws_ecs_cluster.this
  values = { arn = "arn:aws:ecs:eu-west-1:111122223333:cluster/knoggin" }
}

override_resource {
  target = aws_iam_instance_profile.hosts
  values = { arn = "arn:aws:iam::111122223333:instance-profile/knoggin-hosts" }
}

override_resource {
  target = aws_iam_role.hosts
  values = { arn = "arn:aws:iam::111122223333:role/knoggin-hosts" }
}

override_resource {
  target = aws_iam_role.router
  values = { arn = "arn:aws:iam::111122223333:role/knoggin-router" }
}

override_resource {
  target = aws_iam_role.router_execution
  values = { arn = "arn:aws:iam::111122223333:role/knoggin-router-exec" }
}

override_resource {
  target = aws_iam_role.lifecycle
  values = { arn = "arn:aws:iam::111122223333:role/knoggin-lifecycle" }
}

override_resource {
  target = aws_iam_role.lifecycle_execution
  values = { arn = "arn:aws:iam::111122223333:role/knoggin-lifecycle-exec" }
}

override_resource {
  target = aws_iam_role.scheduler
  values = { arn = "arn:aws:iam::111122223333:role/knoggin-scheduler" }
}

override_resource {
  target = aws_lb.public
  values = { arn = "arn:aws:elasticloadbalancing:eu-west-1:111122223333:loadbalancer/app/knoggin-router/1" }
}

override_resource {
  target = aws_lb_target_group.router
  values = { arn = "arn:aws:elasticloadbalancing:eu-west-1:111122223333:targetgroup/knoggin-router/1" }
}

override_resource {
  target = aws_s3_bucket.snapshots
  values = { arn = "arn:aws:s3:::knoggin-snapshots-1", bucket = "knoggin-snapshots-1" }
}

override_resource {
  target = aws_launch_template.hosts
  values = { id = "lt-0123456789abcdef0" }
}

override_resource {
  target = aws_ecs_task_definition.user
  values = {
    arn                  = "arn:aws:ecs:eu-west-1:111122223333:task-definition/knoggin-user:1"
    arn_without_revision = "arn:aws:ecs:eu-west-1:111122223333:task-definition/knoggin-user"
  }
}

override_resource {
  target = aws_ecs_task_definition.lifecycle
  values = {
    arn                  = "arn:aws:ecs:eu-west-1:111122223333:task-definition/knoggin-lifecycle:1"
    arn_without_revision = "arn:aws:ecs:eu-west-1:111122223333:task-definition/knoggin-lifecycle"
  }
}

override_resource {
  target = aws_autoscaling_group.hosts
  values = { arn = "arn:aws:autoscaling:eu-west-1:111122223333:autoScalingGroup:0:autoScalingGroupName/knoggin-hosts" }
}

override_resource {
  target = aws_service_discovery_service.router
  values = { arn = "arn:aws:servicediscovery:eu-west-1:111122223333:service/srv-0123456789abcdef" }
}

# The mock gives every resource of a type the same ID, so the groups and domain lists that
# the tests tell apart get their own.
override_resource {
  target = aws_security_group.alb
  values = { id = "sg-alb" }
}

override_resource {
  target = aws_security_group.router
  values = { id = "sg-router" }
}

override_resource {
  target = aws_security_group.user
  values = { id = "sg-user" }
}

override_resource {
  target = aws_security_group.hosts
  values = { id = "sg-hosts" }
}

override_resource {
  target = aws_security_group.lifecycle
  values = { id = "sg-lifecycle" }
}

override_resource {
  target = aws_route53_resolver_firewall_domain_list.allow
  values = { id = "rslvr-fdl-allow" }
}

override_resource {
  target = aws_route53_resolver_firewall_domain_list.everything
  values = { id = "rslvr-fdl-everything" }
}

variables {
  name                       = "knoggin"
  region                     = "eu-west-1"
  public_url                 = "https://memory.example.com"
  certificate_arn            = "arn:aws:acm:eu-west-1:111122223333:certificate/test"
  image                      = "111122223333.dkr.ecr.eu-west-1.amazonaws.com/knoggin@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  auth_issuer                = "https://auth.example.com/tenant"
  auth_web_client_id         = "client_test"
  auth_web_client_secret_arn = "arn:aws:secretsmanager:eu-west-1:111122223333:secret:web"
  internal_secret_arn        = "arn:aws:secretsmanager:eu-west-1:111122223333:secret:secret"
  internal_cert_arn          = "arn:aws:secretsmanager:eu-west-1:111122223333:secret:cert"
  internal_key_arn           = "arn:aws:secretsmanager:eu-west-1:111122223333:secret:key"
  admin_subjects             = ["user_admin"]
}

run "user_task_definition" {
  assert {
    condition     = aws_ecs_task_definition.user.task_role_arn == null && aws_ecs_task_definition.user.execution_role_arn == null
    error_message = "The user task definition must have no role."
  }
  assert {
    condition = (
      aws_ecs_task_definition.user.network_mode == "awsvpc" &&
      aws_ecs_task_definition.user.requires_compatibilities == toset(["EC2"]) &&
      one(aws_ecs_task_definition.user.runtime_platform).cpu_architecture == "ARM64" &&
      length(aws_ecs_task_definition.user.volume) == 0
    )
    error_message = "The user task runs on EC2 hosts, Arm64, in awsvpc mode, without volumes."
  }
  assert {
    condition     = jsondecode(aws_ecs_task_definition.user.container_definitions)[0].readonlyRootFilesystem == true
    error_message = "The root filesystem must be read-only."
  }
  assert {
    condition     = jsondecode(aws_ecs_task_definition.user.container_definitions)[0].linuxParameters.capabilities.drop == ["ALL"]
    error_message = "Every capability must be dropped."
  }
  assert {
    condition     = !contains(keys(jsondecode(aws_ecs_task_definition.user.container_definitions)[0].linuxParameters.capabilities), "add")
    error_message = "No capability may be added."
  }
  assert {
    condition     = jsondecode(aws_ecs_task_definition.user.container_definitions)[0].linuxParameters.maxSwap == 0
    error_message = "maxSwap must be 0."
  }
  assert {
    condition     = jsondecode(aws_ecs_task_definition.user.container_definitions)[0].user == "65532:65532"
    error_message = "The container must run as 65532:65532."
  }
  assert {
    condition = [
      for u in jsondecode(aws_ecs_task_definition.user.container_definitions)[0].ulimits : [u.softLimit, u.hardLimit] if u.name == "core"
    ] == [[0, 0]]
    error_message = "The core limit must be 0."
  }
  # Measurement M11: drop this assertion with the line, if ECS cannot start the task.
  assert {
    condition     = jsondecode(aws_ecs_task_definition.user.container_definitions)[0].dockerSecurityOptions == ["no-new-privileges"]
    error_message = "The container must carry no-new-privileges."
  }
  assert {
    condition = (
      jsondecode(aws_ecs_task_definition.user.container_definitions)[0].privileged == false &&
      jsondecode(aws_ecs_task_definition.user.container_definitions)[0].memory == 1024 &&
      jsondecode(aws_ecs_task_definition.user.container_definitions)[0].stopTimeout == 120 &&
      length(setintersection(keys(jsondecode(aws_ecs_task_definition.user.container_definitions)[0]), ["environment", "secrets", "mountPoints"])) == 0
    )
    error_message = "Not privileged, 1,024 MiB, a 120-second stop, and no environment, secret or mount of its own."
  }
}

run "kms" {
  assert {
    condition = [
      for s in jsondecode(aws_iam_role_policy.router.policy).Statement : s.Condition if contains(flatten([s.Action]), "kms:Decrypt")
    ] == [{ "ForAnyValue:StringEquals" = { "kms:EncryptionContextKeys" = ["knoggin:store"] } }]
    error_message = "The router's KMS statement must carry ForAnyValue:StringEquals on kms:EncryptionContextKeys."
  }
  assert {
    condition = [
      for s in jsondecode(aws_kms_key.master.policy).Statement : s.Condition if contains(flatten([s.Action]), "kms:Decrypt")
    ] == [{ "ForAnyValue:StringEquals" = { "kms:EncryptionContextKeys" = ["knoggin:store"] } }]
    error_message = "The key policy's router statement must carry the same condition."
  }
  # No principal but the router's task role may use the key.
  assert {
    condition = alltrue([
      for s in jsondecode(aws_kms_key.master.policy).Statement : s.Principal.AWS == aws_iam_role.router.arn
      if s.Effect == "Allow" && anytrue([for a in flatten([s.Action]) : can(regex("\\*|^kms:(Decrypt|Encrypt|GenerateDataKey|ReEncrypt|CreateGrant)", a))])
    ])
    error_message = "Only the router's task role may use the key."
  }
  # The account root manages the key without any use action or wildcard.
  assert {
    condition = alltrue([
      for s in jsondecode(aws_kms_key.master.policy).Statement : alltrue([
        for a in flatten([s.Action]) : !can(regex("\\*|^kms:(Decrypt|GenerateDataKey|Encrypt|ReEncrypt|CreateGrant|Create)", a))
      ]) if s.Principal.AWS == "arn:aws:iam::111122223333:root"
    ])
    error_message = "The account root statement may hold none of Decrypt, GenerateDataKey*, Encrypt, ReEncrypt*, CreateGrant, Create* or kms:*."
  }
  assert {
    condition     = length([for s in jsondecode(aws_kms_key.master.policy).Statement : s if s.Principal.AWS == "arn:aws:iam::111122223333:root"]) == 1
    error_message = "The key policy must keep one statement for the account root."
  }
  assert {
    condition     = aws_kms_key.master.enable_key_rotation && aws_kms_key.master.rotation_period_in_days == 365
    error_message = "Yearly rotation must be on."
  }
}

run "network_rules" {
  assert {
    condition = [
      for r in aws_vpc_security_group_ingress_rule.this : jsonencode([r.ip_protocol, r.from_port, r.to_port, r.referenced_security_group_id, r.cidr_ipv4, r.prefix_list_id])
      if r.security_group_id == aws_security_group.user.id
    ] == [jsonencode(["tcp", 8443, 8443, aws_security_group.router.id, null, null])]
    error_message = "User containers accept TCP 8443 from the router's group and nothing else."
  }
  assert {
    condition = toset([
      for r in aws_vpc_security_group_egress_rule.this : jsonencode([r.ip_protocol, r.from_port, r.to_port, r.referenced_security_group_id, r.cidr_ipv4, r.prefix_list_id])
      if r.security_group_id == aws_security_group.user.id
      ]) == toset([
      jsonencode(["tcp", 443, 443, null, null, aws_vpc_endpoint.s3.prefix_list_id]),
      jsonencode(["tcp", 8443, 8443, aws_security_group.router.id, null, null]),
    ])
    error_message = "User containers may reach only the S3 prefix list on 443 and the router's group on 8443."
  }
  assert {
    condition = length(concat(
      [for r in aws_vpc_security_group_ingress_rule.this : r if r.security_group_id == aws_security_group.user.id && r.referenced_security_group_id == aws_security_group.user.id],
      [for r in aws_vpc_security_group_egress_rule.this : r if r.security_group_id == aws_security_group.user.id && r.referenced_security_group_id == aws_security_group.user.id],
    )) == 0
    error_message = "No rule of the user group may name the group itself."
  }
  assert {
    condition     = length(aws_security_group.user.ingress) == 0 && length(aws_security_group.user.egress) == 0
    error_message = "The user group holds no inline rule."
  }
  assert {
    condition     = length(aws_default_security_group.this.ingress) == 0 && length(aws_default_security_group.this.egress) == 0
    error_message = "The VPC's default group has every rule removed."
  }
  assert {
    condition     = length([for r in aws_vpc_security_group_ingress_rule.this : r if r.security_group_id == aws_security_group.hosts.id || r.security_group_id == aws_security_group.lifecycle.id]) == 0
    error_message = "The hosts and the lifecycle task accept no inbound traffic."
  }
}

run "s3_endpoint" {
  assert {
    condition = toset([
      for r in flatten([for s in jsondecode(aws_vpc_endpoint.s3.policy).Statement : s.Resource]) : regex("^arn:aws:s3:::([^/]+)", r)[0]
    ]) == toset([aws_s3_bucket.snapshots.bucket, "prod-eu-west-1-starport-layer-bucket"])
    error_message = "The S3 endpoint's policy names only the deployment's bucket and the registry's layer bucket."
  }
  assert {
    condition = [
      for s in jsondecode(aws_vpc_endpoint.s3.policy).Statement : s.Condition.ArnEquals["aws:PrincipalArn"]
      if contains(flatten([s.Resource]), aws_s3_bucket.snapshots.arn)
    ] == [[aws_iam_role.router.arn, aws_iam_role.lifecycle.arn]]
    error_message = "Only requests signed by the router's or the lifecycle task's role reach the bucket."
  }
  assert {
    condition = (
      aws_s3_bucket_versioning.snapshots.versioning_configuration[0].status == "Disabled" &&
      aws_s3_bucket_public_access_block.snapshots.block_public_acls &&
      aws_s3_bucket_public_access_block.snapshots.block_public_policy &&
      aws_s3_bucket_public_access_block.snapshots.ignore_public_acls &&
      aws_s3_bucket_public_access_block.snapshots.restrict_public_buckets
    )
    error_message = "The bucket has versioning off and public access blocked."
  }
}

run "dns_firewall" {
  assert {
    condition = (
      aws_route53_resolver_firewall_rule_group_association.this.vpc_id == aws_vpc.this.id &&
      aws_route53_resolver_firewall_rule_group_association.this.firewall_rule_group_id == aws_route53_resolver_firewall_rule_group.this.id
    )
    error_message = "The rule group is attached to the VPC."
  }
  assert {
    condition = [
      for r in aws_route53_resolver_firewall_rule.this : [r.action, r.firewall_domain_list_id]
      if r.priority == max([for x in aws_route53_resolver_firewall_rule.this : x.priority]...)
    ] == [["BLOCK", aws_route53_resolver_firewall_domain_list.everything.id]]
    error_message = "The last rule blocks."
  }
  assert {
    condition     = aws_route53_resolver_firewall_domain_list.everything.domains == toset(["*"])
    error_message = "The last rule's list matches every name."
  }
  assert {
    condition     = alltrue([for r in aws_route53_resolver_firewall_rule.this : r.firewall_rule_group_id == aws_route53_resolver_firewall_rule_group.this.id])
    error_message = "Every rule belongs to the attached group."
  }
  assert {
    condition = aws_route53_resolver_firewall_domain_list.allow.domains == toset([
      "amazonaws.com", "*.amazonaws.com", "knoggin.internal", "*.knoggin.internal", "auth.example.com",
    ])
    error_message = "The allow list holds AWS names, the internal zone and the issuer's host, and nothing else."
  }
}

run "router_permissions" {
  assert {
    condition = [
      for s in jsondecode(aws_iam_role_policy.router.policy).Statement : s.Condition if contains(flatten([s.Action]), "s3:PutObject")
    ] == [{ Null = { "s3:if-none-match" = "false" } }]
    error_message = "The router's S3 upload must carry the create-only condition."
  }
  assert {
    condition = alltrue([
      for a in flatten([for s in jsondecode(aws_iam_role_policy.router.policy).Statement : s.Action]) : !can(regex("\\*|Delete", a)) || a == "dynamodb:DeleteItem"
    ])
    error_message = "The router may not delete objects, and holds no wildcard action."
  }
  assert {
    condition = alltrue([
      for s in jsondecode(aws_iam_role_policy.router.policy).Statement : try(s.Condition.ArnEquals["ecs:cluster"], "") == aws_ecs_cluster.this.arn
      if anytrue([for a in flatten([s.Action]) : startswith(a, "ecs:")]) && !contains(flatten([s.Action]), "ecs:TagResource")
    ])
    error_message = "Every ECS statement of the router but the tagging one carries the condition ecs:cluster."
  }
  # TagResource on a task has no ecs:cluster key; the task ARN keeps it to this cluster.
  assert {
    condition = [
      for s in jsondecode(aws_iam_role_policy.router.policy).Statement : [s.Action, s.Resource, s.Condition]
      if contains(flatten([s.Action]), "ecs:TagResource")
    ] == [["ecs:TagResource", "arn:aws:ecs:eu-west-1:111122223333:task/knoggin/*", { StringEquals = { "ecs:CreateAction" = "RunTask" } }]]
    error_message = "The router tags only the tasks it starts, and only in this cluster."
  }
  assert {
    condition     = !contains(flatten([for s in jsondecode(aws_iam_role_policy.router.policy).Statement : s.Action]), "dynamodb:PutItem")
    error_message = "The router has no PutItem."
  }
}

run "lifecycle_permissions" {
  assert {
    condition     = alltrue([for a in flatten([for s in jsondecode(aws_iam_role_policy.lifecycle.policy).Statement : s.Action]) : !startswith(a, "kms:")])
    error_message = "The lifecycle role has no KMS action."
  }
  assert {
    condition = toset(flatten([
      for s in jsondecode(aws_iam_role_policy.lifecycle.policy).Statement : s.Action if s.Effect == "Allow" && s.Resource == aws_dynamodb_table.state.arn
    ])) == toset(["dynamodb:GetItem", "dynamodb:Query", "dynamodb:DeleteItem"])
    error_message = "On the table the lifecycle role has exactly GetItem, Query and DeleteItem."
  }
  assert {
    condition = [
      for s in jsondecode(aws_iam_role_policy.lifecycle.policy).Statement : [s.Action, s.Resource, s.Condition] if s.Effect == "Deny"
    ] == [["dynamodb:DeleteItem", aws_dynamodb_table.state.arn, { "ForAnyValue:StringEquals" = { "dynamodb:LeadingKeys" = ["meta"] } }]]
    error_message = "A deny keeps DeleteItem away from the partition meta."
  }
  assert {
    condition     = jsondecode(aws_ecs_task_definition.lifecycle.container_definitions)[0].command == ["lifecycle"]
    error_message = "The lifecycle task runs the command lifecycle."
  }
  assert {
    condition = (
      aws_scheduler_schedule.lifecycle.schedule_expression == "rate(10 minutes)" &&
      one(one(aws_scheduler_schedule.lifecycle.target).ecs_parameters).task_count == 1 &&
      one(one(one(aws_scheduler_schedule.lifecycle.target).ecs_parameters).network_configuration).security_groups == toset([aws_security_group.lifecycle.id])
    )
    error_message = "One lifecycle task every 10 minutes, in its own security group."
  }
}

run "scheduler_pass_role" {
  assert {
    condition = [
      for s in jsondecode(aws_iam_role_policy.scheduler.policy).Statement : [s.Resource, s.Condition] if contains(flatten([s.Action]), "iam:PassRole")
      ] == [[
        [aws_iam_role.lifecycle.arn, aws_iam_role.lifecycle_execution.arn],
        { StringEquals = { "iam:PassedToService" = "ecs-tasks.amazonaws.com" } },
    ]]
    error_message = "iam:PassRole names the lifecycle task's two roles and one service."
  }
}

run "state_table" {
  assert {
    condition     = one(aws_dynamodb_table.state.ttl).attribute_name == "expires" && one(aws_dynamodb_table.state.ttl).enabled
    error_message = "The table's time-to-live attribute is expires."
  }
  assert {
    condition = (
      aws_dynamodb_table.state.billing_mode == "PAY_PER_REQUEST" &&
      aws_dynamodb_table.state.hash_key == "pk" && aws_dynamodb_table.state.range_key == "sk" &&
      one(aws_dynamodb_table.state.point_in_time_recovery).enabled
    )
    error_message = "On-demand, keys pk and sk, point-in-time recovery on."
  }
}

run "load_balancer" {
  assert {
    condition     = alltrue([for l in aws_lb.public.access_logs : !l.enabled])
    error_message = "The load balancer's access logs stay off."
  }
  assert {
    condition = (
      aws_lb.public.idle_timeout == 300 &&
      one(aws_lb_target_group.router.health_check).path == "/readyz" &&
      aws_lb_target_group.router.port == 8080
    )
    error_message = "Idle timeout 300 s; target port 8080 with the health check GET /readyz."
  }
}

run "hosts" {
  assert {
    condition = alltrue([for line in [
      "ECS_CLUSTER=knoggin",
      "ECS_AWSVPC_BLOCK_IMDS=true",
      "ECS_DISABLE_PRIVILEGED=true",
      "ECS_TASK_PIDS_LIMIT=256",
      "ECS_IMAGE_PULL_BEHAVIOR=prefer-cached",
      "ECS_RESERVED_MEMORY=512",
      "ECS_CONTAINER_STOP_TIMEOUT=120s",
      "kernel.core_pattern=|/bin/false",
    ] : contains(split("\n", base64decode(aws_launch_template.hosts.user_data)), line)])
    error_message = "The user data writes every ecs.config line and the core pattern."
  }
  assert {
    condition = (
      one(aws_launch_template.hosts.metadata_options).http_tokens == "required" &&
      one(aws_launch_template.hosts.metadata_options).http_put_response_hop_limit == 1
    )
    error_message = "The hosts require metadata tokens with a hop limit of 1."
  }
  assert {
    condition     = aws_launch_template.hosts.key_name == null && one(one(aws_launch_template.hosts.block_device_mappings).ebs).encrypted == "true"
    error_message = "No SSH key; an encrypted root volume."
  }
  assert {
    condition = (
      aws_ecs_account_setting_default.trunking.name == "awsvpcTrunking" && aws_ecs_account_setting_default.trunking.value == "enabled" &&
      one(one(aws_ecs_capacity_provider.hosts.auto_scaling_group_provider).managed_scaling).target_capacity == 100 &&
      one(aws_ecs_capacity_provider.hosts.auto_scaling_group_provider).managed_termination_protection == "ENABLED" &&
      one(aws_ecs_capacity_provider.hosts.auto_scaling_group_provider).managed_draining == "ENABLED"
    )
    error_message = "Trunking on; managed scaling at 100, termination protection and draining on."
  }
}

run "host_role" {
  assert {
    condition = length(setintersection(
      flatten([for s in jsondecode(aws_iam_role_policy.hosts.policy).Statement : s.Action]),
      ["ecs:CreateCluster", "ecs:UpdateContainerInstancesState", "ecs:DeregisterContainerInstance"],
    )) == 0
    error_message = "The host role may not create clusters, drain or deregister hosts."
  }
  assert {
    condition     = alltrue([for a in flatten([for s in jsondecode(aws_iam_role_policy.hosts.policy).Statement : s.Action]) : !strcontains(a, "*")])
    error_message = "The host role holds no wildcard action."
  }
  # Every ECS resource is this cluster or one of its container instances; the one action
  # that supports no resource is the only one on "*".
  assert {
    condition = alltrue(flatten([
      for s in jsondecode(aws_iam_role_policy.hosts.policy).Statement : [
        for r in flatten([s.Resource]) : (
          r == aws_ecs_cluster.this.arn ||
          r == "arn:aws:ecs:eu-west-1:111122223333:container-instance/knoggin/*" ||
          (r == "*" && flatten([s.Action]) == ["ecs:DiscoverPollEndpoint"])
        )
      ] if anytrue([for a in flatten([s.Action]) : startswith(a, "ecs:")])
    ]))
    error_message = "The host role names only this cluster."
  }
  assert {
    condition = [
      for s in jsondecode(aws_iam_role_policy.hosts.policy).Statement : s.Condition if contains(flatten([s.Action]), "ecs:TagResource")
    ] == [{ StringEquals = { "ecs:CreateAction" = "RegisterContainerInstance" } }]
    error_message = "ecs:TagResource only under ecs:CreateAction = RegisterContainerInstance."
  }
  assert {
    condition = [
      for s in jsondecode(aws_iam_role_policy.hosts.policy).Statement : s.Resource if anytrue([for a in flatten([s.Action]) : startswith(a, "logs:")])
    ] == ["${aws_cloudwatch_log_group.this["user"].arn}:*"]
    error_message = "The host role writes the user log group only."
  }
  assert {
    condition = [
      for s in jsondecode(aws_iam_role_policy.hosts.policy).Statement : s.Resource if contains(flatten([s.Action]), "ecr:BatchGetImage")
    ] == ["arn:aws:ecr:eu-west-1:111122223333:repository/knoggin"]
    error_message = "The hosts pull only from the deployment's repository."
  }
}

run "router_config" {
  assert {
    condition = (
      aws_ecs_task_definition.router.requires_compatibilities == toset(["FARGATE"]) &&
      aws_ecs_task_definition.router.cpu == "512" && aws_ecs_task_definition.router.memory == "1024" &&
      aws_ecs_service.router.desired_count == 2 && aws_ecs_service.router.launch_type == "FARGATE"
    )
    error_message = "Two routers on Fargate, 0.5 vCPU and 1 GB each."
  }
  assert {
    condition = { for e in jsondecode(aws_ecs_task_definition.router.container_definitions)[0].environment : e.name => e.value if contains([
      "KNOGGIN_SERVER__TRUSTED_PROXIES", "KNOGGIN_SERVER__IMAGE_DIGEST", "KNOGGIN_CONTAINERS__MEMORY_MIB",
      "KNOGGIN_CONTAINERS__MAX", "KNOGGIN_LAUNCHER__ECS__TASK_DEFINITION", "KNOGGIN_INTERNAL__ROUTER_URL",
      ], e.name) } == {
      KNOGGIN_SERVER__TRUSTED_PROXIES        = "10.0.240.0/24,10.0.241.0/24"
      KNOGGIN_SERVER__IMAGE_DIGEST           = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
      KNOGGIN_CONTAINERS__MEMORY_MIB         = "1024"
      KNOGGIN_CONTAINERS__MAX                = "60"
      KNOGGIN_LAUNCHER__ECS__TASK_DEFINITION = aws_ecs_task_definition.user.arn
      KNOGGIN_INTERNAL__ROUTER_URL           = "https://router.knoggin.internal:8443"
    }
    error_message = "The router's config: the load balancer's subnets, the digest, one memory value for both, 4 hosts of 15."
  }
  assert {
    condition = toset([for e in jsondecode(aws_ecs_task_definition.router.container_definitions)[0].secrets : e.name]) == toset([
      "KNOGGIN_AUTH__WEB_CLIENT_SECRET", "KNOGGIN_INTERNAL__SECRET", "KNOGGIN_INTERNAL__CERT", "KNOGGIN_INTERNAL__KEY",
    ])
    error_message = "The four secrets come from Secrets Manager."
  }
}

run "sizing_interface_cap" {
  variables {
    instance_type        = "m8g.2xlarge"
    container_memory_mib = 512
  }
  # 32 GiB less 2 GiB fits 60 at 512 MiB, but the interface cap is 40.
  assert {
    condition     = [for e in jsondecode(aws_ecs_task_definition.router.container_definitions)[0].environment : e.value if e.name == "KNOGGIN_CONTAINERS__MAX"] == ["160"]
    error_message = "containers.max is max_hosts times the smaller of memory fit and interface cap."
  }
  assert {
    condition     = jsondecode(aws_ecs_task_definition.user.container_definitions)[0].memory == 512
    error_message = "One variable sets the task's memory and containers.memory_mib."
  }
}

run "sizing_unknown_type" {
  command = plan
  variables {
    instance_type = "c8g.xlarge"
  }
  expect_failures = [aws_ecs_task_definition.router]
}

run "sizing_unknown_type_with_count" {
  variables {
    instance_type       = "c8g.xlarge"
    containers_per_host = 6
    max_hosts           = 3
  }
  assert {
    condition     = [for e in jsondecode(aws_ecs_task_definition.router.container_definitions)[0].environment : e.value if e.name == "KNOGGIN_CONTAINERS__MAX"] == ["18"]
    error_message = "containers_per_host is used for a type outside the table."
  }
}

run "no_admin" {
  command = plan
  variables {
    admin_subjects = []
  }
  expect_failures = [aws_ecs_task_definition.router]
}

run "image_elsewhere" {
  command = plan
  variables {
    image = "444455556666.dkr.ecr.eu-west-1.amazonaws.com/knoggin@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
  }
  expect_failures = [aws_ecs_task_definition.user]
}
