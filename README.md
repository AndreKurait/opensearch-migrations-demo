# Migration Assistant — Demo Environment Harness

An interactive **TUI** that stands up a complete, throwaway test environment for
the [OpenSearch **Migration Assistant**](https://github.com/opensearch-project/opensearch-migrations),
then hands you off to the Migration Assistant CLI to run the actual migration.

![ma-demo wizard walkthrough](docs/demo.gif)

> The wizard above is the real `ma-demo` binary in `plan` mode (no resources
> created). It walks every question — source engine + version + plugins,
> snapshot storage, target kind, client apps, sample-data seeding, and the
> Migration Assistant handoff — then prints the plan it would provision.

After provisioning, the harness streams each step live and then drops into a
**real-time status dashboard** that re-probes the actual resources every 2s and
stays up until you quit (or open it any time with `ma-demo status`):

![ma-demo live status dashboard](docs/dashboard.gif)

> Recorded against a live environment — cluster health, source workloads,
> per-index document counts, the target, and the Migration Assistant all update
> in place as the dashboard re-probes.

**Before anything is provisioned**, the harness always shows an editable
**review screen** — the single front door for an interactive run. It carries a
context header (the harness version, the workspace, the AWS account/profile/
region you're about to deploy into, and a newer-release upgrade hint if one
exists) above the whole editable plan — move to any field and edit it, confirm,
or cancel. There are no bash-style printed lines before it. This runs on *every*
interactive run, including a resume from a saved plan, so a fully-answered plan
never silently deploys (especially important for the cloud / real-AWS path).
`-y` auto-confirms for unattended runs (which print the same context to stdout,
since there's no TTY). Press `Ctrl-C` (or `q`/`Esc`) to cancel any screen.

The startup release check is fail-silent — never blocks; opt out with
`MA_DEMO_NO_UPDATE_CHECK=1`.

It asks you a short series of questions — *local or cloud? which source engine
and version? which plugins? snapshot storage? a target cluster? which client
apps?* — and from your answers it provisions everything, end to end:

```
local (KIND)                                   cloud (Terraform, applied)
────────────                                   ──────────────────────────
ma-demo-source  KIND cluster                   aws_instance.source  (Dockerized ES/OS/Solr,
  ├─ Elasticsearch / OpenSearch / Solr           private VPC + NAT — no public IP)
  ├─ requested engine plugins                  aws_s3_bucket.snapshots
  ├─ LocalStack S3 (simulated snapshots)       AOSS NextGen collection  (serverless target)
  ├─ Locust load generator                       or aws_opensearch_domain.target
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
# Install the latest release (macOS / Linux, x86_64 / aarch64):
curl -fsSL https://github.com/AndreKurait/opensearch-migrations-demo/releases/latest/download/install.sh | bash
ma-demo

# …or from source (needs docker, kind, kubectl, curl on PATH for the local path):
cargo run

# Non-interactively with sensible defaults (CI / unattended):
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

## How the cloud path works (Terraform, applied)

Choosing **Cloud** writes a Terraform module under `terraform/` — provider +
variables, a Dockerized source instance (ES/OS/Solr have no managed AWS
equivalent, so the source is modeled as an EC2 instance for engine fidelity, in
a **dedicated private VPC behind a NAT gateway — it never gets a public IP**), an
S3 snapshot bucket, and the target (an **AOSS NextGen** serverless collection or
a managed Amazon OpenSearch Service domain). After you **confirm at the review
screen**, the harness runs `terraform init && terraform apply` for you, streaming
the live output to the terminal. Pass `--no-apply` to emit the files only and
apply them yourself. See [`terraform/README.md`](terraform/README.md).

## The handoff

Once the environment is up, the harness installs the Migration Assistant CLI
from the
[`AndreKurait/opensearch-migrations` 3.3.1 release](https://github.com/AndreKurait/opensearch-migrations/releases/tag/3.3.1)
(prebuilt binary; no Rust toolchain needed) into `./migration-demo-workspace/bin/`
and launches it. From there the Migration Assistant drives the migration itself
(metadata → backfill → capture-and-replay).

## Development

```sh
cargo test                              # full unit + integration suite
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
docs/        demo.tape / demo.gif (wizard) · dashboard.tape / dashboard.gif (live status)
```

### Regenerating the GIFs

The recordings are scripted with [VHS](https://github.com/charmbracelet/vhs):

```sh
cargo build --release          # both tapes drive ./target/release/ma-demo
vhs docs/demo.tape             # → docs/demo.gif (the wizard, plan mode)
# dashboard.gif needs a running env first (ma-demo run), then:
vhs docs/dashboard.tape        # → docs/dashboard.gif (the live status dashboard)
```

## License

Apache-2.0 (matches opensearch-project).
