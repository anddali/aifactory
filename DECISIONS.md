# Decisions

Choices made where `REQUIREMENTS.md` is silent, and deviations with reasons. Newest sections are appended at
the end of each list.

## Structure

- **Workspace layout.** `crates/proto` (types shared across process boundaries), `crates/client` (HTTP client for
  the runner and primitive APIs, with replica failover), `crates/plane` (the control plane, lib + `plane` binary),
  `crates/supervisor` (`supervisor` binary, harness adapters), `crates/factory-cli` (`factory` binary: MCP server and
  CLI). `portal/` is the React SPA, `templates/` holds the workflow instruction templates, `docker/` the images and
  compose file, `deploy/aws` the production definitions.
- **One primitive catalogue.** `proto::primitives::CATALOGUE` defines every primitive once; the MCP tool list, the
  CLI and the plane dispatcher all derive from it. MCP tool names replace `.` with `_` (`pr.comment` → `pr_comment`)
  because MCP clients restrict tool-name characters.
- **MCP implemented by hand.** The `factory mcp` server implements the JSON-RPC subset the harness uses
  (`initialize`, `tools/list`, `tools/call`, `ping`, notifications) over newline-delimited stdio. This is ~100 lines
  and avoids tracking a fast-moving SDK; verified against Copilot CLI 1.0.36.

## Data and queue

- **Queue.** `apalis` 1.2 + `apalis-postgres` 1.1 (sqlx 0.9), one queue `factory`, a single `Job` enum. apalis
  re-enqueues jobs of dead workers (heartbeat/orphan detection), giving at-least-once delivery. Retries are explicit:
  a failed job re-enqueues itself with exponential back-off up to 8 attempts, so behaviour does not depend on
  middleware defaults. Job idempotency keys de-duplicate enqueues (start, launch, chat posts, teardown).
- **Migrations.** sqlx migrations for the plane and apalis' own migrator run under one session advisory lock, so
  replicas starting together serialise.
- **Event log.** `events` is partitioned by month on `ts` with a default partition; the singleton housekeeping
  creates the next partitions (moving any rows from the default partition) and drops whole partitions past the
  retention period. Sequence numbers are allocated per run from `runs.event_seq` in the same statement as the
  insert, which also issues `pg_notify`. Payloads over 16 KiB go to the blob store and are referenced.
- **Blob store.** Content-addressed (`sha256`) table `blobs` behind a `BlobStore` trait; checkpoints' session
  archives and large payloads use it.
- **Fakes share state through Postgres** (`fake_store`), so every replica sees the same fake Jira/GitHub/Slack;
  the fake repo host is real bare git repositories on disk. Fake actions (a reviewer commenting, a human replying in
  Slack) are driven through `/api/fake/...`, which is mounted only when the development sign-in is enabled. They go
  through the same event jobs as real webhooks and are also recorded for polling reconciliation.

## Runs, runners and idempotency

- **States.** `queued → starting → running ⇄ waiting`, plus `paused`, and terminal `finished`, `failed`,
  `cancelled`, `expired` (revivable within retention). `waiting_for` is one of `human_answer`, `approval`, `review`,
  `ci`, `cap_decision`, `input`. Every transition is an event; `runs::rebuild_state` folds the log back into the
  state and the e2e tests check it matches.
- **Instructions are rows.** Inputs (answers, review comments, steering, CI) become `instructions` rows; supervisors
  long-poll for them (woken by `LISTEN/NOTIFY` on any replica). An instruction a dead runner had taken goes back to
  pending, so a relaunched runner replays the turn.
- **Runner generations.** Each launch increments `runner_generation` atomically; the launch job carries the
  generation and is skipped if stale. The container/process name is derived from run and generation, and launchers
  are idempotent on that name, so a retried launch never starts a second runner. The run token is derived
  deterministically (HMAC of the encryption key over run id, generation and a per-run salt), so a retried launch
  job hands out the same token, and a new generation's token invalidates the old runner's.
- **Primitive idempotency.** Each side-effecting call carries an idempotency key (the client defaults it to a hash
  of primitive, arguments and instruction id; the scripted harness uses `instruction:step:name`). The first
  successful response is stored in `primitive_calls` and replayed for the same key. Calls interrupted mid-flight
  re-execute, so each implementation also checks for its earlier effect: `pr.open` reuses an open PR for the head
  branch, comments and replies carry a hidden marker derived from the key and are looked up before posting, and
  question/artifact ids are derived from the key.
- **Job side effects** (chat posts, ticket/PR comments) use `side_effects` with markers the same way.
- **Deterministic rehearsal commits.** The scripted harness commits with dates derived from the instruction and
  step, so a turn replayed after a crash produces the same commit and the push is a no-op rather than a refused
  non-fast-forward.
