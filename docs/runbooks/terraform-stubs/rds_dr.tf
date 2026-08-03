# Terraform stub — RDS Multi-AZ with PITR for Anseo DR.
#
# Story 26.3 reference artifact. REVIEWED BUT NOT APPLIED until a maintainer
# game-day sign-off. Do not `terraform apply` this without:
#   1. Reviewing VPC/subnet group names for the target AWS account.
#   2. Rotating the master password into AWS Secrets Manager.
#   3. Confirming backup_retention_period with the data-retention policy.
#
# Prerequisites: AWS credentials, existing VPC + private subnet group,
# KMS key for storage encryption.

terraform {
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

variable "vpc_id" {
  type        = string
  description = "VPC in which to place the RDS instance."
}

variable "db_subnet_group_name" {
  type        = string
  description = "Name of the existing DB subnet group (private subnets)."
}

variable "kms_key_id" {
  type        = string
  description = "ARN of the KMS key for RDS storage encryption."
}

variable "master_password" {
  type        = string
  sensitive   = true
  description = "Master password — store in AWS Secrets Manager, not in VCS."
}

resource "aws_db_instance" "anseo_primary" {
  identifier        = "anseo-primary"
  engine            = "postgres"
  engine_version    = "16.2"
  instance_class    = "db.r7g.large"
  allocated_storage = 100
  storage_type      = "gp3"
  storage_encrypted = true
  kms_key_id        = var.kms_key_id

  db_name  = "anseo"
  username = "anseo"
  password = var.master_password

  # Multi-AZ: standby replica in a different AZ for automatic failover.
  multi_az = true

  # PITR: 7-day window satisfies RPO ≤ 5 min for all writes in that window.
  backup_retention_period = 7
  backup_window           = "03:00-04:00"
  maintenance_window      = "Mon:04:00-Mon:05:00"

  db_subnet_group_name   = var.db_subnet_group_name
  vpc_security_group_ids = [aws_security_group.rds.id]

  deletion_protection = true
  skip_final_snapshot = false
  final_snapshot_identifier = "anseo-primary-final"

  tags = {
    Project     = "anseo"
    Environment = "production"
    ManagedBy   = "terraform"
  }
}

# Cross-region snapshot replication — RPO guard for region-level failure.
# Uncomment and configure after the primary instance is stable.
#
# resource "aws_db_snapshot" "anseo_cross_region" {
#   db_instance_identifier = aws_db_instance.anseo_primary.id
#   db_snapshot_identifier = "anseo-cross-region-${formatdate("YYYYMMDD", timestamp())}"
# }

resource "aws_security_group" "rds" {
  name        = "anseo-rds"
  description = "Allow Postgres access from the Anseo API security group."
  vpc_id      = var.vpc_id

  ingress {
    from_port       = 5432
    to_port         = 5432
    protocol        = "tcp"
    security_groups = [] # TODO: replace with the API task security group ID
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = {
    Project   = "anseo"
    ManagedBy = "terraform"
  }
}

output "rds_endpoint" {
  value       = aws_db_instance.anseo_primary.endpoint
  description = "RDS writer endpoint — use this in ANSEO_DB_HOST."
}
