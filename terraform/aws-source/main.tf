# =============================================================================
# aws-source — a source search cluster (Elasticsearch / OpenSearch / Solr) on a
# single EC2 instance, plus the S3 snapshot bucket the Migration Assistant
# backfills from.
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

variable "source_engine" {
  type        = string
  default     = "elasticsearch"
  description = "elasticsearch | opensearch | solr"
  validation {
    condition     = contains(["elasticsearch", "opensearch", "solr"], var.source_engine)
    error_message = "source_engine must be elasticsearch, opensearch, or solr."
  }
}

variable "source_version" {
  type    = string
  default = "7.10.2"
}

variable "instance_type" {
  type    = string
  default = "t3.large"
}

variable "allowed_cidr" {
  type        = string
  default     = "10.0.0.0/8"
  description = "CIDR allowed to reach the search port (tighten for non-demo use)."
}

locals {
  # Resolve the container image + run args for the chosen engine.
  image = {
    elasticsearch = "docker.elastic.co/elasticsearch/elasticsearch:${var.source_version}"
    opensearch    = "opensearchproject/opensearch:${var.source_version}"
    solr          = "solr:${var.source_version}"
  }[var.source_engine]

  port = var.source_engine == "solr" ? 8983 : 9200

  run_args = {
    elasticsearch = "-p 9200:9200 -e discovery.type=single-node -e xpack.security.enabled=false"
    opensearch    = "-p 9200:9200 -e discovery.type=single-node -e DISABLE_SECURITY_PLUGIN=true"
    solr          = "-p 8983:8983"
  }[var.source_engine]
}

data "aws_ami" "al2023" {
  most_recent = true
  owners      = ["amazon"]
  filter {
    name   = "name"
    values = ["al2023-ami-*-x86_64"]
  }
}

resource "aws_security_group" "source" {
  name_prefix = "${var.prefix}-source-"
  ingress {
    from_port   = local.port
    to_port     = local.port
    protocol    = "tcp"
    cidr_blocks = [var.allowed_cidr]
  }
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

resource "aws_instance" "source" {
  ami                    = data.aws_ami.al2023.id
  instance_type          = var.instance_type
  vpc_security_group_ids = [aws_security_group.source.id]
  user_data              = <<-EOF
    #!/bin/bash
    dnf install -y docker
    systemctl enable --now docker
    sysctl -w vm.max_map_count=262144
    docker run -d --restart=always ${local.run_args} ${local.image}
  EOF
  tags                   = { Name = "${var.prefix}-source" }
}

resource "aws_s3_bucket" "snapshots" {
  bucket_prefix = "${var.prefix}-snapshots-"
  force_destroy = true
}

resource "aws_s3_bucket_versioning" "snapshots" {
  bucket = aws_s3_bucket.snapshots.id
  versioning_configuration {
    status = "Enabled"
  }
}

output "source_endpoint" {
  value = "http://${aws_instance.source.private_ip}:${local.port}"
}

output "snapshot_bucket" {
  value = aws_s3_bucket.snapshots.bucket
}
