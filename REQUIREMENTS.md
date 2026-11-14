# AI Factory: MVP requirements

This is the requirements list for a new system built from scratch. It states what the system must do and records the decisions already made. It does not prescribe internal design beyond those decisions.

**For the building agent.** You are building this unattended. Where this document is silent, choose the simplest option that satisfies the requirement and record it in `DECISIONS.md`. Build in the phase order in section 16, and do not start a phase until the previous phase's acceptance checks pass. You have no real Jira, GitHub, Azure DevOps, Slack, Entra or model credentials, so every external system needs a fake implementation that the tests run against (section 15). Check current documentation before relying on any crate version, CLI flag or API parameter named here; they were correct as of November 2026.

## 1. What the system is

A software factory: a control plane that takes work items (tickets, pull requests, ad hoc tasks), launches one containerised coding agent per work item, and supervises that agent until the work reaches its end state. Humans interact through Slack and a web portal. The agent never holds credentials for external systems; it works through primitives that the control plane authorises, executes and audits.

Terms used below:

- **Plane**: the control plane service.
- **Run**: one work item's whole lifecycle, from trigger to end state.
- **Runner**: the container in which a run's agent works.
- **Harness**: the third-party coding agent that runs inside the runner (GitHub Copilot CLI first).
- **Turn**: one invocation of the harness with an input, ending when the harness stops.
- **Primitive**: an operation on an external system that the agent requests and the plane performs.

## 2. Decisions already made

| Area | Decision |
|---|---|
| Language | Rust for the plane, the runner supervisor and the agent-side tooling. |
| Portal front end | TypeScript and React single-page app, served by the plane as static assets. |
| Datastore | Postgres only. No Redis. Run checkpoints and large payloads are stored in Postgres behind a blob-store interface, so object storage can replace it later. |
| Job queue | `apalis` 1.0 with `apalis-postgres` (stable since October 2026). Use the sqlx version it requires. Fallback if it proves unworkable: `graphile_worker`. |
| Scaling | Identical plane replicas. Jobs are claimed from the queue; singleton duties (pollers, schedulers, stuck-run detection, expiry) are guarded by a Postgres advisory lock. No dedicated leader. |
| Delivery guarantee | At-least-once job processing with idempotent primitives. A retried job must not open a second PR, post a duplicate comment or launch a second runner. |
| Primitive shape | One authorised HTTP API on the plane. In the runner, a single `factory` binary exposes it two ways: as an MCP server for the harness and as a CLI for scripts. |
| Harness | A pluggable adapter interface. GitHub Copilot CLI is the first adapter, in bring-your-own-model mode. A scripted adapter provides rehearsal mode. Other harnesses (Codex, OpenHands) are written later against the same interface. |
| Models | Model access is a connector role with interchangeable providers. Two are built: a direct provider (any OpenAI-compatible API, such as OpenAI itself) for local development and the proof of concept, and LiteLLM for the organisation's existing proxy later. Switching is a portal setting and changes nothing in the runner or harness. |
| Run model | Runs are event-driven turns. The agent never blocks inside a tool call waiting for a human; it ends its turn, and the reply starts the next turn. |
| Waiting | The plane holds the wait, not the runner. An idle runner is checkpointed and stopped after a short grace period and relaunched when the next event arrives. |
| Run lifetime | Bounded by idle time, with a reminder, a warning and then expiry (section 6). |
| Runner images | One image per stack plus a polyglot image. The plane detects the stack from the repository and picks the image with no human step (section 5). |
| Workflows | Workflow types are defined in code. YAML and Markdown are allowed for instructions, prompts and configuration, never as an end-to-end deterministic workflow engine. |
| Configuration | Environment variables only for what the plane needs to start (database URL, encryption key, listen address, bootstrap admin). Everything else is configured in the portal. |
| Deployment | Docker Compose for local development and tests. AWS ECS on Fargate for production. Runner launching sits behind an interface with a Docker implementation and a Fargate implementation. |

