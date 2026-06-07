//! Terraform emitters for the CLOUD path.
//!
//! Per the harness design these are **written but not applied** — the operator
//! reviews and runs `terraform apply` themselves. Each function renders a `.tf`
//! file body as a string from [`Answers`]; the orchestrator writes them under
//! `terraform/` in the workspace. Pure, so the rendered HCL is asserted in
//! tests.
//!
//! The cloud topology mirrors the local one: a source search domain (a managed
//! cluster on EC2 for engine fidelity, since ES/Solr aren't managed services),
//! an optional target Amazon OpenSearch Service domain, and an S3 bucket for
//! snapshots. The intent is a realistic, reviewable starting point — not a
//! turnkey production module.

use crate::model::{Answers, SnapshotStorage, SourceEngine, TargetMode};

/// The provider + variables file shared by source and target.
pub fn providers_tf(region: &str) -> String {
    format!(
        "terraform {{\n\
         \x20 required_version = \">= 1.5.0\"\n\
         \x20 required_providers {{\n\
         \x20   aws = {{\n\
         \x20     source  = \"hashicorp/aws\"\n\
         \x20     version = \"~> 5.0\"\n\
         \x20   }}\n\
         \x20 }}\n\
         }}\n\
         \n\
         provider \"aws\" {{\n\
         \x20 region = var.region\n\
         }}\n\
         \n\
         variable \"region\" {{\n\
         \x20 type    = string\n\
         \x20 default = \"{region}\"\n\
         }}\n\
         \n\
         variable \"prefix\" {{\n\
         \x20 type    = string\n\
         \x20 default = \"ma-demo\"\n\
         }}\n"
    )
}

/// The S3 snapshot bucket (always emitted on the cloud path — the Migration
/// Assistant needs a repository regardless of source engine).
pub fn snapshot_bucket_tf() -> String {
    "# S3 bucket for migration snapshots (the backfill source-of-truth).\n\
     resource \"aws_s3_bucket\" \"snapshots\" {\n\
     \x20 bucket_prefix = \"${var.prefix}-snapshots-\"\n\
     \x20 force_destroy = true\n\
     }\n\
     \n\
     resource \"aws_s3_bucket_versioning\" \"snapshots\" {\n\
     \x20 bucket = aws_s3_bucket.snapshots.id\n\
     \x20 versioning_configuration {\n\
     \x20   status = \"Enabled\"\n\
     \x20 }\n\
     }\n\
     \n\
     output \"snapshot_bucket\" {\n\
     \x20 value = aws_s3_bucket.snapshots.bucket\n\
     }\n"
    .to_string()
}

