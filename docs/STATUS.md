# Build status

What exists, how it is verified, and what is not verified. Tests named here are in `crates/plane/tests/`
(end-to-end, real processes) unless marked as unit tests.

## Phase 1: core slice — acceptance passes

Acceptance: *a rehearsal ticket-build run goes from trigger to approved PR against the fakes, survives a runner kill
and a plane restart, and expires correctly when left idle.*

| Check | Test |
|---|---|
| Trigger → research → plan submitted for review → approved in Slack → implement, push, PR → review thread fixed, replied, resolved → approval ends the run; runner removed | `e2e_ticket_build` |
| Two-phase runs: plan only in the first turn, changes requested in the thread → revised plan (earlier review superseded), approval in the portal → build instructions with the reviewer's notes → PR; *Don't build* ends an ad hoc run as `declined`; pushes and `pr.open` refused in the plan phase | `e2e_workflows` (`plan_review_changes_then_approval_builds`, ad hoc), `e2e_policy`, `e2e_lifetime` (revival in the plan phase). The plan-budget pause and the forced turn end are not covered by an e2e test (the scripted harness ends its turns itself). **Not yet tried with a real model.** |
| Runner killed while warm; plane process killed and restarted; the run continues from its checkpoint | `e2e_ticket_build` |
| Runner crashes mid-turn (after its push, before opening the PR) while one of two plane replicas is dead: one PR, one commit, one announcement | `e2e_resilience` |
| Duplicate webhook deliveries; first answer wins; the other surface is updated | `e2e_resilience` |
| Idle grace → checkpoint and exit; reminder → warning → expiry in order; summary to Slack and the ticket; revival from Slack restores the checkpoint | `e2e_lifetime` |
| Turn cap stops once, reports once, holds input until a human raises the cap; steering at the turn boundary | `e2e_lifetime` |
| Every section-8 policy rule refused and audited: repo allowlist, protected branch, namespace, fast-forward only, no deletion, merge only with a recorded approval naming the PR (and only when the project allows it), approve role, own-identity events ignored, bad run token | `e2e_policy` |
| Secrets encrypted at rest, shown only as hints, absent from runner environments and logs; payload redaction | `e2e_policy` |
| The event log alone rebuilds the run's state | `e2e_ticket_build` (`/api/runs/{id}/rebuild`) |
| Portal: dev sign-in, dashboard, run view with live activity, question card, answer from the portal | checked by hand in a browser against a running plane; portal unit tests (`npm test`) |

## Phase 2: real integrations

| Item | Verification |
|---|---|
| Copilot CLI adapter (BYOK, offline, `-p`, JSONL events, `--resume`, `factory` MCP server) | `e2e_copilot` runs the real Copilot CLI (1.0.36) against the plane's model endpoint and the fake provider: MCP tool calls reach primitives, usage is metered and priced, the session resumes on the next turn, the provider key never reaches the runner. Skips itself if Copilot CLI 1.0+ is absent. |
| Direct model provider, the plane's model endpoint, budget enforcement | `e2e_copilot` (budget exhausted → 402 at the endpoint → cap decision); unit tests for usage parsing and pricing |
| GitHub, Jira, Slack connectors | unit tests against local mock HTTP servers; contract tests `#[ignore]`d, run with `FACTORY_CONTRACT_*` credentials. **Not run against the real services** (no credentials were available). |
| Slack Socket Mode (outbound WebSocket instead of webhooks) | unit tests against a local fake Slack (`apps.connections.open` + WebSocket): events and button presses delivered before ack, no ack on a failed enqueue, `disconnect` reconnects; TLS handshake with Slack's real WebSocket host (`contract_tls_handshake`, `#[ignore]`d); checked on a local plane that a Socket Mode connector connects and closes when switched back to HTTP. **Not run against a real Slack app.** |
| Webhooks with signature verification; polling reconciliation | signature/parsing unit tests per connector; `e2e_triggers` (label webhook, missed webhook found by polling, Slack trigger, ticket cancellation) |
| Stack detection and the five images | unit tests; images `plane`, `runner-base`, `runner-rust` built locally; the other stack images are written but were not built here |
| Docker launcher | `e2e_docker` runs a ticket build with real runner containers (skips if the image is absent) |
| Egress proxy (allowlist, CONNECT, private feeds with credential injection) | unit tests with local upstreams |
| Budget and turn caps, steering incl. interrupt now, approvals in Slack and portal, kill switch | `e2e_lifetime`, `e2e_triggers`, `e2e_copilot` |

## Phase 3: breadth and production

| Item | Verification |
|---|---|
| PR review (monitoring trigger, inline threads, re-review resolves addressed threads, round limit with closing comment, approval ends, never pushes or votes, no self-review) | `e2e_workflows` |
| Ad hoc (report first, no change without confirmation) and security audit (ranked findings, fix only confirmed items) | `e2e_workflows` |
| LiteLLM per-run virtual keys (budget, model allowlist, expiry), spend read, reconciled and recorded, key deleted | `e2e_workflows` against the fake LiteLLM API |
| Azure DevOps connector | unit tests against a mock server; contract tests `#[ignore]`d. **Not run against Azure DevOps.** |
| Entra sign-in and group → role mapping | `e2e_workflows` against the fake Entra; unit tests for token validation. **Not run against a real tenant.** |
| Stuck, loop and no-progress detection (pause + alert to assignee and ops channel) | `e2e_workflows`; missed heartbeats in `e2e_resilience` |
| Outcome metrics, Prometheus metrics, retention (partition drop) | `e2e_workflows` |
| Fargate launcher | implemented against the same `Launcher` trait; request planning unit-tested. **Not verified in a real AWS account.** |
| AWS deployment definitions (`deploy/aws`) | **Not verified in a real AWS account**; not planned or applied. |