## 3. Control plane

- One service provides the API, the portal back end, triggers, scheduling and runner lifecycle.
- Any number of replicas can run at once. Replicas can start and stop at any time without losing or duplicating work.
- Live updates reach every replica (for portal streams and runner instruction delivery) through Postgres notifications.
- Database migrations run automatically and safely when several replicas start together.
- Structured JSON logs, liveness and readiness endpoints, and Prometheus-format metrics.

## 4. Harness adapter

The adapter interface normalises what differs between harnesses. The plane and the portal see only the normalised form.

- **Prepare**: configure the harness in the workspace with the model endpoint, the run's model key, the selected model and the `factory` MCP server.
- **Run a turn**: start the harness with an input and an optional session to resume, headless and with no interactive prompts.
- **Event stream**: emit normalised events for each model turn, tool call, tool result, agent message, usage report and error.
- **Interrupt**: stop the current turn cleanly.
- **Session export and import**: name the files that hold the session so the supervisor can checkpoint and restore them.
- **Capabilities**: declare what the harness supports (resume, usage reporting, interrupt), so the plane can fall back where a capability is missing.

Requirements on top of the interface:

- Where a harness cannot resume a session, the plane starts a fresh session seeded with a handoff summary built from the run's event log. This fallback exists from the start.
- Turn caps and wall-clock limits are enforced by the supervisor, not by the harness.
- The Copilot CLI adapter uses its custom-provider settings to point at the run's model endpoint (section 7), its non-interactive prompt mode, and its session resume. No GitHub sign-in is present in the runner.
- The scripted adapter runs a deterministic script per workflow type that calls the real primitives and passes through the real gates. This is rehearsal mode. It needs no model.

## 5. Runner

**Lifecycle**

- One run has one logical runner for its whole life. The physical container may be stopped and relaunched many times.
- A Rust supervisor is the container's entry point. It authenticates to the plane with a run token, receives instructions, runs turns through the harness adapter, streams events back and sends heartbeats.
- The supervisor checkpoints on request and before exit. A checkpoint is the work in progress pushed to a run-owned work-in-progress ref through the git primitive, plus the harness session files, stored by the plane.
- A relaunched runner restores from the latest checkpoint and continues the same session.
- Loss of a runner (crash, host retirement, deployment) is recovered the same way, from the last checkpoint.
- On finish, cancel, failure or expiry the container is removed and its workspace destroyed.

**Idle behaviour**

- When a turn ends and the run is waiting, the runner stays warm for a configurable grace period (default 20 minutes), so quick replies get a quick response.
- After the grace period the supervisor checkpoints and exits. The run stays in a waiting state in the plane.
- Any event for the run (human reply, PR comment, review, steering message, CI result) relaunches the runner.

**Images and stack selection**

- Prebuilt images: `python`, `dotnet`, `node`, `rust` and `polyglot` (all four). Each contains git, the stack's build, test and lint tools, a tool-version manager, the supervisor, the `factory` binary and the harness.
- When a repository is onboarded, the plane inspects it through the repo connector for marker files (for example `pyproject.toml`, `*.csproj`, `package.json`, `Cargo.toml`) and records a stack profile. It checks again at run start.
- One stack detected selects that image. Several stacks, or none, select `polyglot`.
- At start, the supervisor reads the repository's own version pins (for example `global.json`, `.nvmrc`, `.python-version`, `rust-toolchain.toml`) and installs the exact version if the image lacks it.
- An administrator can override the image per repository in the portal. A repository can also carry an optional `.factory/runner.yml` with setup, test and lint commands. Neither is required for a run to start.

**Limits**

- Per-run model selection with project defaults and per-run overrides.
- Per-run budget cap, enforced at the model endpoint (section 7) in both provider modes and tracked by the plane.
- Per-run turn cap and per-turn wall-clock limit.
- A run that hits a cap stops once, reports once to the assignee and waits for a human decision (raise the cap or end the run). It never retries on its own.

