# AI Factory

A control plane that takes work items (tickets, pull requests, ad hoc tasks), launches one containerised coding
agent per work item, and supervises it until the work reaches its end state. Humans work with it through Slack and
a web portal. The agent never holds credentials for external systems: it calls *primitives* that the plane
authorises, executes and audits.

Requirements: [`REQUIREMENTS.md`](REQUIREMENTS.md). Choices made where they were silent: [`DECISIONS.md`](DECISIONS.md).
Portal API contract: [`docs/API.md`](docs/API.md). Status by phase: [`docs/STATUS.md`](docs/STATUS.md).

## Layout

| Path | What |
|---|---|
| `crates/plane` | The control plane: API, portal back end, git endpoint, model endpoint, job queue (apalis on Postgres), run engine, connectors and their fakes, launchers (process, Docker, Fargate), egress proxy (`egress` binary) |
| `crates/supervisor` | The runner's entry point; harness adapters (`scripted` rehearsal, `copilot`) |
| `crates/factory-cli` | The `factory` binary in the runner: MCP server (`factory mcp`) and CLI (`factory call …`) |
| `crates/proto`, `crates/client` | Shared types; HTTP client with replica failover |
| `portal/` | React + TypeScript SPA, served by the plane |
| `templates/` | Workflow instruction templates (Markdown with YAML front matter) |
| `docker/`, `docker-compose.yml` | Plane, runner images (base, python, dotnet, node, rust, polyglot), local stack |
| `deploy/aws` | ECS on Fargate definitions (not yet verified in AWS) |

## Run the tests (one command, no credentials)

```bash
scripts/test.sh
```

Windows: `scripts\test.ps1`. This starts Postgres with Docker Compose, builds the workspace, runs unit and
end-to-end tests (real plane processes, real runner supervisors, fake Jira/GitHub/Slack/model provider/LiteLLM/Entra,
real local git repositories), then the portal's tests and build. Two suites skip themselves when their tool is
absent: `e2e_copilot` (needs Copilot CLI 1.0+) and `e2e_docker` (needs the runner images; set
`FACTORY_DOCKER_IMAGES=1` to build them first). Contract tests against real services are `#[ignore]`d and read
their credentials from `FACTORY_CONTRACT_*` environment variables (see the top of each connector file):

```bash
cargo test -p factory-plane --lib -- --ignored contract
```

## Run it locally

```bash
docker compose --profile images build      # runner images (first time)
docker compose up -d                       # postgres, two plane replicas, egress proxy
```

Open http://localhost:8080 and use the development sign-in (`admin@example.test` is the bootstrap admin). The
development sign-in exists only because `FACTORY_DEV_LOGIN=1` is set; production uses Entra ID.

Start-up environment variables (everything else is configured in the portal):

| Variable | Meaning |
|---|---|
| `DATABASE_URL` | Postgres |
| `FACTORY_ENCRYPTION_KEYS` | `kid:base64key[,kid2:key2…]`; the first encrypts, all decrypt (rotation: add a new key first, then *Settings → rotate*) |
| `FACTORY_LISTEN_ADDR` | default `0.0.0.0:8080` |
| `FACTORY_PUBLIC_URLS` | how runners reach the planes (several for failover) |
| `FACTORY_BOOTSTRAP_ADMIN` | email of the first global administrator |
| `FACTORY_DEV_LOGIN` | `1` enables the development sign-in and the fake-system controls |
| `FACTORY_ENTRA_TENANT_ID`, `_CLIENT_ID`, `_CLIENT_SECRET`, `_REDIRECT_URL` | Entra sign-in |
| `FACTORY_DOCKER_NETWORK`, `FACTORY_IMAGE_PREFIX` | Docker launcher |
| `FACTORY_FARGATE_*` | Fargate launcher (see `deploy/aws/README.md`) |
