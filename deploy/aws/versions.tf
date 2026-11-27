# SPDX-License-Identifier: AGPL-3.0-only
# AWS: one module, one region. It has not yet been applied to a real account: it is
# checked with `tofu validate` and `tofu test` against mock providers.

terraform {
  required_version = "~> 1.13.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "6.73.0"
    }
  }
}

data "aws_caller_identity" "current" {}

data "aws_region" "current" {}

data "aws_availability_zones" "available" {
  state = "available"
}

locals {
  account = data.aws_caller_identity.current.account_id
  zones   = slice(data.aws_availability_zones.available.names, 0, 2)
}
