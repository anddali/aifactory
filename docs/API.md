# Plane HTTP API (portal-facing)

All portal endpoints are JSON, authenticated by the `factory_session` cookie
set at sign-in. Errors are `{"error": "message"}` with a 4xx/5xx status.
Timestamps are RFC 3339 strings. IDs are UUID strings unless noted.

## Auth

| Method | Path | Body / notes |
|---|---|---|
| GET | `/auth/config` | → `{"dev_login": bool, "entra": bool}` (no auth needed) |
| POST | `/auth/dev-login` | `{"email": "a@b.c", "name"?: "..."}` → `User`. Only when dev login is enabled. |
| GET | `/auth/entra/login` | Redirects to Entra. |
| POST | `/auth/logout` | → `{}` |
| GET | `/auth/me` | → `User` or 401 |

`User` = `{"id", "email", "display_name", "is_global_admin": bool, "grants": [{"project_id": uuid|null, "role": "view"|"operate"|"approve"|"administer"}]}`

## Runs

`RunSummary` =
```json
{"id","title","project_id","project_key","workflow":"ticket_build|pr_review|ad_hoc|security_audit",
 "state":"queued|starting|running|waiting|paused|finished|failed|cancelled|expired",
 "waiting_for": "human_answer|approval|review|ci|cap_decision|input"|null,
 "pause_reason": string|null, "spent_usd": number, "budget_usd": number,
 "turns_used": int, "max_turns": int, "needs_you": bool,
 "ticket_key": string|null, "repo": string|null, "pr_number": int|null, "pr_url": string|null,
 "branch": string|null, "assignee": {"id","email","display_name"}|null,
 "model": string, "harness": string, "phase": "plan"|"build"|null, "created_at", "updated_at", "last_activity_at"}
```

| Method | Path | Notes |
|---|---|---|
| GET | `/api/runs?state=active\|done\|all&project_id=&limit=100&status=&workflow=&harness=&model=&repo=&assignee=&q=` | → `{"runs": [RunSummary]}` newest first, only projects the caller can see. `active` = non-terminal. Optional filters: `status` exact run states, comma-separated; `workflow`, `harness`, `model`; `repo` full name; `assignee` a user id or `me`; `q` case-insensitive text matched against title, ticket key, branch and run id, or an exact PR number (`#12` or `12`). |
| GET | `/api/runs/facets` | → `{"models":[string],"harnesses":[string],"repos":[string],"assignees":[{"id","email","display_name"}]}`: values present in the runs the caller can see, for filter pickers. |
| POST | `/api/runs` | `{"project_id", "workflow", "ticket_key"?, "repo"? (full name), "pr_number"?, "title"?, "brief"?, "scope"?, "model"?, "budget_usd"?, "max_turns"?}` → `RunSummary` |
| GET | `/api/runs/{id}` | → `RunDetail` = RunSummary + `{"artifacts":[{"id","kind","title","url","content","created_at"}], "questions":[Question], "cost_by_model":[{"model","input_tokens","output_tokens","cost_usd"}], "event_count": int, "chat_thread": object|null, "template_version", "policy_version", "config_version", "runner": {"state","generation","handle","last_heartbeat_at"}}` |
| GET | `/api/runs/{id}/events?after=SEQ&before=SEQ&limit=200` | → `{"events":[Event], "total": int, "max_seq": int}`. Without `after`/`before` returns the newest `limit` events. Ascending by seq. |
| GET | `/api/runs/{id}/stream` | Server-sent events. Each message `data:` is an `Event` JSON. Event name `event`. Also sends `state` events with a `RunSummary`. |
| POST | `/api/runs/{id}/steer` | `{"text", "interrupt": bool}` |
| POST | `/api/runs/{id}/pause` | `{}` |
| POST | `/api/runs/{id}/resume` | `{}` |
| POST | `/api/runs/{id}/cancel` | `{}` |
| POST | `/api/runs/{id}/revive` | `{}` (expired runs inside retention) |
| POST | `/api/runs/{id}/teardown` | `{}` stops the runner container now (run keeps its state) |
| POST | `/api/runs/{id}/cap` | `{"action":"raise"\|"end", "max_turns"?, "budget_usd"?}` |
| POST | `/api/questions/{id}/answer` | `{"answer"?: string, "decision"?: "approved"\|"rejected"\|"changes"}` → Question. For a plan review (`action: "plan"`): `approved` starts the build phase, `changes` (with the changes as `answer`; assignee or operator) sends them back for a revised plan, `rejected` ends the run with outcome `declined`. |

