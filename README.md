# Migration Assistant — Demo Environment Harness

An interactive **TUI** that stands up a complete, throwaway test environment for
the [OpenSearch **Migration Assistant**](https://github.com/opensearch-project/opensearch-migrations),
then hands you off to the Migration Assistant CLI to run the actual migration.

It asks you a short series of questions — *local or cloud? which source engine
and version? which plugins? snapshot storage? a target cluster? which client
apps?* — and from your answers it provisions everything, end to end:

```
local (KIND)                                   cloud (Terraform, emitted only)
────────────                                   ───────────────────────────────
ma-demo-source  KIND cluster                   aws_instance.source  (Dockerized ES/OS/Solr)
  ├─ Elasticsearch / OpenSearch / Solr         aws_s3_bucket.snapshots
  ├─ requested engine plugins                  aws_opensearch_domain.target
  ├─ LocalStack S3 (simulated snapshots)
  ├─ Locust load generator
  ├─ sample search application
  └─ DataGenerator seed Job
ma-demo-target  KIND cluster (optional)
  └─ OpenSearch
        │
        ▼
install + launch  migration-assistant  (from AndreKurait/opensearch-migrations 3.3.1)
```

Built in Rust on [Ratatui](https://ratatui.rs) 0.30.1, mirroring the
architecture of the Migration Assistant CLI it sets up for: **pure decision
logic** (the wizard state machine + every manifest/Terraform emitter) separated
from **all external I/O** behind a single `CommandRunner` seam, so the entire
provisioning pipeline is unit-tested against a mock with no Docker — and an
Elm-architecture TUI whose rendered frames are asserted via `TestBackend`.

## Quick start

```sh
# Build + run (needs docker, kind, kubectl, curl on PATH for the local path).
cargo run

# Or non-interactively with sensible defaults (CI / unattended):
cargo run -- --non-interactive

# Just see the plan it would provision — touches nothing:
cargo run -- plan
```

The harness writes everything under `./migration-demo-workspace/` (one
removable folder): the saved plan (`plan.json`), every rendered manifest + KIND
config, the emitted Terraform, and the installed `migration-assistant` binary
under `bin/`. Re-running resumes from the saved plan.

## What it asks

| Question | Choices |
|---|---|
| **Where** | Local (KIND/Docker) · Cloud (AWS/Terraform) |
| **Source engine** | Elasticsearch · OpenSearch · Solr |
| **Source version** | per-engine (ES 5.6–8.17, OS 1.3–2.19, Solr 8–9) |
| **Source plugins** | repository-s3, analysis-icu, … (multi-select) |
| **Snapshot storage** | LocalStack (simulated S3) · real AWS S3 · none |
| **Target** | provision a target · leave it to the Migration Assistant |
| **Target kind** | local OpenSearch (KIND) · **Amazon OpenSearch Serverless NextGen** (cloud, fast) |
| **Target version** | OpenSearch 2.19 / 3.1 / 3.3 (local KIND target only) |
| **Client apps** | Locust load gen · sample search app (multi-select) |
| **Seed data** | seed sample documents into the source (yes/no) |
| **MA handoff** *(local only)* | **deploy MA locally to KIND** (no AWS) · install the MA CLI (deploys to EKS) |

The flow is adaptive — e.g. choosing *leave the target to the Migration
Assistant* skips both the target-kind and target-version questions, choosing
the **AOSS NextGen** target kind skips the version question (a serverless
collection takes no OpenSearch version), and the MA-handoff question only
appears for local runs (cloud always installs the EKS-targeting CLI).

### Migration Assistant handoff

Once the environment is up, the harness hands off to the Migration Assistant.
For **local** runs you choose how:

- **Deploy MA locally to KIND** (default for local, no AWS) — `helm install`s
  the `migrationAssistantWithArgo` chart into a dedicated KIND cluster
  (`ma-demo-ma`, node image `kindest/node:v1.35.0` to meet the chart's k8s
  ≥1.35 floor) with the `opensearchstaging` images, waits for the
  `migration-console` statefulset, then `kubectl exec`s you into
  `migration-console-0`. A complete migration with zero cloud dependencies.
  Requires `helm` + the opensearch-migrations chart (set `MA_CHART_PATH`, or a
  sibling checkout is auto-detected).
- **Install the MA CLI** — curls `install.sh` from the
  [`AndreKurait/opensearch-migrations` 3.3.1 release](https://github.com/AndreKurait/opensearch-migrations/releases/tag/3.3.1)
  and launches it; the CLI deploys MA to **EKS** (needs AWS).

Override non-interactively with `--ma-handoff deploy-local-helm|install-cli`.

### Target kinds

- **Local OpenSearch (KIND)** — a self-managed OpenSearch cluster in a second
  KIND cluster (`ma-demo-target`, host port `localhost:29200`).
- **Amazon OpenSearch Serverless — NextGen collection** — a managed cloud
  collection that goes ACTIVE in ~5 seconds and scales to zero. The harness
  drives the `aws opensearchserverless` control plane (NextGen group with
  standby replicas, the encryption/network/data-access policies, then the
  collection) and records the resolved `*.aoss.<region>.on.aws` endpoint.
  Requires AWS CLI v2 (≥ 2.34.56) on PATH and credentials. **Note:** AOSS
  data-plane requests with a body must send an explicit `X-Amz-Content-SHA256`
  header (the payload hash) or they 403 — the sample client handles this.

## Subcommands

```
ma-demo [flags]            Run the wizard, provision, launch MA (default)
ma-demo run [flags]        Same as default
ma-demo plan [flags]       Collect answers + print the plan; provision nothing
ma-demo clear [flags]      Wipe the local workspace (no Docker/cloud changes)
ma-demo destroy [flags]    Delete the local KIND clusters + wipe the workspace
ma-demo version
ma-demo help

Flags:
  -y, --non-interactive    Accept defaults; for CI / unattended runs.
  --dry-run, --plan        Stop after printing the plan.
  --workspace DIR          Workspace dir (default ./migration-demo-workspace).
```

## How the local path works (multiple KIND clusters)

The source and (optional) target run as **separate KIND clusters** —
`ma-demo-source` and `ma-demo-target` — the most realistic cross-cluster
migration topology. Each cluster maps its search NodePort to a distinct host
port (source → `localhost:19200`, target → `localhost:29200`) so both the
operator and the Migration Assistant can reach them. Source workloads (the
search engine, LocalStack, Locust, the sample app, the DataGenerator Job) all
live in the `ma-demo` namespace of the source cluster.

## How the cloud path works (Terraform, emitted only)

Choosing **Cloud** writes a reviewable Terraform module under
`terraform/` — provider + variables, a Dockerized source instance (ES/OS/Solr
have no managed AWS equivalent, so the source is modeled as an EC2 instance for
engine fidelity), an S3 snapshot bucket, and a managed Amazon OpenSearch Service
target domain. **The harness writes the Terraform but does not apply it** — you
review it and run `terraform init && terraform apply` yourself. See
[`terraform/README.md`](terraform/README.md).

## The handoff

Once the environment is up, the harness installs the Migration Assistant CLI
from the
[`AndreKurait/opensearch-migrations` 3.3.1 release](https://github.com/AndreKurait/opensearch-migrations/releases/tag/3.3.1)
(prebuilt binary; no Rust toolchain needed) into `./migration-demo-workspace/bin/`
and launches it. From there the Migration Assistant drives the migration itself
(metadata → backfill → capture-and-replay).

## Development

```sh
cargo test                              # 120+ unit + integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo build --release
```

The toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (Rust
1.96.0), the same version the Migration Assistant CLI builds against.

### Layout

```
src/
  core/      error · runner (the I/O seam) · state · util
  domain/    model · wizard (state machine) · manifests · terraform · plan
  view/      ui (stdout discipline) · progress (timeline) · tui (Ratatui wizard)
  command/   app (provisioning orchestrator) · cli (dispatcher)
tests/       provision_pipeline · cli_dispatch · manifests_and_terraform
terraform/   standalone AWS examples (aws-source / aws-target)
```

## License

Apache-2.0 (matches opensearch-project).