- **Checkpoint format.** A checkpoint is a WIP commit whose tree is the whole working tree (tracked and untracked,
  built with a temporary index) and whose parents are the branch HEAD and the previous WIP tip. It is pushed to the
  run-owned branch `factory-wip/<run>`; because it has the previous tip as a parent, the push is a fast-forward,
  so the fast-forward-only rule holds for the WIP ref too. Restore checks out `wip^1` as the branch and the WIP tree
  as the working tree. The harness session directory is archived (tar.gz) and stored in the blob store. The
  supervisor checkpoints after every turn as well as on request and before exit, so a crash loses at most the
  turn in progress (which is then replayed).
- **Idle clock.** The clock starts when a run enters `waiting` or `paused`; time spent in turns does not count.
  Human or external activity (answer, steer, review, approval, revive) resets it. Reminder, warning and expiry fire
  in that order. Expired runs keep their checkpoint for the project's retention; revival delivers an input saying
  the run was revived, which relaunches the runner from the checkpoint.
- **Caps.** The plane counts turns and spend; when either is reached the next turn instructions are held, the run
  waits for `cap_decision`, and one message is posted. The supervisor enforces the per-turn wall-clock limit
  (interrupt, then a 15 s grace, then the turn is reported as timed out). A timed-out turn pauses the run as stuck.
- **Review rounds.** For ticket builds, each review submission is a round; past the limit the run asks the assignee
  ("continue" raises the allowance, "end" finishes). For PR reviews each push or author reply is a round; at the
  limit the factory posts a closing comment listing open threads and finishes.

## Git endpoint and policy

- **Smart HTTP over a per-replica mirror.** The plane serves `/git/<repo-id>` itself: it fetches upstream into a
  local bare mirror (with the connector credential as an `http.extraHeader` on the plane side only), runs
  `git upload-pack`/`receive-pack --stateless-rpc` against the mirror, and forwards accepted ref updates upstream.
  Before receive-pack runs, the plane parses the ref-update commands and refuses deletions, protected branches and
  anything outside `factory/<run>`, `factory/<run>-*` and `factory-wip/<run>`; receive-pack runs with
  `denyNonFastForwards` so non-fast-forward updates are refused. The PR review workflow can never push.
- **Runners hold the run token as a git header passed through environment-only git config** (`GIT_CONFIG_*`), never
  written to a file.

## Harnesses and models

- **Copilot CLI adapter** (verified against 1.0.36): BYOK via `COPILOT_PROVIDER_BASE_URL`/`_API_KEY`/`_TYPE=openai`
  and `COPILOT_MODEL`, `COPILOT_OFFLINE=true` (no GitHub sign-in or traffic), `-p … --allow-all-tools --no-ask-user
  --output-format=json`, `--resume=<uuid>` (creates the session with that id on the first turn and resumes it after),
  `COPILOT_HOME` inside the checkpointed session directory, `--additional-mcp-config=@file` for the `factory` MCP
  server and `--secret-env-vars` to strip the run token from the agent's shell. JSONL `assistant.message`,
  `tool.execution_start/complete`, `assistant.turn_start` map to the normalised events. Copilot reports output
  tokens only, so cost is metered at the plane's model endpoint, not from harness events.
- **Fake model provider** is served by the plane at `/fake-model/v1` and requires a fixed provider key, proving that
  the plane adds the provider key and the runner's run key is not forwarded.

## Phase 2 and 3 additions

- **LiteLLM keys.** One virtual key per run at run start (and again on revival), with `max_budget` = remaining
  budget, `models` = the run's model only, and a `duration` of twice the project's idle expiry plus a day. The
  plane deletes it at run end after reading `key/info` spend; the difference between LiteLLM's spend and what the
  plane tracked is recorded as a `litellm_reconciliation` usage entry, so per-run, per-model and per-project totals
  match LiteLLM. The key is stored encrypted on the run row. Creation is idempotent per run and issue count.
- **Harness-reported usage** is informational in direct mode (the plane meters at its endpoint, which also sees
  input tokens that Copilot does not report). With LiteLLM it is the running estimate until reconciliation.
- **Egress proxy** is a separate small binary (`egress`) sharing the plane crate. It is an HTTP forward proxy
  (CONNECT and absolute-form) checking the host against `settings.egress_allowlist` (an entry allows the domain
  and its subdomains; IP literals are not allowed unless listed), and a reverse proxy at `/feeds/<name>/` for
  private package feeds, which are `package_feed` connectors whose `auth_header` secret the proxy adds. It
  reloads its policy from the database every 5 s.
- **Entra.** Authorization-code flow; the ID token's issuer, audience, expiry and nonce are validated, its
  signature is not, because it is received directly from the token endpoint over TLS (OIDC Core 3.1.3.7). Groups
  map to roles through the `entra_group_roles` setting; Entra-sourced grants are replaced at every sign-in while
  manual grants stay. Group overage (more than 200 groups, `_claim_names`) is not handled yet.
