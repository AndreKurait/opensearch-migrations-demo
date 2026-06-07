# =============================================================================
# aws-source — a PRIVATE source search cluster (Elasticsearch / OpenSearch /
# Solr) on a single EC2 instance, plus the S3 snapshot bucket the Migration
# Assistant backfills from.
#
# Private by design — a public EC2 is structurally impossible here:
#   * dedicated VPC with a PRIVATE subnet (no route to an internet gateway)
#   * NAT gateway for outbound only (so the instance can pull its container)
#   * associate_public_ip_address = false on the instance
#   * security-group ingress restricted to the VPC CIDR — never 0.0.0.0/0
#   * SSM (via VPC endpoints) for shell access without any public exposure
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

variable "vpc_cidr" {
  type    = string
  default = "10.20.0.0/16"
}

locals {
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

# ---- network: a dedicated VPC with a PRIVATE subnet + NAT egress ----

resource "aws_vpc" "this" {
  cidr_block           = var.vpc_cidr
  enable_dns_support   = true
  enable_dns_hostnames = true
  tags                 = { Name = "${var.prefix}-vpc" }
}

resource "aws_internet_gateway" "this" {
  vpc_id = aws_vpc.this.id
}

# A public subnet exists ONLY to host the NAT gateway — the instance never
# lives here.
resource "aws_subnet" "public" {
  vpc_id            = aws_vpc.this.id
  cidr_block        = cidrsubnet(var.vpc_cidr, 8, 0)
  availability_zone = data.aws_availability_zones.available.names[0]
}

resource "aws_subnet" "private" {
  vpc_id            = aws_vpc.this.id
  cidr_block        = cidrsubnet(var.vpc_cidr, 8, 1)
  availability_zone = data.aws_availability_zones.available.names[0]
  # Belt-and-suspenders: never auto-assign a public IP in this subnet.
  map_public_ip_on_launch = false
}

data "aws_availability_zones" "available" {
  state = "available"
}

resource "aws_eip" "nat" {
  domain = "vpc"
}

resource "aws_nat_gateway" "this" {
  allocation_id = aws_eip.nat.id
  subnet_id     = aws_subnet.public.id
}

resource "aws_route_table" "public" {
  vpc_id = aws_vpc.this.id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.this.id
  }
}

resource "aws_route_table_association" "public" {
  subnet_id      = aws_subnet.public.id
  route_table_id = aws_route_table.public.id
}

resource "aws_route_table" "private" {
  vpc_id = aws_vpc.this.id
  route {
    cidr_block     = "0.0.0.0/0"
    nat_gateway_id = aws_nat_gateway.this.id
  }
}

resource "aws_route_table_association" "private" {
  subnet_id      = aws_subnet.private.id
  route_table_id = aws_route_table.private.id
}

# ---- security group: VPC-CIDR ingress only, never 0.0.0.0/0 ----

resource "aws_security_group" "source" {
  name_prefix = "${var.prefix}-source-"
  vpc_id      = aws_vpc.this.id

  ingress {
    description = "Search API, reachable only from inside the VPC."
    from_port   = local.port
    to_port     = local.port
    protocol    = "tcp"
    cidr_blocks = [var.vpc_cidr]
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

# ---- SSM access (private — no SSH, no public IP) ----

resource "aws_iam_role" "ssm" {
  name_prefix = "${var.prefix}-ssm-"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ec2.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
}

resource "aws_iam_role_policy_attachment" "ssm" {
  role       = aws_iam_role.ssm.name
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_instance_profile" "ssm" {
  name_prefix = "${var.prefix}-ssm-"
  role        = aws_iam_role.ssm.name
}

data "aws_ami" "al2023" {
  most_recent = true
  owners      = ["amazon"]
  filter {
    name   = "name"
    values = ["al2023-ami-*-x86_64"]
  }
}

resource "aws_instance" "source" {
  ami                    = data.aws_ami.al2023.id
  instance_type          = var.instance_type
  subnet_id              = aws_subnet.private.id
  vpc_security_group_ids = [aws_security_group.source.id]
  iam_instance_profile   = aws_iam_instance_profile.ssm.name

  # GUARDRAIL: never assign a public IP. The demo's policy is "no public EC2".
  associate_public_ip_address = false

  user_data = <<-EOF
    #!/bin/bash
    dnf install -y docker
    systemctl enable --now docker
    sysctl -w vm.max_map_count=262144
    docker run -d --restart=always ${local.run_args} ${local.image}
  EOF
  tags      = { Name = "${var.prefix}-source" }
}

# ---- S3 snapshot bucket ----

resource "aws_s3_bucket" "snapshots" {
  bucket_prefix = "${var.prefix}-snapshots-"
  force_destroy = true
}

resource "aws_s3_bucket_public_access_block" "snapshots" {
  bucket                  = aws_s3_bucket.snapshots.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_versioning" "snapshots" {
  bucket = aws_s3_bucket.snapshots.id
  versioning_configuration {
    status = "Enabled"
  }
}

# ---- guard: fail the plan if a public IP was somehow requested ----

check "no_public_ip" {
  assert {
    condition     = aws_instance.source.associate_public_ip_address == false
    error_message = "The source instance must never have a public IP (demo policy: no public EC2)."
  }
}

output "source_private_ip" {
  value = aws_instance.source.private_ip
}

output "source_endpoint" {
  value = "http://${aws_instance.source.private_ip}:${local.port}"
}

output "snapshot_bucket" {
  value = aws_s3_bucket.snapshots.bucket
}