## 6. Run lifetime and expiry

- The idle clock measures time since the last human or external activity on the run: a reply, a steering message, a PR comment or review, an approval.
- At 24 hours idle, the plane sends the assignee a Slack reminder that states what the run is waiting for.
- At 48 hours idle, it sends a warning that states when the run will expire.
- At 72 hours idle, the run expires. The plane posts a final summary to Slack and to the ticket or PR, leaves the branch and PR in place, and tears the runner down.
- Any activity resets the clock. All three thresholds are configurable per project.
- An expired run's checkpoint is kept for a configurable period (default 14 days). During that period an operator can revive the run from the portal or Slack.
- A run also ends when its PR is closed or abandoned, its ticket is cancelled, or an operator cancels it. Each workflow type has its own end state and review-round limit (section 9).

## 7. Secrets and isolation

- A runner contains at most two credentials: its run token for the plane and a run-scoped model key. Both are scoped to the run, expire at teardown and are useless outside the factory network.
- No connector token or provider API key ever enters a runner: not in environment variables, files, git configuration or process arguments.
- The harness always talks to an OpenAI-compatible model endpoint with a run-scoped key. What sits behind that endpoint depends on the configured provider:
  - **Direct provider.** The plane serves the model endpoint itself. It accepts the run-scoped key, forwards the request (including streaming) to the provider with the real API key added, records token usage from each response, prices it from a per-model price table set in the portal, and refuses further requests once the run's budget is spent.
  - **LiteLLM.** The plane creates a LiteLLM virtual key at run start with the run's budget, model allowlist and an expiry, and the harness calls LiteLLM directly. At run end the plane reads the final spend, records it, then deletes the key.
- Any OpenAI-compatible base URL can be configured as a direct provider. Nothing in the system assumes a particular vendor or model name.
- Runners have no direct internet access. In AWS, runner tasks sit in private subnets whose security group permits traffic only to the plane, the LiteLLM proxy when that provider is in use, and a factory egress proxy.
- The egress proxy allows only an administrator-managed domain allowlist (package registries and similar). Security groups filter by address, not by domain, so the proxy is what enforces the allowlist.
- Private package feeds are reached through the egress proxy, which adds the feed credential. The runner never sees it.
- Event payloads are scanned for known secret patterns and redacted before they are stored.

## 8. Primitives

Every primitive call is authenticated with the run token, authorised against the run's project and repositories, made idempotent where it has side effects, and written to the audit log with its arguments and outcome.

- **Repos**: search, read files, clone, fetch, create branch, push. Git traffic goes through a plane git endpoint that adds the connector credential.
- **Tickets**: search, fetch with children and comments, transition, comment.
- **Pull requests**: open, read, list, comment, reply to a thread, mark a thread resolved or won't-fix, read review status and merge status.
- **CI**: read check and build status for a PR or branch, and read failing logs.
- **Humans**: send a message to the assignee, ask a question, request an approval. Ask and approve end the turn; the answer starts the next one.
- **Status**: report progress, attach artifacts (PR links, reports), declare the work finished or blocked.
- Anything else an agent needs for uninterrupted work is added as a primitive, never as a credential.

Policy enforced in code by the plane:

- Repository allowlist per project.
- Pushes are accepted only to the run's own branch namespace, fast-forward only. Protected branches and deletions are refused.
- No merge without a recorded human approval that names that PR.
- The agent acts under a factory service identity per connector. The plane ignores events authored by that identity, so an agent never answers itself.
- A deny-list for dangerous shell commands exists in the supervisor as a secondary safeguard. The network and primitive boundaries are the primary ones.

## 9. Workflow types

Each type is defined in code and has an instruction template (Markdown, with YAML front matter where useful) that is versioned in the repository and can be overridden per project in the portal. The exact template, policy and configuration versions used are recorded on each run.

