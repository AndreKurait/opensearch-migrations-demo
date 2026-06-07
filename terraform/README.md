# Terraform — AWS cloud path

These modules are the **cloud** counterpart to the local KIND provisioning. The
TUI emits equivalent files into your workspace when you pick *Cloud*; the
standalone examples here are the same shape, checked in for reference and so you
can `terraform apply` them directly.

> **Not applied by the harness.** Per the demo design, the TUI *writes* the
> Terraform but never runs it. Review the plan and apply it yourself. These
> modules are a realistic starting point, not a hardened production module —
> they default to permissive CIDRs and small instances for a demo.

## Modules

- [`aws-source/`](aws-source) — a source search cluster on a single EC2
  instance running the chosen engine (Elasticsearch / OpenSearch / Solr) in
  Docker. ES and Solr have no managed AWS equivalent, so the source is modeled
  as an instance for engine/version fidelity. Also provisions the S3 snapshot
  bucket the Migration Assistant backfills from.
- [`aws-target/`](aws-target) — a managed **Amazon OpenSearch Service** domain
  as the migration target.

## Usage

```sh
cd aws-source
terraform init
terraform apply -var='region=us-east-1' -var='source_engine=elasticsearch' -var='source_version=7.10.2'

cd ../aws-target
terraform init
terraform apply -var='region=us-east-1' -var='target_version=3.3'
```

Then point the Migration Assistant at the `source_endpoint` /
`snapshot_bucket` / `target_endpoint` outputs.
