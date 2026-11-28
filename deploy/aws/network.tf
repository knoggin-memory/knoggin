# SPDX-License-Identifier: AGPL-3.0-only
# AWS: the network and its rules. The module always makes its own VPC,
# because these rules would change any VPC they were put on for everything else in it.

locals {
  vpc_cidr = "10.0.0.0/16"
  # /20 per zone: one address per user container, far above the /22 needed.
  private_cidrs = [for i in range(2) : cidrsubnet(local.vpc_cidr, 4, i)]
  public_cidrs  = [for i in range(2) : cidrsubnet(local.vpc_cidr, 8, 240 + i)]
}

resource "aws_vpc" "this" {
  cidr_block           = local.vpc_cidr
  enable_dns_support   = true
  enable_dns_hostnames = true
  tags                 = { Name = var.name }

  lifecycle {
    precondition {
      condition     = data.aws_region.current.region == var.region
      error_message = "region must be the AWS provider's region."
    }
  }
}

# Every rule of the default group is removed.
resource "aws_default_security_group" "this" {
  vpc_id = aws_vpc.this.id
}

resource "aws_subnet" "private" {
  count             = 2
  vpc_id            = aws_vpc.this.id
  availability_zone = local.zones[count.index]
  cidr_block        = local.private_cidrs[count.index]
  tags              = { Name = "${var.name}-private-${local.zones[count.index]}" }
}

# For the load balancer and the NAT gateway only.
resource "aws_subnet" "public" {
  count             = 2
  vpc_id            = aws_vpc.this.id
  availability_zone = local.zones[count.index]
  cidr_block        = local.public_cidrs[count.index]
  tags              = { Name = "${var.name}-public-${local.zones[count.index]}" }
}

resource "aws_internet_gateway" "this" {
  vpc_id = aws_vpc.this.id
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.this.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.this.id
  }
}

resource "aws_route_table_association" "public" {
  count          = 2
  subnet_id      = aws_subnet.public[count.index].id
  route_table_id = aws_route_table.public.id
}

# Host egress. User containers cannot use it: their security group allows no such traffic.
resource "aws_eip" "nat" {
  domain = "vpc"
}

resource "aws_nat_gateway" "this" {
  allocation_id = aws_eip.nat.id
  subnet_id     = aws_subnet.public[0].id
  depends_on    = [aws_internet_gateway.this]
}

resource "aws_route_table" "private" {
  vpc_id = aws_vpc.this.id

  route {
    cidr_block     = "0.0.0.0/0"
    nat_gateway_id = aws_nat_gateway.this.id
  }
}

resource "aws_route_table_association" "private" {
  count          = 2
  subnet_id      = aws_subnet.private[count.index].id
  route_table_id = aws_route_table.private.id
}

# The S3 prefix list is what user containers may reach. The policy lets through only the
# deployment's bucket, for requests signed by the router's role (a container's signed
# links carry the router's) to list, read and upload, or by the lifecycle task's role to
# list and delete, and reads of the registry's layer bucket, which the hosts and the
# Fargate tasks pull image layers from.
resource "aws_vpc_endpoint" "s3" {
  vpc_id            = aws_vpc.this.id
  service_name      = "com.amazonaws.${var.region}.s3"
  vpc_endpoint_type = "Gateway"
  route_table_ids   = [aws_route_table.private.id]

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "RouterBucket"
        Effect    = "Allow"
        Principal = "*"
        Action    = ["s3:ListBucket", "s3:GetObject", "s3:PutObject"]
        Resource  = local.bucket_and_objects
        Condition = { ArnEquals = { "aws:PrincipalArn" = aws_iam_role.router.arn } }
      },
      {
        Sid       = "LifecycleBucket"
        Effect    = "Allow"
        Principal = "*"
        Action    = ["s3:ListBucket", "s3:DeleteObject"]
        Resource  = local.bucket_and_objects
        Condition = { ArnEquals = { "aws:PrincipalArn" = aws_iam_role.lifecycle.arn } }
      },
      {
        Sid       = "RegistryLayers"
        Effect    = "Allow"
        Principal = "*"
        Action    = "s3:GetObject"
        Resource  = "arn:aws:s3:::prod-${var.region}-starport-layer-bucket/*"
      },
    ]
  })
}

