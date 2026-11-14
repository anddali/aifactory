//! Run orchestration: creation, runner lifecycle, inputs and turns, controls,
//! caps, expiry and teardown.

use crate::jobs::Job;
use crate::launcher::LaunchSpec;
use crate::runs::{self, Run};
use crate::settings::{self, ProjectConfig};
use crate::{AppState, events, humans};
use anyhow::{Context, Result, anyhow, bail};
use proto::{InputKind, Instruction, RunState, WorkflowKind};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

pub const POLICY_VERSION: &str = "policy-2026.11-1";

// ---------------------------------------------------------------------------
// Creation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NewRun {
    pub project_id: Uuid,
    pub workflow: String,
    #[serde(default)]
    pub ticket_key: Option<String>,
    /// Repository full name.
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub pr_number: Option<i64>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub brief: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub budget_usd: Option<f64>,
    #[serde(default)]
    pub max_turns: Option<i32>,
    #[serde(default)]
    pub harness: Option<String>,
    #[serde(default)]
    pub assignee_email: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CreateError {
    #[error("work item already has an active run {0}")]
    AlreadyActive(Uuid),
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub async fn project(state: &AppState, id: Uuid) -> Result<(String, Value, Value)> {
    sqlx::query_as("SELECT key, config, templates FROM projects WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| anyhow!("project {id} not found"))
}

pub async fn project_cfg(state: &AppState, id: Uuid) -> Result<ProjectConfig> {
    Ok(settings::project_config(&project(state, id).await?.1))
}

#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct RepoRow {
    pub id: Uuid,
    pub project_id: Uuid,
    pub connector_id: Uuid,
    pub full_name: String,
    pub default_branch: String,
    pub protected_branches: Vec<String>,
    pub stack: String,
    pub stack_profile: Value,
    pub image_override: Option<String>,
    pub monitor_prs: bool,
    pub runner_config: Option<Value>,
}

pub async fn repo(state: &AppState, id: Uuid) -> Result<RepoRow> {
    sqlx::query_as("SELECT id, project_id, connector_id, full_name, default_branch, protected_branches, stack, stack_profile, image_override, monitor_prs, runner_config FROM repositories WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| anyhow!("repository {id} not found"))
}

pub async fn repo_by_name(state: &AppState, project_id: Uuid, name: &str) -> Result<Option<RepoRow>> {
    Ok(sqlx::query_as("SELECT id, project_id, connector_id, full_name, default_branch, protected_branches, stack, stack_profile, image_override, monitor_prs, runner_config FROM repositories WHERE project_id=$1 AND full_name=$2")
        .bind(project_id)
        .bind(name)
        .fetch_optional(&state.pool)
        .await?)
}

pub async fn project_repos(state: &AppState, project_id: Uuid) -> Result<Vec<RepoRow>> {
    Ok(sqlx::query_as("SELECT id, project_id, connector_id, full_name, default_branch, protected_branches, stack, stack_profile, image_override, monitor_prs, runner_config FROM repositories WHERE project_id=$1 ORDER BY full_name")
        .bind(project_id)
        .fetch_all(&state.pool)
        .await?)
}

pub async fn user_by_email(state: &AppState, email: &str) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar("SELECT id FROM users WHERE email=$1")
        .bind(email.to_lowercase())
        .fetch_optional(&state.pool)
        .await?)
}