- **Roles are ordered** view < operate < approve < administer; a global administrator has every role.
- **No-progress detection** counts a turn as progress when it pushed code or made a side-effecting primitive call
  other than status reports, messages and questions.
- **Fargate launcher** uses `startedBy` (derived from run and generation, ≤ 36 chars) for idempotency and one task
  definition per stack image; runner tasks have no task role, so they hold no AWS credentials.
- **Wire API per model connector.** `direct_model` and `litellm` connectors have `wire_api` = `completions`
  (default, any OpenAI-compatible endpoint) or `responses`. It travels to the runner in `ModelAccess.wire_api` and
  the Copilot adapter sets `COPILOT_PROVIDER_WIRE_API`. Newer OpenAI models refuse tools together with reasoning
  effort on `/chat/completions`; on `/responses` Copilot sends `reasoning.effort` and tools. The plane's endpoint
  proxies any path and reads Responses usage (`response.usage`), so metering is unchanged.
- **Two-phase ticket build and ad hoc runs.** A run starts in phase `plan` (`runs.phase`): the first turn gets only
  the plan part of the template (everything before `<!-- build phase -->`) and submits a plan with `plan.submit`,
  which stores it as a versioned `plan` artifact and opens an approval question with action `plan` (Slack buttons
  *Approve plan* / *Don't build*; any other thread reply is the changes wanted). Approval sets phase `build` and
  delivers the build part of the template with the reviewer's notes; requested changes come back as an answer and the
  agent resubmits (earlier open reviews are superseded); *Don't build* finishes the run with outcome `declined` and
  comments on the ticket. In the plan phase the push policy refuses the run's branch (checkpoints to the wip branch
  are still allowed), and with it `pr.open`. A plan-phase turn that ends without a plan gets one reminder, then the
  run pauses (`no_plan`); a plan phase that spends more than `plan_budget_share` of the run budget (default 25%) pauses
  (`plan_budget`). A project template without the marker gets the built-in build part. Security audit and PR review
  stay single-phase.
- **Chat users are linked by email on first use.** A Slack user without a linked identity is matched to the factory
  user with the same email (Slack `users.info`, scope `users:read.email`) and linked (audited as
  `identity.auto_linked`). Button presses and thread replies that still cannot be matched, or that are refused (role,
  already answered), get a reply in the run's thread saying why; they used to be dropped silently.
- **Ending the turn is enforced.** When a primitive that ends the turn returns (`human.ask`, `human.request_approval`,
  `status.blocked`, `plan.submit`), the plane sends the runner an interrupt, because models keep working regardless of
  the instruction to stop. An interrupt that arrives after the turn already ended is acknowledged and ignored.
- **A spent budget stops the turn.** When recorded spend reaches the run budget during a turn, the plane interrupts
  it instead of letting the harness wait on refused model calls until the turn limit; the cap decision follows.
- **Identical-call loop detection is off by default** (`stuck_repeat_threshold` = 0). It counted the calls of all
  sub-agents together, so fan-out looked like a loop, while real loops with varying arguments went unnoticed.
- **A failed turn pauses the run.** When the harness reports `failed` (model refused the request, CLI crashed),
  the same turn instruction is queued again as held, the run is paused (`stuck_detected` / `turn_failed`) and
  the assignee's run thread and ops get the error. Resuming runs the turn again. Before this, a failed turn left
  the run in `waiting · input` with no message.
- **Workspace setup failures pause the run.** A runner that starts but cannot set up its workspace (clone refused,
  bad credentials) would fail the same way on every relaunch, and launch attempts are reset by each runner hello.
  After 3 setup failures in a row (since the last turn started or the run was resumed) the run is paused with a
  `stuck_detected` event (`workspace_setup_failed`) and the assignee and ops are told the last error; resuming
  retries.
- **Slack Socket Mode** is a per-connector choice (`mode` = `http` | `socket`, default `http`). In Socket Mode the
  replica holding the singleton lock keeps one outbound WebSocket per such connector (`slack_socket`), reconciled
  every tick: new or changed connectors (config or secrets) reconnect, removed ones or ones switched back close, and
  all close when the lock is lost. Envelopes are queued as the webhook would queue them, then acknowledged, so a
  crash between the two means a redelivery that the usual event-id de-duplication absorbs. The rustls client config
  names aws-lc-rs explicitly because both ring and aws-lc-rs are linked.
- **Copilot CLI on Windows** is started through its node entry point rather than the npm batch shim, which cannot
  carry multi-line prompts. `FACTORY_COPILOT_BIN` may point at a binary or a `.js` entry point.
- **Fake services served by the plane.** `/fake-model/v1` (OpenAI-compatible) and `/fake-litellm` are always
  mounted (they are inert without configuration); `/fake-entra` and `/api/fake/...` only with the development
  sign-in enabled.
