# Terraform — AWS cloud path

These standalone modules are **reference examples** of the cloud counterpart to
the local KIND provisioning, checked in so you can read and `terraform apply`
them directly.

> **How the harness uses Terraform.** When you pick *Cloud*, the TUI generates an
> equivalent module into your workspace's `terraform/` and — after you confirm at
> the review screen — runs `terraform init && terraform apply -auto-approve` for
> you, streaming the output live. Pass `--no-apply` (or, non-interactively, omit
> `--apply`) to have it emit the files only and apply them yourself. The modules
> here are a realistic starting point, not a hardened production module — the
> emitted source instance is private-by-design (dedicated VPC + NAT, no public
> IP), but these defaults target a throwaway demo.

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