/// Create a run. Enforces the kill switch, the per-project limits and the
/// one-active-run-per-work-item constraint (in the database).
pub async fn create_run(state: &AppState, req: NewRun, actor: &str, actor_user: Option<Uuid>) -> Result<Run, CreateError> {
    let workflow = WorkflowKind::parse(&req.workflow).ok_or_else(|| CreateError::Refused(format!("unknown workflow {}", req.workflow)))?;
    let (pkey, pconf_v, _) = project(state, req.project_id).await?;
    let pconf = settings::project_config(&pconf_v);
    let global = settings::get(&state.pool).await?;
    if global.kill_switch {
        return Err(CreateError::Refused("the kill switch is on: no new runs".into()));
    }
    if !pconf.enabled_workflows.iter().any(|w| w == workflow.as_str()) {
        return Err(CreateError::Refused(format!("workflow {} is not enabled for project {pkey}", workflow.as_str())));
    }
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM runs WHERE project_id=$1 AND state NOT IN ('finished','failed','cancelled','expired')")
        .bind(req.project_id)
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    if active >= pconf.max_concurrent_runs {
        return Err(CreateError::Refused(format!("project {pkey} is at its limit of {} concurrent runs", pconf.max_concurrent_runs)));
    }
    let spent_today: f64 = sqlx::query_scalar("SELECT COALESCE(sum(cost_usd),0)::float8 FROM usage WHERE project_id=$1 AND ts > now() - interval '1 day'")
        .bind(req.project_id)
        .fetch_one(&state.pool)
        .await
        .map_err(anyhow::Error::from)?;
    if spent_today >= pconf.daily_spend_usd {
        return Err(CreateError::Refused(format!("project {pkey} has reached its daily spend limit")));
    }

    // Resolve the work item.
    let mut ticket = None;
    let mut repo_name = req.repo.clone();
    if workflow == WorkflowKind::TicketBuild {
        let key = req.ticket_key.clone().ok_or_else(|| CreateError::Refused("ticket_build needs ticket_key".into()))?;
        let tc = pconf.ticket_connector_id.ok_or_else(|| CreateError::Refused("project has no ticket source".into()))?;
        let t = crate::connectors::ticket_source(state, tc).await?.get(&key).await?;
        if repo_name.is_none() {
            repo_name = t.repo.clone();
        }
        ticket = Some(t);
    }
    let repos = project_repos(state, req.project_id).await?;
    let repo = match &repo_name {
        Some(n) => Some(
            repos
                .iter()
                .find(|r| &r.full_name == n)
                .cloned()
                .ok_or_else(|| CreateError::Refused(format!("repository {n} is not onboarded in project {pkey}")))?,
        ),
        None if repos.len() == 1 => Some(repos[0].clone()),
        None if matches!(workflow, WorkflowKind::AdHoc | WorkflowKind::SecurityAudit) => repos.first().cloned(),
        None => return Err(CreateError::Refused("which repository? none given and the project has several".into())),
    };
    if matches!(workflow, WorkflowKind::TicketBuild | WorkflowKind::PrReview) && repo.is_none() {
        return Err(CreateError::Refused("this workflow needs a repository".into()));
    }
    let mut pr_url = None;
    if workflow == WorkflowKind::PrReview {
        let n = req.pr_number.ok_or_else(|| CreateError::Refused("pr_review needs pr_number".into()))?;
        let r = repo.as_ref().unwrap();
        let host = crate::connectors::repo_host(state, r.connector_id).await?;
        let pr = host.get_pr(&r.full_name, n).await?;
        if pr.pr.state != "open" {
            return Err(CreateError::Refused(format!("PR {n} is {}", pr.pr.state)));
        }
        let own = crate::connectors::service_identity(state, r.connector_id).await.unwrap_or_else(|| "factory-bot".into());
        if pr.pr.author == own && !pconf.review_own_prs {
            return Err(CreateError::Refused("the factory does not review its own PRs".into()));
        }
        pr_url = Some(pr.pr.url.clone());
    }

    let work_item_key = match workflow {
        WorkflowKind::TicketBuild => Some(format!("ticket:{}:{}", pconf.ticket_connector_id.unwrap(), req.ticket_key.clone().unwrap())),
        WorkflowKind::PrReview => Some(format!("pr:{}:{}", repo.as_ref().unwrap().id, req.pr_number.unwrap())),
        _ => None,
    };
    let title = req
        .title
        .clone()
        .or_else(|| ticket.as_ref().map(|t| format!("{}: {}", t.key, t.title)))
        .or_else(|| req.pr_number.map(|n| format!("Review {}#{n}", repo_name.clone().unwrap_or_default())))
        .unwrap_or_else(|| "Ad hoc task".into());
    let assignee_email = req
        .assignee_email
        .clone()
        .or_else(|| ticket.as_ref().and_then(|t| t.assignee_email.clone()))
        .or_else(|| pconf.default_assignee_email.clone());
    let assignee = match &assignee_email {
        Some(e) => user_by_email(state, e).await?,
        None => None,
    }
    .or(actor_user);

    let (tpl_body, tpl_version) = crate::workflows::template(state, req.project_id, workflow).await?;
    let _ = tpl_body;
    let config_version = crate::crypto::sha256_hex(pconf_v.to_string().as_bytes())[..12].to_string();
    let id = Uuid::now_v7();
    let salt = hex::encode(crate::crypto::random_bytes(16));
    let review_round_limit = pconf.review_round_limit;
    let r = sqlx::query(
        "INSERT INTO runs(id, project_id, workflow, state, title, work_item_key, ticket_key, repo_id, pr_number, pr_url,
            base_branch, assignee_user_id, harness, model, budget_usd, max_turns, turn_timeout_secs, template_version,
            policy_version, config_version, brief, scope, data, created_by, token_salt)
         VALUES ($1,$2,$3,'queued',$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24)",
    )
    .bind(id)
    .bind(req.project_id)
    .bind(workflow.as_str())
    .bind(&title)
    .bind(&work_item_key)
    .bind(&req.ticket_key)
    .bind(repo.as_ref().map(|r| r.id))
    .bind(req.pr_number)
    .bind(&pr_url)
    .bind(repo.as_ref().map(|r| r.default_branch.clone()))
    .bind(assignee)
    .bind(req.harness.clone().unwrap_or(pconf.harness.clone()))
    .bind(req.model.clone().unwrap_or(pconf.default_model.clone()))
    .bind(req.budget_usd.unwrap_or(pconf.budget_usd))
    .bind(req.max_turns.unwrap_or(pconf.max_turns))
    .bind(pconf.turn_timeout_secs)
    .bind(&tpl_version)
    .bind(POLICY_VERSION)
    .bind(&config_version)
    .bind(&req.brief)
    .bind(&req.scope)
    .bind(json!({"review_round_limit": review_round_limit, "ticket": ticket}))
    .bind(actor_user)
    .bind(&salt)
    .execute(&state.pool)
    .await;
    if let Err(e) = r {
        if let Some(db) = e.as_database_error() {
            if db.constraint() == Some("runs_one_active_per_item") {
                let existing: Uuid = sqlx::query_scalar(
                    "SELECT id FROM runs WHERE work_item_key=$1 AND state NOT IN ('finished','failed','cancelled','expired')",
                )
                .bind(&work_item_key)
                .fetch_one(&state.pool)
                .await
                .map_err(anyhow::Error::from)?;
                return Err(CreateError::AlreadyActive(existing));
            }
        }
        return Err(CreateError::Other(e.into()));
    }
    events::append(&state.pool, &*state.blobs, id, "run_created", Some(actor), json!({
        "workflow": workflow.as_str(), "title": title, "ticket_key": req.ticket_key, "repo": repo_name,
        "pr_number": req.pr_number, "template_version": tpl_version, "policy_version": POLICY_VERSION,
        "config_version": config_version, "brief": req.brief, "scope": req.scope,
    }))
    .await?;
    crate::audit::record(&state.pool, actor, actor_user, "run.trigger", Some(req.project_id), Some(id), json!({"workflow": workflow.as_str(), "ticket_key": req.ticket_key, "pr_number": req.pr_number, "repo": repo_name})).await;
    state.jobs.push(Job::StartRun { run_id: id }, Some(format!("start:{id}"))).await?;
    Ok(runs::get(state, id).await?)
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