- **Ticket build**: starts from a ticket. The agent researches, agrees direction with the assignee, implements, pushes, opens a PR, responds to review comments and CI failures, and continues until the PR is approved.
  - End state: the PR is approved. Merge is a human action by default; a project may enable merge by the agent behind the approval gate.
  - If the PR exceeds a configurable number of review rounds, the run stops and asks the assignee how to proceed.
- **PR review**: reviews an open PR on an onboarded repository. Its only output is comments and threads on the PR itself: it never pushes code, casts an approval vote, or merges.
  - After the first review, the run waits. Each new push or author reply triggers a re-review that checks earlier threads and resolves those that were addressed.
  - End state: the PR is approved by its human reviewers, merged or closed, or the run reaches a configurable limit of review rounds per PR (default 5). On reaching the limit it posts a closing comment that lists what is still open.
  - By default the factory does not review PRs opened by its own identity.
- **Ad hoc task**: no ticket. A title, a brief and an explicit scope, such as report first and fix on confirmation.
- **Security audit**: an ad hoc flavour. Ranked, verified findings first; fixes only for items a human confirms.

## 10. Triggers

- Manual from Slack, from Jira (a label or a transition, configurable) and from the portal.
- Jira webhooks as the fast path, with signature verification, and Jira polling as reconciliation for missed events.
- Repository monitoring for new and updated PRs on onboarded repositories, plus manual trigger of a review.
- One ticket or PR never has two active runs. This is enforced by a database uniqueness constraint, not by queue behaviour.
- An event for a work item that already has an active run is delivered to that run as input.
- Per-project limits on concurrent runs and on daily spend, and a global kill switch that stops new runs and pauses active ones.

## 11. Connectors and onboarding

- Connectors: Jira, GitHub, Azure DevOps, Slack, direct model provider and LiteLLM. Each is an implementation of a connector interface for its role (ticket source, repo host, chat, model provider).
- Secrets and settings are entered in the portal, encrypted at rest with a key supplied at startup, rotatable, and never displayed in full after entry.
- Each connector has a "test connection" action.
- Project onboarding: choose repositories, ticket sources, enabled workflows, default model and budgets, and the assignee mapping.
- Repository onboarding: the portal lists repositories from the connector; selecting one records its default branch, protected branches and detected stack with no further input.
- Identity mapping: a person's Jira, repo-host, Slack and Entra identities are linked, by email where possible and by an administrator otherwise.

## 12. Human in the loop

- Slack is the main channel with the assignee: findings, direction, questions, approvals, reminders and expiry warnings, in one thread per run.
- Every question and approval can also be answered in the portal. The first answer wins and the other surface updates.
- An approval counts only if the Slack or portal user maps to an identity that holds the approve role for that project.
- Humans can steer a running agent. A steering message is delivered at the next turn boundary; "interrupt now" stops the current turn and starts a new one with the message.
- Merge and other irreversible actions sit behind hard approval gates.
- Waiting on a human is shown as a distinct state and consumes no budget, because no turn runs.
- If the assignee does not respond, the section 6 reminders apply. A project can name a fallback person or channel.

## 13. Portal

- Sign-in through Entra ID, with roles mapped from Entra groups. A local development sign-in exists and can only be enabled by an environment variable.
- Roles: view, operate, approve, administer. Roles are granted per project, with a global administrator.
- Dashboard of active and past runs showing state, cost and a "needs you" marker.
- Run view: live activity, the conversation thread, inline question and approval cards, the tool trace, artifacts such as PR links, and the run's cost.
- Run controls: start, steer, pause, resume, cancel, revive an expired run, tear down.
- Configuration screens for connectors, projects, repositories, triggers, models, budgets, lifetimes, the egress allowlist and policy.
- Usable with thousands of events per run: windowed rendering, opens at the newest events, jump to latest.
- Dark theme with steel accents.

## 14. Logging, monitoring and audit