resource "aws_vpc_endpoint" "dynamodb" {
  vpc_id            = aws_vpc.this.id
  service_name      = "com.amazonaws.${var.region}.dynamodb"
  vpc_endpoint_type = "Gateway"
  route_table_ids   = [aws_route_table.private.id]
}

resource "aws_security_group" "alb" {
  name        = "${var.name}-alb"
  description = "Knoggin load balancer"
  vpc_id      = aws_vpc.this.id
}

resource "aws_security_group" "router" {
  name        = "${var.name}-router"
  description = "Knoggin routers"
  vpc_id      = aws_vpc.this.id
}

resource "aws_security_group" "user" {
  name        = "${var.name}-user"
  description = "Knoggin user containers: no rule names this group itself"
  vpc_id      = aws_vpc.this.id
}

resource "aws_security_group" "hosts" {
  count       = local.ec2 ? 1 : 0
  name        = "${var.name}-hosts"
  description = "Knoggin user hosts"
  vpc_id      = aws_vpc.this.id
}

resource "aws_security_group" "lifecycle" {
  name        = "${var.name}-lifecycle"
  description = "Knoggin lifecycle task"
  vpc_id      = aws_vpc.this.id
}

# Every rule of every group, in two maps, so that a test can see all of them at once.
# The groups above hold no inline rules. A rule with `only` exists on that user_compute only.
locals {
  ingress = { for k, r in {
    alb_https           = { group = aws_security_group.alb.id, port = 443, cidr = "0.0.0.0/0" }
    router_alb          = { group = aws_security_group.router.id, port = 8080, from = aws_security_group.alb.id }
    router_user         = { group = aws_security_group.router.id, port = 8443, from = aws_security_group.user.id }
    user_router         = { group = aws_security_group.user.id, port = 8443, from = aws_security_group.router.id }
    endpoints_user      = { group = one(aws_security_group.endpoints[*].id), port = 443, from = aws_security_group.user.id, only = "fargate" }
    endpoints_router    = { group = one(aws_security_group.endpoints[*].id), port = 443, from = aws_security_group.router.id, only = "fargate" }
    endpoints_lifecycle = { group = one(aws_security_group.endpoints[*].id), port = 443, from = aws_security_group.lifecycle.id, only = "fargate" }
  } : k => r if lookup(r, "only", var.user_compute) == var.user_compute }
  egress = { for k, r in {
    alb_router     = { group = aws_security_group.alb.id, port = 8080, to = aws_security_group.router.id }
    router_user    = { group = aws_security_group.router.id, port = 8443, to = aws_security_group.user.id }
    router_https   = { group = aws_security_group.router.id, port = 443, cidr = "0.0.0.0/0" }
    user_s3        = { group = aws_security_group.user.id, port = 443, prefix_list = aws_vpc_endpoint.s3.prefix_list_id }
    user_router    = { group = aws_security_group.user.id, port = 8443, to = aws_security_group.router.id }
    user_endpoints = { group = aws_security_group.user.id, port = 443, to = one(aws_security_group.endpoints[*].id), only = "fargate" }
    hosts_https    = { group = one(aws_security_group.hosts[*].id), port = 443, cidr = "0.0.0.0/0", only = "ec2" }
    lifecycle_out  = { group = aws_security_group.lifecycle.id, port = 443, cidr = "0.0.0.0/0" }
  } : k => r if lookup(r, "only", var.user_compute) == var.user_compute }
}

resource "aws_vpc_security_group_ingress_rule" "this" {
  for_each                     = local.ingress
  security_group_id            = each.value.group
  ip_protocol                  = "tcp"
  from_port                    = each.value.port
  to_port                      = each.value.port
  cidr_ipv4                    = lookup(each.value, "cidr", null)
  referenced_security_group_id = lookup(each.value, "from", null)
}

resource "aws_vpc_security_group_egress_rule" "this" {
  for_each                     = local.egress
  security_group_id            = each.value.group
  ip_protocol                  = "tcp"
  from_port                    = each.value.port
  to_port                      = each.value.port
  cidr_ipv4                    = lookup(each.value, "cidr", null)
  prefix_list_id               = lookup(each.value, "prefix_list", null)
  referenced_security_group_id = lookup(each.value, "to", null)
}