async fn start_run(state: &AppState, run_id: Uuid) -> Result<()> {
    let run = runs::get(state, run_id).await?;
    if run.state() != RunState::Queued {
        return Ok(());
    }
    let workflow = run.workflow();
    let mut branch = None;
    let mut image = None;
    if let Some(repo_id) = run.repo_id {
        let r = repo(state, repo_id).await?;
        // Re-check the stack at run start.
        let stack = match crate::stack::detect_for_repo(state, &r).await {
            Ok((stack, profile)) => {
                sqlx::query("UPDATE repositories SET stack=$2, stack_profile=$3 WHERE id=$1")
                    .bind(r.id)
                    .bind(&stack)
                    .bind(&profile)
                    .execute(&state.pool)
                    .await?;
                stack
            }
            Err(e) => {
                tracing::warn!("stack detection failed: {e:#}");
                r.stack.clone()
            }
        };
        image = Some(r.image_override.clone().unwrap_or(stack));
        if workflow != WorkflowKind::PrReview {
            branch = Some(run.namespace());
        }
    }
    let model_key = format!("fmk_{}", state.crypto.derive(&format!("model|{}|{}", run.id, run.token_salt)));
    sqlx::query("UPDATE runs SET branch=$2, runner_image=COALESCE($3, 'polyglot'), model_key_hash=$4 WHERE id=$1")
        .bind(run.id)
        .bind(&branch)
        .bind(&image)
        .bind(crate::crypto::sha256_hex(model_key.as_bytes()))
        .execute(&state.pool)
        .await?;
    crate::model_access::open(state, run.id).await?;
    let run = runs::get(state, run_id).await?;
    let (text, data) = crate::workflows::start_input(state, &run).await?;
    humans::ensure_thread(state, &run).await.ok();
    runs::transition(state, run.id, RunState::Starting, None, "run started", None).await?;
    deliver_input(state, &run, InputKind::Start, &text, data, &format!("start:{}", run.id), false).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Runner lifecycle
// ---------------------------------------------------------------------------

pub fn run_token(state: &AppState, run: &Run, generation: i64) -> String {
    format!("frt_{}", state.crypto.derive(&format!("run|{}|{generation}|{}", run.id, run.token_salt)))
}

/// Launch a runner unless one is alive or launching.
pub async fn ensure_runner(state: &AppState, run_id: Uuid) -> Result<()> {
    let row: Option<(i64,)> = sqlx::query_as(
        "UPDATE runs SET runner_generation = runner_generation + 1, runner_state = 'launching',
            runner_launched_at = now(), launch_attempts = launch_attempts + 1, runner_handle = NULL, last_heartbeat_at = NULL
         WHERE id=$1 AND runner_state NOT IN ('alive','launching')
           AND state NOT IN ('finished','failed','cancelled','expired','paused')
         RETURNING runner_generation",
    )
    .bind(run_id)
    .fetch_optional(&state.pool)
    .await?;
    if let Some((generation,)) = row {
        state
            .jobs
            .push(Job::Launch { run_id, generation }, Some(format!("launch:{run_id}:{generation}")))
            .await?;
    }
    Ok(())
}

async fn launch(state: &AppState, run_id: Uuid, generation: i64) -> Result<()> {
    let run = runs::get(state, run_id).await?;
    if run.runner_generation != generation || run.state().is_terminal() || run.runner_state != "launching" {
        return Ok(());
    }
    let s = settings::get(&state.pool).await?;
    if run.launch_attempts > s.max_launch_attempts {
        events::append(&state.pool, &*state.blobs, run.id, "runner_launch_failed", None, json!({"attempts": run.launch_attempts})).await?;
        fail_run(state, run.id, "runner could not be launched").await?;
        return Ok(());
    }
    let token = run_token(state, &run, generation);
    // The new token replaces the old one: an earlier runner generation can
    // no longer act for the run.
    sqlx::query("UPDATE runs SET run_token_hash=$2 WHERE id=$1 AND runner_generation=$3")
        .bind(run.id)
        .bind(crate::crypto::sha256_hex(token.as_bytes()))
        .bind(generation)
        .execute(&state.pool)
        .await?;
    let kind = s.launcher.clone();
    let launcher = state.launcher(&kind);
    let stack = run.runner_image.clone().unwrap_or_else(|| "polyglot".into());
    let image = match kind.as_str() {
        "docker" => crate::launcher::docker::DockerLauncher::from_env().image_for(&stack),
        _ => stack.clone(),
    };
    let mut env = vec![
        ("FACTORY_PLANE_URLS".to_string(), state.cfg.public_urls.join(",")),
        ("FACTORY_RUN_TOKEN".to_string(), token),
        ("FACTORY_RUN_ID".to_string(), run.id.to_string()),
        ("FACTORY_GENERATION".to_string(), generation.to_string()),
        ("FACTORY_HARNESS".to_string(), run.harness.clone()),
    ];
    if let Some(p) = &s.egress_proxy_url {
        for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            env.push((k.to_string(), p.clone()));
        }
        let no_proxy = state.cfg.public_urls.iter().filter_map(|u| url::Url::parse(u).ok()?.host_str().map(String::from)).collect::<Vec<_>>().join(",");
        env.push(("NO_PROXY".into(), no_proxy.clone()));
        env.push(("no_proxy".into(), no_proxy));
    }
    if let Ok(f) = std::env::var("FACTORY_TEST_RUNNER_FAULT") {
        env.push(("FACTORY_SCRIPTED_FAULT".into(), f));
    }
    let spec = LaunchSpec { run_id: run.id, generation, name: LaunchSpec::runner_name(&run.id, generation), image: image.clone(), env };
    let handle = launcher.launch(&spec).await.with_context(|| format!("launching runner via {kind}"))?;
    let updated = sqlx::query("UPDATE runs SET runner_handle=$3 WHERE id=$1 AND runner_generation=$2 AND runner_handle IS NULL")
        .bind(run.id)
        .bind(generation)
        .bind(format!("{kind}|{handle}"))
        .execute(&state.pool)
        .await?;
    if updated.rows_affected() > 0 {
        crate::metrics::RUNNERS.with_label_values(&[launcher.kind()]).inc();
        events::append(&state.pool, &*state.blobs, run.id, "runner_launched", None, json!({"generation": generation, "launcher": kind, "image": image, "handle": handle})).await?;
    }
    Ok(())
}