/// A source search cluster on a single EC2 instance running the chosen engine
/// in Docker (ES/OpenSearch/Solr aren't all managed services, so we model the
/// source as an instance for engine/version fidelity). The user_data boots the
/// container.
pub fn source_instance_tf(engine: SourceEngine, version: &str) -> String {
    let image = match engine {
        SourceEngine::Elasticsearch => {
            format!("docker.elastic.co/elasticsearch/elasticsearch:{version}")
        }
        SourceEngine::OpenSearch => format!("opensearchproject/opensearch:{version}"),
        SourceEngine::Solr => format!("solr:{version}"),
    };
    let (port, run_args) = match engine {
        SourceEngine::Solr => (8983, "-p 8983:8983".to_string()),
        SourceEngine::Elasticsearch => (
            9200,
            "-p 9200:9200 -e discovery.type=single-node -e xpack.security.enabled=false"
                .to_string(),
        ),
        SourceEngine::OpenSearch => (
            9200,
            "-p 9200:9200 -e discovery.type=single-node -e DISABLE_SECURITY_PLUGIN=true"
                .to_string(),
        ),
    };
    format!(
        "# Source {engine_label} {version} on a single EC2 instance (Dockerized\n\
         # for engine/version fidelity — ES/Solr have no managed AWS equivalent).\n\
         data \"aws_ami\" \"al2023\" {{\n\
         \x20 most_recent = true\n\
         \x20 owners      = [\"amazon\"]\n\
         \x20 filter {{\n\
         \x20   name   = \"name\"\n\
         \x20   values = [\"al2023-ami-*-x86_64\"]\n\
         \x20 }}\n\
         }}\n\
         \n\
         resource \"aws_security_group\" \"source\" {{\n\
         \x20 name_prefix = \"${{var.prefix}}-source-\"\n\
         \x20 ingress {{\n\
         \x20   from_port   = {port}\n\
         \x20   to_port     = {port}\n\
         \x20   protocol    = \"tcp\"\n\
         \x20   cidr_blocks = [\"10.0.0.0/8\"]\n\
         \x20 }}\n\
         \x20 egress {{\n\
         \x20   from_port   = 0\n\
         \x20   to_port     = 0\n\
         \x20   protocol    = \"-1\"\n\
         \x20   cidr_blocks = [\"0.0.0.0/0\"]\n\
         \x20 }}\n\
         }}\n\
         \n\
         resource \"aws_instance\" \"source\" {{\n\
         \x20 ami                    = data.aws_ami.al2023.id\n\
         \x20 instance_type          = \"t3.large\"\n\
         \x20 vpc_security_group_ids = [aws_security_group.source.id]\n\
         \x20 # GUARDRAIL: never assign a public IP (demo policy: no public EC2).\n\
         \x20 associate_public_ip_address = false\n\
         \x20 user_data = <<-EOF\n\
         \x20   #!/bin/bash\n\
         \x20   dnf install -y docker\n\
         \x20   systemctl enable --now docker\n\
         \x20   sysctl -w vm.max_map_count=262144\n\
         \x20   docker run -d --restart=always {run_args} {image}\n\
         \x20 EOF\n\
         \x20 tags = {{ Name = \"${{var.prefix}}-source\" }}\n\
         }}\n\
         \n\
         # Fail the plan if a public IP is ever requested on the source instance.\n\
         check \"no_public_ip\" {{\n\
         \x20 assert {{\n\
         \x20   condition     = aws_instance.source.associate_public_ip_address == false\n\
         \x20   error_message = \"The source instance must never have a public IP (demo policy: no public EC2).\"\n\
         \x20 }}\n\
         }}\n\
         \n\
         output \"source_endpoint\" {{\n\
         \x20 value = \"http://${{aws_instance.source.private_ip}}:{port}\"\n\
         }}\n",
        engine_label = engine.label(),
    )
}

/// An Amazon OpenSearch Service domain as the migration target (managed
/// service — the realistic cloud target).
pub fn target_domain_tf(version: &str) -> String {
    format!(
        "# Target: a managed Amazon OpenSearch Service domain.\n\
         resource \"aws_opensearch_domain\" \"target\" {{\n\
         \x20 domain_name    = \"${{var.prefix}}-target\"\n\
         \x20 engine_version = \"OpenSearch_{version}\"\n\
         \n\
         \x20 cluster_config {{\n\
         \x20   instance_type  = \"r6g.large.search\"\n\
         \x20   instance_count = 2\n\
         \x20 }}\n\
         \n\
         \x20 ebs_options {{\n\
         \x20   ebs_enabled = true\n\
         \x20   volume_size = 50\n\
         \x20 }}\n\
         }}\n\
         \n\
         output \"target_endpoint\" {{\n\
         \x20 value = aws_opensearch_domain.target.endpoint\n\
         }}\n"
    )
}

/// One emitted terraform file: a relative path under the workspace + its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TfFile {
    pub path: String,
    pub body: String,
}

/// All terraform files for the cloud path, given the answers. The OpenSearch
/// Service `engine_version` truncates the patch (it wants major.minor).
pub fn files(answers: &Answers) -> Vec<TfFile> {
    let engine = answers.source_engine.unwrap_or(SourceEngine::Elasticsearch);
    let src_ver = answers.source_version.as_deref().unwrap_or("7.10.2");
    let region = "us-east-1";

    let mut out = vec![
        TfFile {
            path: "terraform/providers.tf".into(),
            body: providers_tf(region),
        },
        TfFile {
            path: "terraform/source.tf".into(),
            body: source_instance_tf(engine, src_ver),
        },
    ];

    // Snapshot bucket whenever cloud snapshot storage is requested (S3) or
    // unset/localstack maps to a real bucket on the cloud path.
    if answers.snapshot_storage != Some(SnapshotStorage::None) {
        out.push(TfFile {
            path: "terraform/snapshots.tf".into(),
            body: snapshot_bucket_tf(),
        });
    }

    if answers.target_mode == Some(TargetMode::Provision) {
        let tv = answers.target_version.as_deref().unwrap_or("3.3.0");
        let mm = major_minor(tv);
        out.push(TfFile {
            path: "terraform/target.tf".into(),
            body: target_domain_tf(&mm),
        });
    }

    out
}

