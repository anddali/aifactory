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