pub async fn remove_runner(state: &AppState, handle: Option<&str>) {
    let Some(h) = handle else { return };
    let (kind, inner) = h.split_once('|').unwrap_or(("process", h));
    if let Err(e) = state.launcher(kind).remove(inner).await {
        tracing::warn!("removing runner {h}: {e:#}");
    }
}

/// Called when a supervisor exits on its own (idle grace, stop instruction).
pub async fn runner_exited(state: &AppState, run: &Run, generation: i64, reason: &str) -> Result<()> {
    let r = sqlx::query("UPDATE runs SET runner_state='stopped', runner_busy=false WHERE id=$1 AND runner_generation=$2 AND runner_state IN ('alive','launching')")
        .bind(run.id)
        .bind(generation)
        .execute(&state.pool)
        .await?;
    if r.rows_affected() == 0 {
        return Ok(());
    }
    events::append(&state.pool, &*state.blobs, run.id, "runner_exited", None, json!({"generation": generation, "reason": reason})).await?;
    // Docker leaves an exited container; remove it.
    let handle = run.runner_handle.clone();
    let st = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        remove_runner(&st, handle.as_deref()).await;
    });
    reset_acked(state, run.id, generation + 1).await?;
    if has_pending(state, run.id).await? {
        ensure_runner(state, run.id).await?;
    }
    Ok(())
}

/// The runner stopped heart-beating: treat it as lost and recover from the
/// last checkpoint.
pub async fn runner_lost(state: &AppState, run: &Run) -> Result<()> {
    let r = sqlx::query("UPDATE runs SET runner_state='lost', runner_busy=false WHERE id=$1 AND runner_generation=$2 AND runner_state IN ('alive','launching')")
        .bind(run.id)
        .bind(run.runner_generation)
        .execute(&state.pool)
        .await?;
    if r.rows_affected() == 0 {
        return Ok(());
    }
    events::append(&state.pool, &*state.blobs, run.id, "runner_lost", None, json!({"generation": run.runner_generation, "last_heartbeat_at": run.last_heartbeat_at})).await?;
    remove_runner(state, run.runner_handle.as_deref()).await;
    reset_acked(state, run.id, run.runner_generation + 1).await?;
    if has_pending(state, run.id).await? || run.state() == RunState::Running || run.state() == RunState::Starting {
        ensure_runner(state, run.id).await?;
    }
    Ok(())
}

/// Instructions a dead generation took but did not finish go back to pending.
async fn reset_acked(state: &AppState, run_id: Uuid, below_generation: i64) -> Result<()> {
    sqlx::query("UPDATE instructions SET state='pending', acked_generation=NULL WHERE run_id=$1 AND state='acked' AND acked_generation < $2")
        .bind(run_id)
        .bind(below_generation)
        .execute(&state.pool)
        .await?;
    // Control instructions for a dead runner are meaningless now.
    sqlx::query("UPDATE instructions SET state='cancelled' WHERE run_id=$1 AND state='pending' AND body->>'type' IN ('interrupt','checkpoint','stop','teardown')")
        .bind(run_id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

pub async fn has_pending(state: &AppState, run_id: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM instructions WHERE run_id=$1 AND state IN ('pending','acked') AND body->>'type'='turn')")
        .bind(run_id)
        .fetch_one(&state.pool)
        .await?)
}

// ---------------------------------------------------------------------------
// Instructions and inputs
// ---------------------------------------------------------------------------

