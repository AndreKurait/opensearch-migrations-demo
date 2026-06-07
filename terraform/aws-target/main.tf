# =============================================================================
# aws-target — a managed Amazon OpenSearch Service domain as the migration
# target.
#
# Emitted (in equivalent form) by the demo TUI's cloud path. NOT applied by the
# harness — review and `terraform apply` yourself.
# =============================================================================

terraform {
  required_version = ">= 1.5.0"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

provider "aws" {
  region = var.region
}

variable "region" {
  type    = string
  default = "us-east-1"
}

variable "prefix" {
  type    = string
  default = "ma-demo"
}

variable "target_version" {
  type        = string
  default     = "3.3"
  description = "OpenSearch Service engine version (major.minor), e.g. 3.3, 3.1, 2.19."
}

variable "instance_type" {
  type    = string
  default = "r6g.large.search"
}

variable "instance_count" {
  type    = number
  default = 2
}

variable "volume_size_gb" {
  type    = number
  default = 50
}

resource "aws_opensearch_domain" "target" {
  domain_name    = "${var.prefix}-target"
  engine_version = "OpenSearch_${var.target_version}"

  cluster_config {
    instance_type  = var.instance_type
    instance_count = var.instance_count
  }

  ebs_options {
    ebs_enabled = true
    volume_size = var.volume_size_gb
  }
}

output "target_endpoint" {
  value = aws_opensearch_domain.target.endpoint
}

output "target_arn" {
  value = aws_opensearch_domain.target.arn
}