`Event` = `{"seq": int, "ts", "kind": string, "actor": string|null, "payload": object}`.
Common kinds: `run_created`, `state_changed` (`{from,to,waiting_for,reason}`), `input_received`, `turn_started`, `turn_ended`,
`harness.model_turn`, `harness.tool_call` (`{call_id,name,args}`), `harness.tool_result`, `harness.agent_message` (`{text}`),
`harness.usage`, `harness.error`, `primitive_call` (`{name,args,ok,result|error,replayed}`), `human_message` (`{text, direction:"to_human"|"from_human", user}`),
`question_asked` (`{question_id, kind, text}`), `question_answered`, `steering`, `runner_launched`, `runner_exited`, `runner_lost`,
`checkpoint_saved`, `cost`, `reminder_sent`, `warning_sent`, `expired`, `artifact`, `status_report`, `cap_reached`, `stuck_detected`,
`plan_submitted` (`{version, artifact_id, summary}`), `plan_approved`, `plan_changes_requested`, `plan_declined` (`{question_id, note}`),
`plan_missing` (`{action: "reminded"|"paused"}`), `plan_budget_reached`, `turn_end_enforced` (`{after}`).

`Question` = `{"id","run_id","kind":"question"|"approval","text","action":string|null,"pr_number":int|null,"state":"open"|"answered","answer":string|null,"decision":"approved"|"rejected"|"changes"|"superseded"|null,"answered_by":string|null,"answered_via":"slack"|"portal"|null,"created_at","answered_at"}`

## Configuration (administer role)

| Method | Path | Notes |
|---|---|---|
| GET | `/api/connectors` | → `{"connectors":[Connector]}` |
| POST | `/api/connectors` | `{"kind","name","config":{}}` → Connector |
| PUT | `/api/connectors/{id}` | `{"name"?, "config"?}` |
| DELETE | `/api/connectors/{id}` | |
| PUT | `/api/connectors/{id}/secrets` | `{"secrets": {"name": "value"}}` – values are write-only |
| POST | `/api/connectors/{id}/test` | → `{"ok": bool, "message": string}` |
| POST | `/api/connectors/test` | `{"id"?, "kind", "config", "secrets"}`: tests unsaved settings without writing them; blank secrets fall back to the saved ones when `id` is given → `{"ok", "message"}` |
| GET | `/api/connectors/{id}/repos` | repo host connectors → `{"repos":[{"full_name","default_branch"}]}` |
| GET | `/api/connector-kinds` | → `{"kinds":[{"kind","label","role","description","config_fields":[{"name","label","type","required","placeholder"?,"help"?,"options"?:[{"value","label"}],"default"?,"show_if"?:{"field","equals"}}],"secret_fields":[...],"webhook_url","webhook_show_if"}]}` (`webhook_url` has `{id}` for the connector id, null when the kind takes no webhooks; `type` `select` comes with `options` and a `default`; a field with `show_if` is shown and required only while that config field, or its default, equals the value; `webhook_show_if` likewise for the webhook) |

`Connector` = `{"id","kind","role":"ticket|repo|chat|model","name","config":{},"secrets":[{"name","hint":"…abcd","updated_at"}],"service_identity":string|null,"created_at"}`

Kinds: `fake_tickets`, `jira`, `fake_repo`, `github`, `azure_devops`, `fake_chat`, `slack`, `fake_model`, `direct_model`, `litellm`.