pub async fn add_instruction(state: &AppState, run_id: Uuid, ins: &Instruction, input_id: Option<Uuid>, held: bool) -> Result<i64> {
    let id: i64 = sqlx::query_scalar("INSERT INTO instructions(run_id, body, state, input_id) VALUES ($1,$2,$3,$4) RETURNING id")
        .bind(run_id)
        .bind(serde_json::to_value(ins)?)
        .bind(if held { "held" } else { "pending" })
        .bind(input_id)
        .fetch_one(&state.pool)
        .await?;
    if !held {
        crate::notify::notify(&state.pool, crate::notify::CH_INSTR, &run_id.to_string()).await;
    }
    Ok(id)
}

/// Deliver an input to a run: record it (de-duplicated), queue a turn and make
/// sure a runner is there to take it. `human` resets the idle clock.
pub async fn deliver_input(state: &AppState, run: &Run, kind: InputKind, text: &str, data: Value, dedupe_key: &str, human: bool) -> Result<Option<Uuid>> {
    let input_id = Uuid::now_v7();
    let inserted = sqlx::query("INSERT INTO inputs(id, run_id, dedupe_key, kind, text, data) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (dedupe_key) DO NOTHING")
        .bind(input_id)
        .bind(run.id)
        .bind(dedupe_key)
        .bind(kind.as_str())
        .bind(text)
        .bind(&data)
        .execute(&state.pool)
        .await?;
    if inserted.rows_affected() == 0 {
        return Ok(None);
    }
    if human {
        touch_activity(state, run.id).await?;
    }
    let held = run.state() == RunState::Paused || run.waiting_for.as_deref() == Some("cap_decision") && kind != InputKind::CapDecision;
    let ins = Instruction::Turn { input_id, kind, text: text.to_string(), data: data.clone() };
    add_instruction(state, run.id, &ins, Some(input_id), held).await?;
    events::append(&state.pool, &*state.blobs, run.id, "input_received", None, json!({"input_id": input_id, "kind": kind.as_str(), "text": text, "data": data, "held": held})).await?;
    if !held {
        ensure_runner(state, run.id).await?;
    }
    Ok(Some(input_id))
}

