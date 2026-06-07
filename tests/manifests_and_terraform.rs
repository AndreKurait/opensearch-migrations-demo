//! Integration: the pure artifact emitters — k8s manifests, KIND config, and
//! the cloud terraform — exercised end to end from a realistic answer set, with
//! light structural validation of the rendered YAML/HCL. Mirrors the
//! migration-assistant CLI's `pack_and_manifest.rs`.

use ma_demo::model::{Answers, ClientApp, SnapshotStorage, SourceEngine, Target, TargetMode};
use ma_demo::{manifests, plan, terraform};

fn es_local() -> Answers {
    let mut a = Answers::new();
    a.target = Some(Target::Local);
    a.source_engine = Some(SourceEngine::Elasticsearch);
    a.source_version = Some("7.10.2".into());
    a.source_plugins = vec!["repository-s3".into(), "analysis-icu".into()];
    a.snapshot_storage = Some(SnapshotStorage::LocalStack);
    a.target_mode = Some(TargetMode::Provision);
    a.target_version = Some("3.3.0".into());
    a.clients = vec![ClientApp::Locust, ClientApp::SampleSearchApp];
    a.seed_data = Some(true);
    a
}

/// Every manifest the local plan emits should at minimum carry an apiVersion +
/// kind and reference the namespace where applicable.
#[test]
fn every_emitted_manifest_is_structurally_yaml() {
    let a = es_local();
    let p = plan::build(&a);
    let bodies: Vec<&str> = p
        .actions
        .iter()
        .filter_map(|act| match act {
            plan::Action::ApplyManifest { body, .. } => Some(body.as_str()),
            _ => None,
        })
        .collect();
    assert!(!bodies.is_empty());
    for body in bodies {
        assert!(
            body.contains("apiVersion:"),
            "manifest missing apiVersion: {body}"
        );
        assert!(body.contains("kind:"), "manifest missing kind: {body}");
    }
}

#[test]
fn source_manifest_carries_engine_image_and_plugins() {
    let a = es_local();
    let y = manifests::source(&a);
    assert!(y.contains("docker.elastic.co/elasticsearch/elasticsearch:7.10.2"));
    assert!(y.contains("command: [\"sh\", \"-c\","));
    assert!(y.contains("elasticsearch-plugin install --batch repository-s3"));
    assert!(y.contains("exec /usr/local/bin/docker-entrypoint.sh eswrapper"));
    assert!(y.contains("analysis-icu"));
}

#[test]
fn kind_configs_have_distinct_host_ports() {
    let a = es_local();
    let p = plan::build(&a);
    let configs: Vec<&str> = p
        .actions
        .iter()
        .filter_map(|act| match act {
            plan::Action::CreateKindCluster { config, .. } => Some(config.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(configs.len(), 2);
    // Source maps 19200, target maps 29200 — no host-port collision.
    assert!(configs.iter().any(|c| c.contains("hostPort: 19200")));
    assert!(configs.iter().any(|c| c.contains("hostPort: 29200")));
}

#[test]
fn locust_and_sample_app_target_the_source_service() {
    let a = es_local();
    let host = plan::source_service_name(SourceEngine::Elasticsearch);
    let clients = manifests::client_manifests(&a, host);
    assert_eq!(clients.len(), 2);
    for (_, body) in &clients {
        assert!(
            body.contains(host),
            "client should target source host {host}"
        );
    }
}

#[test]
fn cloud_path_emits_reviewable_terraform() {
    let mut a = es_local();
    a.target = Some(Target::Cloud);
    a.snapshot_storage = Some(SnapshotStorage::AwsS3);
    let files = terraform::files(&a);
    let by_path: std::collections::HashMap<&str, &str> = files
        .iter()
        .map(|f| (f.path.as_str(), f.body.as_str()))
        .collect();

    // Providers pins AWS + a region var.
    let providers = by_path["terraform/providers.tf"];
    assert!(providers.contains("hashicorp/aws"));
    // Source instance Dockerizes the chosen engine/version.
    let source = by_path["terraform/source.tf"];
    assert!(source.contains("docker.elastic.co/elasticsearch/elasticsearch:7.10.2"));
    // Target is a managed OpenSearch Service domain at major.minor.
    let target = by_path["terraform/target.tf"];
    assert!(target.contains("aws_opensearch_domain"));
    assert!(target.contains("OpenSearch_3.3"));
    // Snapshot bucket present for the S3 choice.
    assert!(by_path.contains_key("terraform/snapshots.tf"));
}

#[test]
fn data_seed_job_bulk_loads_three_indices() {
    let host = plan::source_service_name(SourceEngine::OpenSearch);
    let y = manifests::data_seed_job(host);
    // Self-contained curl bulk loader; creates + loads all SEED_INDICES.
    assert!(y.contains("curlimages/curl:8.10.1"));
    for idx in manifests::SEED_INDICES {
        assert!(y.contains(&format!("http://{host}:9200/{idx}/_bulk")));
    }
}