| Method | Path | Notes |
|---|---|---|
| GET | `/api/projects` | → `{"projects":[Project]}` |
| POST | `/api/projects` | `{"key","name","config"?}` → Project |
| GET | `/api/projects/{id}` | → Project + `{"repos":[Repo]}` |
| PUT | `/api/projects/{id}` | `{"name"?, "config"?}` (config is merged) |
| GET | `/api/projects/{id}/repos` | → `{"repos":[Repo]}` |
| POST | `/api/projects/{id}/repos` | `{"connector_id","full_name"}` onboard → Repo |
| PUT | `/api/repos/{id}` | `{"image_override"?: string|null, "monitor_prs"?: bool}` |
| DELETE | `/api/repos/{id}` | |
| GET | `/api/templates` | → `{"templates":[{"workflow","version","body"}]}` built-in |
| PUT | `/api/projects/{id}/templates/{workflow}` | `{"body"}` override (empty body removes override) |

`Project.config` fields (all optional, defaults shown):
```json
{"enabled_workflows":["ticket_build","pr_review","ad_hoc","security_audit"],
 "ticket_connector_id":null,"chat_connector_id":null,"chat_channel":"#factory",
 "default_model":"gpt-4.1","harness":"scripted","budget_usd":10,"max_turns":30,"turn_timeout_secs":1800,
 "idle_reminder_secs":86400,"idle_warning_secs":172800,"idle_expiry_secs":259200,
 "checkpoint_retention_days":14,"idle_grace_secs":1200,"max_concurrent_runs":5,"daily_spend_usd":100,
 "review_round_limit":5,"merge_by_agent":false,"fallback_contact":null,"ops_channel":null,
 "jira_trigger":{"label":"factory","transition":null},"review_own_prs":false,"default_assignee_email":null}
```

`Repo` = `{"id","project_id","connector_id","full_name","default_branch","protected_branches":[...],"stack":"python|dotnet|node|rust|polyglot","stack_profile":{},"image_override":string|null,"monitor_prs":bool,"runner_config":object|null}`

| Method | Path | Notes |
|---|---|---|
| GET | `/api/users` | → `{"users":[User + {"identities":[{"provider","external_id"}]}]}` |
| POST | `/api/users` | `{"email","display_name"?,"is_global_admin"?}` |
| PUT | `/api/users/{id}/identities` | `{"identities":[{"provider":"jira|github|azure_devops|slack|entra","external_id"}]}` |
| PUT | `/api/users/{id}/grants` | `{"grants":[{"project_id"|null,"role"}]}` |
| GET | `/api/settings` | → `Settings` |
| PUT | `/api/settings` | partial `Settings` (merged) |
| POST | `/api/settings/egress-test` | `{"url"?: string}` (default: the saved `egress_proxy_url`) → `{"ok","proxy","allowed":{"host","ok","detail"},"blocked":{"host","ok","detail"}}`. From the plane, through the proxy: the first allowlisted host must answer and a host not on the allowlist must be refused by the proxy. Nothing is saved. Admin only. |
| GET | `/api/audit?run_id=&limit=200` | → `{"entries":[{"id","ts","actor","action","project_id","run_id","details"}]}` |
| GET | `/api/metrics/outcomes` | → `{"rows":[{"project_key","model","harness","runs","approved_prs","approval_rate","avg_review_rounds","avg_hours_to_approval","cost_per_approved_pr"}]}` |

`Settings` =
```json
{"kill_switch": false, "model_connector_id": null,
 "model_prices": {"gpt-4.1": {"input_per_mtok": 2.0, "output_per_mtok": 8.0}},
 "egress_allowlist": ["pypi.org","files.pythonhosted.org","registry.npmjs.org","crates.io","static.crates.io","index.crates.io","api.nuget.org"],
 "deny_patterns": ["rm -rf /", "mkfs", ":(){"],
 "event_retention_days": 90, "tick_ms": 15000, "heartbeat_secs": 10,
 "stuck_repeat_threshold": 0, "stuck_noprogress_turns": 4, "ops_channel": null,
 "launcher": "process|docker|fargate"}
```

## Health

`GET /healthz` (liveness), `GET /readyz` (readiness: DB reachable and migrations done), `GET /metrics` (Prometheus).