pub async fn touch_activity(state: &AppState, run_id: Uuid) -> Result<()> {
    sqlx::query("UPDATE runs SET last_activity_at=now(), reminder_sent_at=NULL, warning_sent_at=NULL WHERE id=$1")
        .bind(run_id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

async fn release_held(state: &AppState, run_id: Uuid) -> Result<()> {
    let n = sqlx::query("UPDATE instructions SET state='pending' WHERE run_id=$1 AND state='held'")
        .bind(run_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if n > 0 {
        crate::notify::notify(&state.pool, crate::notify::CH_INSTR, &run_id.to_string()).await;
    }
    Ok(())
}

/// A supervisor took an instruction.
pub async fn instruction_acked(state: &AppState, run: &Run, generation: i64, id: i64) -> Result<()> {
    let body: Option<Value> = sqlx::query_scalar("UPDATE instructions SET state='acked', acked_generation=$3 WHERE id=$1 AND run_id=$2 AND state='pending' RETURNING body")
        .bind(id)
        .bind(run.id)
        .bind(generation)
        .fetch_optional(&state.pool)
        .await?;
    let Some(body) = body else { return Ok(()) };
    let ins: Instruction = serde_json::from_value(body)?;
    if let Instruction::Turn { input_id, kind, .. } = ins {
        sqlx::query("UPDATE runs SET turn_started_at=now(), current_instruction=$2, runner_busy=true WHERE id=$1")
            .bind(run.id)
            .bind(id)
            .execute(&state.pool)
            .await?;
        runs::transition(state, run.id, RunState::Running, None, &format!("turn started ({})", kind.as_str()), None).await?;
        events::append(&state.pool, &*state.blobs, run.id, "turn_started", None, json!({"instruction_id": id, "input_id": input_id, "kind": kind.as_str(), "generation": generation})).await?;
    } else {
        sqlx::query("UPDATE instructions SET state='done', done_at=now() WHERE id=$1").bind(id).execute(&state.pool).await?;
    }
    Ok(())
}

/// A turn ended. Decide what the run waits for next.
pub async fn turn_completed(state: &AppState, run: &Run, tc: &proto::TurnComplete) -> Result<()> {
    let done = sqlx::query("UPDATE instructions SET state='done', done_at=now() WHERE id=$1 AND run_id=$2 AND state='acked'")
        .bind(tc.instruction_id)
        .bind(run.id)
        .execute(&state.pool)
        .await?;
    if done.rows_affected() == 0 {
        return Ok(());
    }
    sqlx::query("UPDATE runs SET turns_used = turns_used + 1, runner_busy=false, turn_started_at=NULL, current_instruction=NULL WHERE id=$1")
        .bind(run.id)
        .execute(&state.pool)
        .await?;
    events::append(&state.pool, &*state.blobs, run.id, "turn_ended", None, json!({"instruction_id": tc.instruction_id, "input_id": tc.input_id, "outcome": tc.outcome, "summary": tc.summary})).await?;
    let run = runs::get(state, run.id).await?;
    crate::singleton::detect_after_turn(state, &run, tc).await?;
    let run = runs::get(state, run.id).await?;
    evaluate(state, &run).await
}

/// Work out the waiting state of a run between turns.
pub async fn evaluate(state: &AppState, run: &Run) -> Result<()> {
    if run.state().is_terminal() || run.state() == RunState::Paused {
        return Ok(());
    }
    if run.runner_busy {
        return Ok(());
    }
    let workflow = run.workflow();
    // Caps: stop once, report once, wait for a human.
    let over_turns = run.turns_used >= run.max_turns;
    let over_budget = run.spent_usd >= run.budget_usd;
    if over_turns || over_budget {
        let which = if over_turns { "turn cap" } else { "budget cap" };
        sqlx::query("UPDATE instructions SET state='held' WHERE run_id=$1 AND state='pending' AND body->>'type'='turn'")
            .bind(run.id)
            .execute(&state.pool)
            .await?;
        runs::transition(state, run.id, RunState::Waiting, Some("cap_decision"), which, None).await?;
        let first = sqlx::query("UPDATE runs SET cap_notified=true WHERE id=$1 AND NOT cap_notified")
            .bind(run.id)
            .execute(&state.pool)
            .await?
            .rows_affected()
            > 0;
        if first {
            events::append(&state.pool, &*state.blobs, run.id, "cap_reached", None, json!({"cap": which, "turns_used": run.turns_used, "max_turns": run.max_turns, "spent_usd": run.spent_usd, "budget_usd": run.budget_usd})).await?;
            humans::notify(state, run, &format!("cap:{}:{}:{}", run.id, run.max_turns, run.budget_usd), &format!(
                "This run hit its {which} ({} of {} turns, ${:.2} of ${:.2}). It has stopped. Raise the cap or end the run.",
                run.turns_used, run.max_turns, run.spent_usd, run.budget_usd
            )).await?;
        }
        return Ok(());
    }
    if has_pending(state, run.id).await? {
        return Ok(());
    }
    let open: Option<String> = sqlx::query_scalar("SELECT kind FROM questions WHERE run_id=$1 AND state='open' ORDER BY created_at DESC LIMIT 1")
        .bind(run.id)
        .fetch_optional(&state.pool)
        .await?;
    if let Some(kind) = open {
        let w = if kind == "approval" { "approval" } else { "human_answer" };
        runs::transition(state, run.id, RunState::Waiting, Some(w), "waiting on a human", None).await?;
        return Ok(());
    }
    if let Some(reason) = &run.blocked_reason {
        runs::transition(state, run.id, RunState::Waiting, Some("human_answer"), &format!("blocked: {reason}"), None).await?;
        return Ok(());
    }
    match workflow {
        WorkflowKind::AdHoc | WorkflowKind::SecurityAudit if run.finish_declared => {
            finish(state, run.id, "completed", "the agent declared the work finished").await?;
        }
        WorkflowKind::TicketBuild if run.pr_number.is_some() => {
            runs::transition(state, run.id, RunState::Waiting, Some("review"), "waiting for review", None).await?;
        }
        WorkflowKind::PrReview => {
            runs::transition(state, run.id, RunState::Waiting, Some("review"), "waiting for the author", None).await?;
        }
        _ => {
            runs::transition(state, run.id, RunState::Waiting, Some("input"), "turn ended", None).await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

pub async fn steer(state: &AppState, run: &Run, text: &str, interrupt: bool, actor: &str, user: Option<Uuid>) -> Result<()> {
    if run.state().is_terminal() {
        bail!("run is {}", run.state);
    }
    events::append(&state.pool, &*state.blobs, run.id, "steering", Some(actor), json!({"text": text, "interrupt": interrupt})).await?;
    crate::audit::record(&state.pool, actor, user, "run.steer", Some(run.project_id), Some(run.id), json!({"text": text, "interrupt": interrupt})).await;
    if interrupt && run.runner_busy {
        add_instruction(state, run.id, &Instruction::Interrupt, None, false).await?;
    }
    deliver_input(state, run, InputKind::Steer, text, json!({"from": actor}), &format!("steer:{}", Uuid::now_v7()), true).await?;
    Ok(())
}

pub async fn pause(state: &AppState, run: &Run, reason: &str, actor: &str, user: Option<Uuid>) -> Result<()> {
    if runs::transition(state, run.id, RunState::Paused, None, reason, Some(actor)).await?.is_none() {
        return Ok(());
    }
    sqlx::query("UPDATE instructions SET state='held' WHERE run_id=$1 AND state='pending' AND body->>'type'='turn'")
        .bind(run.id)
        .execute(&state.pool)
        .await?;
    if run.runner_busy {
        add_instruction(state, run.id, &Instruction::Interrupt, None, false).await?;
    }
    add_instruction(state, run.id, &Instruction::Stop { checkpoint: true }, None, false).await?;
    crate::audit::record(&state.pool, actor, user, "run.pause", Some(run.project_id), Some(run.id), json!({"reason": reason})).await;
    Ok(())
}

pub async fn resume(state: &AppState, run: &Run, actor: &str, user: Option<Uuid>) -> Result<()> {
    if run.state() != RunState::Paused {
        bail!("run is not paused");
    }
    if settings::get(&state.pool).await?.kill_switch {
        bail!("the kill switch is on");
    }
    runs::transition(state, run.id, RunState::Waiting, Some("input"), "resumed", Some(actor)).await?;
    touch_activity(state, run.id).await?;
    release_held(state, run.id).await?;
    crate::audit::record(&state.pool, actor, user, "run.resume", Some(run.project_id), Some(run.id), json!({})).await;
    let run = runs::get(state, run.id).await?;
    if has_pending(state, run.id).await? {
        ensure_runner(state, run.id).await?;
    } else {
        evaluate(state, &run).await?;
    }
    Ok(())
}

pub async fn cancel(state: &AppState, run: &Run, reason: &str, actor: &str, user: Option<Uuid>) -> Result<()> {
    if runs::transition(state, run.id, RunState::Cancelled, None, reason, Some(actor)).await?.is_none() {
        return Ok(());
    }
    crate::audit::record(&state.pool, actor, user, "run.cancel", Some(run.project_id), Some(run.id), json!({"reason": reason})).await;
    sqlx::query("UPDATE runs SET outcome=$2 WHERE id=$1").bind(run.id).bind(reason).execute(&state.pool).await?;
    humans::notify(state, run, &format!("cancelled:{}", run.id), &format!("Run cancelled: {reason}.")).await?;
    state.jobs.push(Job::Teardown { run_id: run.id, reason: reason.into() }, Some(format!("teardown:{}", run.id))).await?;
    Ok(())
}

pub async fn fail_run(state: &AppState, run_id: Uuid, reason: &str) -> Result<()> {
    if runs::transition(state, run_id, RunState::Failed, None, reason, None).await?.is_some() {
        sqlx::query("UPDATE runs SET outcome=$2 WHERE id=$1").bind(run_id).bind(reason).execute(&state.pool).await?;
        if let Ok(run) = runs::get(state, run_id).await {
            humans::notify(state, &run, &format!("failed:{run_id}"), &format!("Run failed: {reason}.")).await?;
        }
        state.jobs.push(Job::Teardown { run_id, reason: reason.into() }, Some(format!("teardown:{run_id}"))).await?;
    }
    Ok(())
}

/// The run reached its end state.
pub async fn finish(state: &AppState, run_id: Uuid, outcome: &str, summary: &str) -> Result<()> {
    if runs::transition(state, run_id, RunState::Finished, None, outcome, None).await?.is_none() {
        return Ok(());
    }
    sqlx::query("UPDATE runs SET outcome=$2 WHERE id=$1").bind(run_id).bind(outcome).execute(&state.pool).await?;
    let run = runs::get(state, run_id).await?;
    humans::notify(state, &run, &format!("finished:{run_id}"), &format!("Run finished ({outcome}): {summary}")).await?;
    state.jobs.push(Job::Teardown { run_id, reason: outcome.into() }, Some(format!("teardown:{run_id}"))).await?;
    Ok(())
}

/// Stop the runner and destroy its workspace. Used at end of run and for the
/// operator's "tear down" control (where the run itself carries on).
pub async fn teardown(state: &AppState, run_id: Uuid, reason: &str) -> Result<()> {
    let run = runs::get(state, run_id).await?;
    let terminal = run.state().is_terminal();
    if run.runner_state == "alive" || run.runner_state == "launching" {
        add_instruction(state, run.id, &Instruction::Teardown, None, false).await?;
        // Give the supervisor a moment to exit cleanly, then remove.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    remove_runner(state, run.runner_handle.as_deref()).await;
    sqlx::query("UPDATE runs SET runner_state='removed', runner_busy=false, run_token_hash = CASE WHEN $2 THEN NULL ELSE run_token_hash END WHERE id=$1")
        .bind(run.id)
        .bind(terminal)
        .execute(&state.pool)
        .await?;
    if terminal {
        sqlx::query("UPDATE instructions SET state='cancelled' WHERE run_id=$1 AND state IN ('pending','held','acked')")
            .bind(run.id)
            .execute(&state.pool)
            .await?;
        state.jobs.push(Job::CloseModelAccess { run_id }, Some(format!("close-model:{run_id}"))).await?;
    } else {
        reset_acked(state, run.id, i64::MAX).await?;
    }
    events::append(&state.pool, &*state.blobs, run.id, "runner_removed", None, json!({"reason": reason})).await?;
    Ok(())
}

pub async fn operator_teardown(state: &AppState, run: &Run, actor: &str, user: Option<Uuid>) -> Result<()> {
    crate::audit::record(&state.pool, actor, user, "run.teardown", Some(run.project_id), Some(run.id), json!({})).await;
    teardown(state, run.id, "operator tear down").await
}

pub async fn expire(state: &AppState, run: &Run) -> Result<()> {
    if runs::transition(state, run.id, RunState::Expired, None, "idle expiry", None).await?.is_none() {
        return Ok(());
    }
    events::append(&state.pool, &*state.blobs, run.id, "expired", None, json!({"idle_since": run.last_activity_at})).await?;
    let summary = crate::workflows::handoff_summary(state, run).await.unwrap_or_default();
    let text = format!(
        "This run expired after being idle since {}. The branch and PR are left in place. An operator can revive it from the portal or Slack within the retention period.\n\n{summary}",
        run.last_activity_at.format("%Y-%m-%d %H:%M UTC")
    );
    humans::notify(state, run, &format!("expired:{}", run.id), &text).await?;
    state.jobs.push(Job::ExternalComment { run_id: run.id, key: format!("expired-comment:{}", run.id), text }, Some(format!("expired-comment:{}", run.id))).await?;
    // Tear the runner down but keep the checkpoint for revival.
    let st = state.clone();
    let id = run.id;
    state.jobs.push(Job::Teardown { run_id: id, reason: "expired".into() }, Some(format!("teardown-expired:{id}:{}", run.event_seq))).await?;
    let _ = st;
    Ok(())
}

pub async fn revive(state: &AppState, run: &Run, actor: &str, user: Option<Uuid>) -> Result<()> {
    if run.state() != RunState::Expired {
        bail!("only expired runs can be revived");
    }
    let pc = project_cfg(state, run.project_id).await?;
    let expired_at = run.expired_at.unwrap_or(run.updated_at);
    if chrono::Utc::now() - expired_at > chrono::Duration::days(pc.checkpoint_retention_days) {
        bail!("the checkpoint retention period has passed");
    }
    if run.data.get("checkpoint_purged").and_then(|v| v.as_bool()) == Some(true) {
        bail!("the checkpoint was purged");
    }
    // Another run may have taken the work item meanwhile.
    if let Some(k) = &run.work_item_key {
        let other: Option<Uuid> = sqlx::query_scalar("SELECT id FROM runs WHERE work_item_key=$1 AND id<>$2 AND state NOT IN ('finished','failed','cancelled','expired')")
            .bind(k)
            .bind(run.id)
            .fetch_optional(&state.pool)
            .await?;
        if let Some(o) = other {
            bail!("work item already has active run {o}");
        }
    }
    runs::transition(state, run.id, RunState::Waiting, Some("input"), "revived", Some(actor)).await?;
    // A fresh token salt generation: re-enable the run token at next launch.
    sqlx::query("UPDATE runs SET runner_state='none', cap_notified=false WHERE id=$1").bind(run.id).execute(&state.pool).await?;
    touch_activity(state, run.id).await?;
    crate::model_access::open(state, run.id).await?;
    crate::audit::record(&state.pool, actor, user, "run.revive", Some(run.project_id), Some(run.id), json!({})).await;
    let run = runs::get(state, run.id).await?;
    deliver_input(state, &run, InputKind::Other, &format!("The run was revived by {actor}. Continue from where you left off."), json!({"revived": true}), &format!("revive:{}:{}", run.id, run.event_seq), true).await?;
    Ok(())
}

/// A human decision after a cap was hit.
pub async fn cap_decision(state: &AppState, run: &Run, action: &str, max_turns: Option<i32>, budget: Option<f64>, review_rounds: Option<i32>, actor: &str, user: Option<Uuid>) -> Result<()> {
    if run.waiting_for.as_deref() != Some("cap_decision") {
        bail!("run is not waiting for a cap decision");
    }
    crate::audit::record(&state.pool, actor, user, "run.cap_decision", Some(run.project_id), Some(run.id), json!({"action": action, "max_turns": max_turns, "budget_usd": budget, "review_rounds": review_rounds})).await;
    match action {
        "end" => finish(state, run.id, "ended_at_cap", &format!("ended by {actor} at a cap")).await,
        "raise" => {
            let new_turns = max_turns.unwrap_or(run.max_turns.max(run.turns_used) + 10);
            let new_budget = budget.unwrap_or(run.budget_usd.max(run.spent_usd) * 1.5 + 1.0);
            let mut data = run.data.clone();
            let limit = data.get("review_round_limit").and_then(|v| v.as_i64()).unwrap_or(5) as i32;
            data["review_round_limit"] = json!(review_rounds.unwrap_or(limit.max(run.review_rounds) + limit.max(1)));
            sqlx::query("UPDATE runs SET max_turns=$2, budget_usd=$3, cap_notified=false, data=$4 WHERE id=$1")
                .bind(run.id)
                .bind(new_turns)
                .bind(new_budget)
                .bind(&data)
                .execute(&state.pool)
                .await?;
            crate::model_access::update_budget(state, run.id, new_budget).await.ok();
            runs::transition(state, run.id, RunState::Waiting, Some("input"), "cap raised", Some(actor)).await?;
            release_held(state, run.id).await?;
            let run = runs::get(state, run.id).await?;
            if has_pending(state, run.id).await? {
                ensure_runner(state, run.id).await?;
            } else {
                deliver_input(state, &run, InputKind::CapDecision, &format!("{actor} raised the cap. Continue."), json!({"max_turns": new_turns, "budget_usd": new_budget}), &format!("cap:{}:{}", run.id, run.event_seq), true).await?;
            }
            Ok(())
        }
        a => bail!("unknown cap action {a}"),
    }
}

/// Kill switch: pause every active run.
pub async fn apply_kill_switch(state: &AppState, actor: &str, user: Option<Uuid>) -> Result<()> {
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM runs WHERE state IN ('queued','starting','running','waiting')")
        .fetch_all(&state.pool)
        .await?;
    for id in ids {
        if let Ok(run) = runs::get(state, id).await {
            pause(state, &run, "kill_switch", actor, user).await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Job dispatch
// ---------------------------------------------------------------------------

pub async fn run_job(state: &AppState, job: Job) -> Result<()> {
    match job {
        Job::StartRun { run_id } => start_run(state, run_id).await,
        Job::Launch { run_id, generation } => launch(state, run_id, generation).await,
        Job::Teardown { run_id, reason } => teardown(state, run_id, &reason).await,
        Job::ChatPost { run_id, key, text, question_id, ops } => humans::chat_post(state, run_id, &key, &text, question_id, ops).await,
        Job::ChatUpdate { question_id, text } => humans::chat_update(state, question_id, &text).await,
        Job::ExternalComment { run_id, key, text } => humans::external_comment(state, run_id, &key, &text).await,
        Job::RepoEvent { connector_id, event } => humans::handle_repo_event(state, connector_id, event).await,
        Job::TicketEvent { connector_id, event } => humans::handle_ticket_event(state, connector_id, event).await,
        Job::ChatEvent { connector_id, event } => humans::handle_chat_event(state, connector_id, event).await,
        Job::CloseModelAccess { run_id } => crate::model_access::close(state, run_id).await,
    }
}
