# SPDX-License-Identifier: AGPL-3.0-only
# AWS: the bucket, the state database and the master key.

# Snapshots.
resource "aws_s3_bucket" "snapshots" {
  bucket_prefix = "${var.name}-snapshots-"
}

# Versioning is off unless undelete_days is set: Knoggin's own 24-hour window is the undo,
# and a kept old version would keep a forgotten memory readable for longer.
resource "aws_s3_bucket_versioning" "snapshots" {
  bucket = aws_s3_bucket.snapshots.id

  versioning_configuration {
    status = var.undelete_days > 0 ? "Enabled" : "Disabled"
  }
}

# With undelete_days, a deleted snapshot stays as an old version that many days. No rule
# here ever expires a current object by age: only the lifecycle task deletes snapshots.
resource "aws_s3_bucket_lifecycle_configuration" "snapshots" {
  count  = var.undelete_days > 0 ? 1 : 0
  bucket = aws_s3_bucket.snapshots.id

  rule {
    id     = "noncurrent"
    status = "Enabled"
    filter {}

    noncurrent_version_expiration {
      # The variable refuses fractions; floor keeps a refused value from reaching the
      # provider's whole-number field during the same plan.
      noncurrent_days = floor(var.undelete_days)
    }

    abort_incomplete_multipart_upload {
      days_after_initiation = 1
    }
  }

  rule {
    id     = "markers"
    status = "Enabled"
    filter {}

    expiration {
      expired_object_delete_marker = true
    }
  }

  depends_on = [aws_s3_bucket_versioning.snapshots]
}

# Denies only, so that no change to the roles' IAM policies can let the router destroy a
# snapshot, or the lifecycle task reach old versions, or either change the bucket's rules.
resource "aws_s3_bucket_policy" "snapshots" {
  bucket = aws_s3_bucket.snapshots.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Sid       = "DenyRouterDestroy"
        Effect    = "Deny"
        Principal = "*"
        Action = [
          "s3:DeleteObject",
          "s3:DeleteObjectVersion",
          "s3:PutObjectRetention",
          "s3:PutObjectLegalHold",
          "s3:BypassGovernanceRetention",
          "s3:PutObjectAcl",
          "s3:PutObjectVersionAcl",
          "s3:PutObjectTagging",
          "s3:PutObjectVersionTagging",
          "s3:GetObjectVersion",
        ]
        Resource  = local.bucket_and_objects
        Condition = { ArnEquals = { "aws:PrincipalArn" = aws_iam_role.router.arn } }
      },
      {
        Sid       = "DenyRouterUnconditionalPut"
        Effect    = "Deny"
        Principal = "*"
        Action    = "s3:PutObject"
        Resource  = local.bucket_and_objects
        Condition = {
          ArnEquals = { "aws:PrincipalArn" = aws_iam_role.router.arn }
          Null      = { "s3:if-none-match" = "true" }
        }
      },
      {
        Sid       = "DenyLifecyclePurge"
        Effect    = "Deny"
        Principal = "*"
        Action = [
          "s3:DeleteObjectVersion",
          "s3:BypassGovernanceRetention",
          "s3:PutObject",
          "s3:GetObject",
          "s3:GetObjectVersion",
          "s3:PutObjectVersionTagging",
        ]
        Resource  = local.bucket_and_objects
        Condition = { ArnEquals = { "aws:PrincipalArn" = aws_iam_role.lifecycle.arn } }
      },
      {
        Sid       = "DenyAppBucketConfig"
        Effect    = "Deny"
        Principal = "*"
        Action = [
          "s3:PutBucketVersioning",
          "s3:PutLifecycleConfiguration",
          "s3:PutBucketPolicy",
          "s3:DeleteBucketPolicy",
          "s3:PutBucketObjectLockConfiguration",
          "s3:PutBucketAcl",
          "s3:PutBucketOwnershipControls",
          "s3:PutReplicationConfiguration",
          "s3:PutBucketPublicAccessBlock",
          "s3:PutEncryptionConfiguration",
          "s3:PutBucketNotification",
          "s3:PutIntelligentTieringConfiguration",
          "s3:DeleteBucket",
        ]
        Resource  = local.bucket_and_objects
        Condition = { ArnEquals = { "aws:PrincipalArn" = [aws_iam_role.router.arn, aws_iam_role.lifecycle.arn] } }
      },
    ]
  })

  depends_on = [aws_s3_bucket_public_access_block.snapshots]
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

# The state database on DynamoDB. Restoring it from a backup brings
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
  bucket_and_objects = [aws_s3_bucket.snapshots.arn, "${aws_s3_bucket.snapshots.arn}/*"]

  # ForAnyValue, not ForAllValues: ForAllValues passes a request with no context at all.
  kms_store_context = {
    "ForAnyValue:StringEquals" = { "kms:EncryptionContextKeys" = ["knoggin:store"] }
  }
}