/// `major.minor` of a semver-ish string (`3.3.0` → `3.3`). OpenSearch Service
/// engine versions are major.minor.
fn major_minor(v: &str) -> String {
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() >= 2 {
        format!("{}.{}", parts[0], parts[1])
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Target, TargetMode};

    fn cloud_answers() -> Answers {
        let mut a = Answers::new();
        a.target = Some(Target::Cloud);
        a.source_engine = Some(SourceEngine::Elasticsearch);
        a.source_version = Some("7.10.2".into());
        a.snapshot_storage = Some(SnapshotStorage::AwsS3);
        a.target_mode = Some(TargetMode::Provision);
        a.target_version = Some("3.3.0".into());
        a
    }

    #[test]
    fn providers_pins_aws_and_region() {
        let tf = providers_tf("us-west-2");
        assert!(tf.contains("hashicorp/aws"));
        assert!(tf.contains("default = \"us-west-2\""));
        assert!(tf.contains("required_version = \">= 1.5.0\""));
    }

    #[test]
    fn source_instance_embeds_engine_image_and_run_args() {
        let tf = source_instance_tf(SourceEngine::Elasticsearch, "7.10.2");
        assert!(tf.contains("docker.elastic.co/elasticsearch/elasticsearch:7.10.2"));
        assert!(tf.contains("discovery.type=single-node"));
        assert!(tf.contains("vm.max_map_count=262144"));
        assert!(tf.contains("aws_instance"));

        let solr = source_instance_tf(SourceEngine::Solr, "9.7.0");
        assert!(solr.contains("solr:9.7.0"));
        assert!(solr.contains("8983:8983"));
    }

    #[test]
    fn source_instance_forbids_a_public_ip() {
        // The guardrail: emitted source instances must never get a public IP,
        // and a check block enforces it at plan time.
        for eng in [
            SourceEngine::Elasticsearch,
            SourceEngine::OpenSearch,
            SourceEngine::Solr,
        ] {
            let tf = source_instance_tf(eng, "1.2.3");
            assert!(
                tf.contains("associate_public_ip_address = false"),
                "{} instance must disable public IP",
                eng.id()
            );
            assert!(
                tf.contains("check \"no_public_ip\""),
                "{} must carry the no-public-ip guard",
                eng.id()
            );
            // No 0.0.0.0/0 INGRESS (egress 0.0.0.0/0 is fine for the image pull).
            assert!(
                !tf.contains(
                    "cidr_blocks = [\"0.0.0.0/0\"]\n         \x20 }}\n         \x20 egress"
                ),
                "ingress must not be open to the world"
            );
        }
    }

    #[test]
    fn target_domain_uses_engine_version_format() {
        let tf = target_domain_tf("3.3");
        assert!(tf.contains("engine_version = \"OpenSearch_3.3\""));
        assert!(tf.contains("aws_opensearch_domain"));
    }

    #[test]
    fn files_include_providers_source_snapshots_target() {
        let f = files(&cloud_answers());
        let paths: Vec<&str> = f.iter().map(|t| t.path.as_str()).collect();
        assert!(paths.contains(&"terraform/providers.tf"));
        assert!(paths.contains(&"terraform/source.tf"));
        assert!(paths.contains(&"terraform/snapshots.tf"));
        assert!(paths.contains(&"terraform/target.tf"));
    }

    #[test]
    fn leave_to_ma_omits_target_tf() {
        let mut a = cloud_answers();
        a.target_mode = Some(TargetMode::LeaveToMa);
        let paths: Vec<String> = files(&a).iter().map(|t| t.path.clone()).collect();
        assert!(!paths.contains(&"terraform/target.tf".to_string()));
    }

    #[test]
    fn no_snapshot_omits_bucket_tf() {
        let mut a = cloud_answers();
        a.snapshot_storage = Some(SnapshotStorage::None);
        let paths: Vec<String> = files(&a).iter().map(|t| t.path.clone()).collect();
        assert!(!paths.contains(&"terraform/snapshots.tf".to_string()));
    }

    #[test]
    fn major_minor_truncates_patch() {
        assert_eq!(major_minor("3.3.0"), "3.3");
        assert_eq!(major_minor("2.19.0"), "2.19");
        assert_eq!(major_minor("3"), "3");
    }

    #[test]
    fn target_tf_uses_major_minor_from_files() {
        let f = files(&cloud_answers());
        let target = f.iter().find(|t| t.path.ends_with("target.tf")).unwrap();
        assert!(target.body.contains("OpenSearch_3.3"));
        assert!(!target.body.contains("OpenSearch_3.3.0"));
    }
}
