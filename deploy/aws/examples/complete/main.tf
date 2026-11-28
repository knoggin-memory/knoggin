# SPDX-License-Identifier: AGPL-3.0-only
# A minimal root module for the Knoggin AWS module. Fill in terraform.tfvars from
# terraform.tfvars.example. The Secrets Manager entries are made and filled by hand first
# (deploy-aws.md), so that no secret value sits in the state.

terraform {
  required_version = "~> 1.13.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "6.73.0"
    }
  }
}

provider "aws" {
  region = var.region

  default_tags {
    tags = { Application = "knoggin" }
  }
}

module "knoggin" {
  source = "../.."

  name                       = var.name
  region                     = var.region
  public_url                 = var.public_url
  certificate_arn            = var.certificate_arn
  image                      = var.image
  auth_issuer                = var.auth_issuer
  auth_web_client_id         = var.auth_web_client_id
  auth_web_client_secret_arn = var.auth_web_client_secret_arn
  internal_secret_arn        = var.internal_secret_arn
  internal_cert_arn          = var.internal_cert_arn
  internal_key_arn           = var.internal_key_arn
  admin_subjects             = var.admin_subjects
  user_compute               = var.user_compute
  max_containers             = var.max_containers
  container_memory_mib       = var.container_memory_mib
  undelete_days              = var.undelete_days
}

variable "name" {
  type    = string
  default = "knoggin"
}

variable "region" {
  type = string
}

variable "public_url" {
  type = string
}

variable "certificate_arn" {
  type = string
}

variable "image" {
  type = string
}

variable "auth_issuer" {
  type = string
}

variable "auth_web_client_id" {
  type = string
}

variable "auth_web_client_secret_arn" {
  type = string
}

variable "internal_secret_arn" {
  type = string
}

variable "internal_cert_arn" {
  type = string
}

variable "internal_key_arn" {
  type = string
}

variable "admin_subjects" {
  type = list(string)
}

variable "user_compute" {
  type    = string
  default = "ec2"
}

variable "max_containers" {
  type    = number
  default = null
}

variable "container_memory_mib" {
  type    = number
  default = 1536
}

variable "undelete_days" {
  type    = number
  default = 0
}

output "load_balancer_dns_name" {
  value = module.knoggin.load_balancer_dns_name
}

output "bucket_name" {
  value = module.knoggin.bucket_name
}

output "table_name" {
  value = module.knoggin.table_name
}

output "key_arn" {
  value = module.knoggin.key_arn
}

output "cluster_name" {
  value = module.knoggin.cluster_name
}

output "log_group_names" {
  value = module.knoggin.log_group_names
}