- An append-only event log per run is the system of record. A run's state can be rebuilt from it.
- Full work trace: every model turn, tool call, primitive call and result, in the normalised form.
- Large payloads are stored once and referenced from events. Events are partitioned by time and subject to a configurable retention period.
- Audit trail of human actions: who triggered, approved, rejected, steered, revived or changed configuration.
- Audit trail of connector and secret use per run.
- Cost and token accounting per run, per model and per project. With LiteLLM, the total is reconciled with LiteLLM's reported spend at run end.
- Detection of stuck, looping and crashed runs: missed heartbeats, a turn exceeding its limit, repeated identical tool calls, and repeated turns with no progress. Detection pauses the run and alerts the assignee and an operations channel.
- Outcome metrics per project, model and harness: PR approval rate, review rounds, time to approval and cost per approved PR.

## 15. Testability

- Tests and mocks are built alongside each feature, not after it. No feature is complete until it has tests that pass without real keys or network access to any external system.
- The mock for an external system is written before or together with the first code that calls it, so the system can be run and checked end to end at every step of the build.
- The full suite is run after each feature, and a failing suite is fixed before the next feature starts.
- Real keys are never required, requested or hard-coded during development. Tests that need real credentials are separate, skipped by default, and run only when the credentials are supplied.

- Every connector has a fake implementation with the same interface, runnable in Docker Compose: fake ticket source, fake repo host backed by real local git repositories, fake chat, fake model provider.
- Rehearsal mode (scripted harness, fake or real connectors) drives each workflow type end to end, including checkpoint, stop, relaunch and expiry.
- The end-to-end suite runs with one command and needs no external credentials.
- Tests cover the delivery guarantee: killing a plane replica or a runner mid-run causes no lost work and no duplicated side effect.
- Tests cover each policy rule in section 8 with an attempt that must be refused.

## 16. Build order

**Phase 1: core slice.** Plane skeleton, Postgres schema, job queue, event log, run state machine, Docker runner launcher, supervisor, `factory` binary, scripted harness, fake connectors, primitives for repos, PRs, tickets, humans and status, git endpoint with push policy, checkpoint and relaunch, idle expiry, portal with dev sign-in, dashboard and run view. Acceptance: a rehearsal ticket-build run goes from trigger to approved PR against the fakes, survives a runner kill and a plane restart, and expires correctly when left idle.

**Phase 2: real integrations.** Copilot CLI adapter, direct model provider with the plane's model endpoint and budget enforcement, GitHub, Jira and Slack connectors, webhook and polling triggers, stack detection and the five images, egress proxy, budget and turn caps, steering, approvals in Slack and portal. Acceptance: the same suite passes against the fakes with the real adapter code paths exercised wherever credentials are not needed, and each real connector has contract tests that run when credentials are supplied.

**Phase 3: breadth and production.** Azure DevOps connector, LiteLLM connector with per-run keys and spend reconciliation, PR review, ad hoc and security audit workflows, repository monitoring, Entra sign-in and role mapping, stuck and loop detection, outcome metrics, retention, the Fargate runner launcher and deployment definitions for AWS. Acceptance: all workflow types pass in rehearsal; the Fargate launcher is implemented against the same interface and clearly marked as not yet verified in a real AWS account.

## 17. Assumptions to confirm

These were chosen so the build can proceed. Each is isolated behind an interface or a setting, so reversing one is cheap.

1. GitHub and Jira are the first repo host and ticket source; Azure DevOps follows in phase 3.
2. In PR review, "approved" means approved by the PR's human reviewers; the review agent never casts a vote itself.
3. The organisation's existing LiteLLM proxy supports creating and deleting virtual keys through its API with the credentials the factory will be given.
4. The default limit of 5 review rounds per PR suits both workflows until real usage says otherwise.
5. `apalis` 1.0 is new as a stable release. If it blocks progress, switch to `graphile_worker` and record why.
