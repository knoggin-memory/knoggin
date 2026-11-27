# SPDX-License-Identifier: AGPL-3.0-only
# Tab "Deployment", AWS: the bucket, the state database and the master key.

# Snapshots. Versioning stays off: a kept old version would keep an erased store readable.
resource "aws_s3_bucket" "snapshots" {
  bucket_prefix = "${var.name}-snapshots-"
}

resource "aws_s3_bucket_versioning" "snapshots" {
  bucket = aws_s3_bucket.snapshots.id

  versioning_configuration {
    status = "Disabled"
  }
}

resource "aws_s3_bucket_public_access_block" "snapshots" {
  bucket                  = aws_s3_bucket.snapshots.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "snapshots" {
  bucket = aws_s3_bucket.snapshots.id

  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

# The state database (tab "State database", DynamoDB). Restoring it from a backup brings
# back revoked keys, ended sessions and removed users; docs/operations.md says what to do.
resource "aws_dynamodb_table" "state" {
  name         = "${var.name}-state"
  billing_mode = "PAY_PER_REQUEST"
  hash_key     = "pk"
  range_key    = "sk"

  attribute {
    name = "pk"
    type = "S"
  }

  attribute {
    name = "sk"
    type = "S"
  }

  ttl {
    attribute_name = "expires"
    enabled        = true
  }

  point_in_time_recovery {
    enabled = true
  }
}

# The master key. Its policy lets the account (that is, IAM) manage the key but not use
# it: only the router's task role may wrap and unwrap, and only with the encryption
# context key knoggin:store. A principal that may change this policy can still give
# itself use of the key, and counts as an operator.
resource "aws_kms_key" "master" {
  description             = "Knoggin ${var.name} master key"
  enable_key_rotation     = true
  rotation_period_in_days = 365
  deletion_window_in_days = 30

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "AccountManagesKey"
        Effect    = "Allow"
        Principal = { AWS = "arn:aws:iam::${local.account}:root" }
        Action = [
          "kms:CancelKeyDeletion",
          "kms:DescribeKey",
          "kms:DisableKey",
          "kms:DisableKeyRotation",
          "kms:EnableKey",
          "kms:EnableKeyRotation",
          "kms:GetKeyPolicy",
          "kms:GetKeyRotationStatus",
          "kms:ListGrants",
          "kms:ListKeyPolicies",
          "kms:ListKeyRotations",
          "kms:ListResourceTags",
          "kms:PutKeyPolicy",
          "kms:RevokeGrant",
          "kms:RotateKeyOnDemand",
          "kms:ScheduleKeyDeletion",
          "kms:TagResource",
          "kms:UntagResource",
          "kms:UpdateKeyDescription",
        ]
        Resource = "*"
      },
      {
        Sid       = "RouterUsesKey"
        Effect    = "Allow"
        Principal = { AWS = aws_iam_role.router.arn }
        Action    = ["kms:GenerateDataKey", "kms:Decrypt"]
        Resource  = "*"
        Condition = local.kms_store_context
      },
    ]
  })
}

locals {
  # ForAnyValue, not ForAllValues: ForAllValues passes a request with no context at all.
  kms_store_context = {
    "ForAnyValue:StringEquals" = { "kms:EncryptionContextKeys" = ["knoggin:store"] }
  }
}
