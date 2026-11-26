# Tab "Deployment", AWS: the routers' internal name, and the DNS firewall. The VPC's
# resolver answers every network interface whatever its security group says, and a name
# lookup can carry data out, so every lookup outside the allow list is blocked. That holds
# for everything in this VPC, not only for user containers.

resource "aws_service_discovery_private_dns_namespace" "internal" {
  name = "${var.name}.internal"
  vpc  = aws_vpc.this.id
}

# Containers reach the broker at https://router.<name>.internal:8443.
resource "aws_service_discovery_service" "router" {
  name = "router"

  dns_config {
    namespace_id   = aws_service_discovery_private_dns_namespace.internal.id
    routing_policy = "MULTIVALUE"

    dns_records {
      type = "A"
      ttl  = 10
    }
  }

  health_check_custom_config {}
}

locals {
  issuer_host = regex("^https://([^/:]+)", var.auth_issuer)[0]
}

resource "aws_route53_resolver_firewall_domain_list" "allow" {
  name = "${var.name}-allow"
  domains = distinct(concat(
    ["amazonaws.com", "*.amazonaws.com"],
    ["${var.name}.internal", "*.${var.name}.internal"],
    [local.issuer_host],
    var.dns_allow,
  ))
}

resource "aws_route53_resolver_firewall_domain_list" "everything" {
  name    = "${var.name}-everything"
  domains = ["*"]
}

resource "aws_route53_resolver_firewall_rule_group" "this" {
  name = var.name
}

# Every rule of the group in one map, so that a test sees them all: the allow list first,
# and last a rule that blocks every other query.
locals {
  firewall_rules = {
    allow = { priority = 100, action = "ALLOW", list = aws_route53_resolver_firewall_domain_list.allow.id }
    block = { priority = 200, action = "BLOCK", list = aws_route53_resolver_firewall_domain_list.everything.id }
  }
}

resource "aws_route53_resolver_firewall_rule" "this" {
  for_each                = local.firewall_rules
  name                    = each.key
  firewall_rule_group_id  = aws_route53_resolver_firewall_rule_group.this.id
  firewall_domain_list_id = each.value.list
  priority                = each.value.priority
  action                  = each.value.action
  block_response          = each.value.action == "BLOCK" ? "NXDOMAIN" : null
}

resource "aws_route53_resolver_firewall_rule_group_association" "this" {
  name                   = var.name
  firewall_rule_group_id = aws_route53_resolver_firewall_rule_group.this.id
  vpc_id                 = aws_vpc.this.id
  priority               = 101
  mutation_protection    = "ENABLED"
}
