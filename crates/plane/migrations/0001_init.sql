-- AI Factory plane schema.

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id UUID PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL DEFAULT '',
    is_global_admin BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE identities (
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    external_id TEXT NOT NULL,
    PRIMARY KEY (provider, external_id)
);
CREATE INDEX identities_user ON identities(user_id);

CREATE TABLE role_grants (
    id BIGSERIAL PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id UUID,
    role TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'manual'
);
CREATE INDEX role_grants_user ON role_grants(user_id);

CREATE TABLE sessions (
    id_hash TEXT PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE connectors (
    id UUID PRIMARY KEY,
    kind TEXT NOT NULL,
    role TEXT NOT NULL,
    name TEXT NOT NULL,
    config JSONB NOT NULL DEFAULT '{}',
    service_identity TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE secrets (
    connector_id UUID NOT NULL REFERENCES connectors(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    key_id TEXT NOT NULL,
    nonce BYTEA NOT NULL,
    ciphertext BYTEA NOT NULL,
    hint TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (connector_id, name)
);

CREATE TABLE projects (
    id UUID PRIMARY KEY,
    key TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    config JSONB NOT NULL DEFAULT '{}',
    templates JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE repositories (
    id UUID PRIMARY KEY,
    project_id UUID NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    connector_id UUID NOT NULL REFERENCES connectors(id),
    full_name TEXT NOT NULL,
    default_branch TEXT NOT NULL,
    protected_branches TEXT[] NOT NULL DEFAULT '{}',
    stack TEXT NOT NULL DEFAULT 'polyglot',
    stack_profile JSONB NOT NULL DEFAULT '{}',
    image_override TEXT,
    monitor_prs BOOLEAN NOT NULL DEFAULT false,
    runner_config JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (project_id, full_name)
);

CREATE TABLE runs (
    id UUID PRIMARY KEY,
    project_id UUID NOT NULL REFERENCES projects(id),
    workflow TEXT NOT NULL,
    state TEXT NOT NULL,
    waiting_for TEXT,
    pause_reason TEXT,
    title TEXT NOT NULL,
    work_item_key TEXT,
    ticket_key TEXT,
    repo_id UUID REFERENCES repositories(id),
    pr_number BIGINT,
    pr_url TEXT,
    branch TEXT,
    base_branch TEXT,
    assignee_user_id UUID REFERENCES users(id),
    harness TEXT NOT NULL,
    model TEXT NOT NULL,
    budget_usd DOUBLE PRECISION NOT NULL,
    max_turns INT NOT NULL,
    turn_timeout_secs BIGINT NOT NULL,
    spent_usd DOUBLE PRECISION NOT NULL DEFAULT 0,
    tokens_in BIGINT NOT NULL DEFAULT 0,
    tokens_out BIGINT NOT NULL DEFAULT 0,
    turns_used INT NOT NULL DEFAULT 0,
    review_rounds INT NOT NULL DEFAULT 0,
    event_seq BIGINT NOT NULL DEFAULT 0,
    runner_generation BIGINT NOT NULL DEFAULT 0,
    runner_state TEXT NOT NULL DEFAULT 'none',
    runner_handle TEXT,
    runner_image TEXT,
    runner_launched_at TIMESTAMPTZ,
    launch_attempts INT NOT NULL DEFAULT 0,
    last_heartbeat_at TIMESTAMPTZ,
    runner_busy BOOLEAN NOT NULL DEFAULT false,
    turn_started_at TIMESTAMPTZ,
    current_instruction BIGINT,
    token_salt TEXT NOT NULL DEFAULT '',
    last_activity_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    reminder_sent_at TIMESTAMPTZ,
    warning_sent_at TIMESTAMPTZ,
    expired_at TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    run_token_hash TEXT,
    model_key_hash TEXT,
    model_key_ref TEXT,
    chat_thread JSONB,
    wip_commit TEXT,
    session_blob TEXT,
    cap_notified BOOLEAN NOT NULL DEFAULT false,
    finish_declared BOOLEAN NOT NULL DEFAULT false,
    blocked_reason TEXT,
    outcome TEXT,
    template_version TEXT,
    policy_version TEXT,
    config_version TEXT,
    brief TEXT,
    scope TEXT,
    data JSONB NOT NULL DEFAULT '{}',
    created_by UUID,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- One work item never has two active runs.
CREATE UNIQUE INDEX runs_one_active_per_item ON runs(work_item_key)
    WHERE work_item_key IS NOT NULL AND state NOT IN ('finished','failed','cancelled','expired');
CREATE INDEX runs_state ON runs(state);
CREATE INDEX runs_project ON runs(project_id, created_at DESC);
CREATE UNIQUE INDEX runs_token ON runs(run_token_hash) WHERE run_token_hash IS NOT NULL;
CREATE UNIQUE INDEX runs_model_key ON runs(model_key_hash) WHERE model_key_hash IS NOT NULL;

-- Append-only event log, partitioned by time.
CREATE TABLE events (
    run_id UUID NOT NULL,
    seq BIGINT NOT NULL,
    ts TIMESTAMPTZ NOT NULL DEFAULT now(),
    kind TEXT NOT NULL,
    actor TEXT,
    payload JSONB NOT NULL DEFAULT '{}',
    blob_ref TEXT,
    PRIMARY KEY (run_id, seq, ts)
) PARTITION BY RANGE (ts);
CREATE TABLE events_default PARTITION OF events DEFAULT;
CREATE INDEX events_run_seq ON events(run_id, seq);

-- Content-addressed large payloads (blob-store interface, Postgres backend).
CREATE TABLE blobs (
    hash TEXT PRIMARY KEY,
    size BIGINT NOT NULL,
    data BYTEA NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE instructions (
    id BIGSERIAL PRIMARY KEY,
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    body JSONB NOT NULL,
    -- held: waiting for a cap decision or resume; pending: deliverable;
    -- acked: a supervisor took it; done; cancelled
    state TEXT NOT NULL DEFAULT 'pending',
    input_id UUID,
    acked_generation BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    done_at TIMESTAMPTZ
);
CREATE INDEX instructions_run_state ON instructions(run_id, state, id);

CREATE TABLE inputs (
    id UUID PRIMARY KEY,
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    dedupe_key TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    data JSONB NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE primitive_calls (
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    idem_key TEXT NOT NULL,
    name TEXT NOT NULL,
    args JSONB NOT NULL,
    status TEXT NOT NULL,
    response JSONB,
    instruction_id BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, idem_key)
);

-- Exactly-once bookkeeping for side effects performed by jobs.
CREATE TABLE side_effects (
    key TEXT PRIMARY KEY,
    status TEXT NOT NULL,
    result JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE questions (
    id UUID PRIMARY KEY,
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    action TEXT,
    pr_number BIGINT,
    state TEXT NOT NULL DEFAULT 'open',
    answer TEXT,
    decision TEXT,
    answered_by UUID REFERENCES users(id),
    answered_via TEXT,
    chat_ref JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    answered_at TIMESTAMPTZ
);
CREATE INDEX questions_run ON questions(run_id, state);

CREATE TABLE artifacts (
    id UUID PRIMARY KEY,
    run_id UUID NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    url TEXT,
    content TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE usage (
    id BIGSERIAL PRIMARY KEY,
    run_id UUID NOT NULL,
    project_id UUID NOT NULL,
    model TEXT NOT NULL,
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    cost_usd DOUBLE PRECISION NOT NULL,
    source TEXT NOT NULL,
    ts TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX usage_run ON usage(run_id);
CREATE INDEX usage_project_ts ON usage(project_id, ts);

CREATE TABLE audit (
    id BIGSERIAL PRIMARY KEY,
    ts TIMESTAMPTZ NOT NULL DEFAULT now(),
    actor TEXT NOT NULL,
    actor_user_id UUID,
    action TEXT NOT NULL,
    project_id UUID,
    run_id UUID,
    details JSONB NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_run ON audit(run_id);
CREATE INDEX audit_ts ON audit(ts DESC);

-- Generic document store for the fake connectors, shared by all replicas.
CREATE TABLE fake_store (
    ns TEXT NOT NULL,
    id TEXT NOT NULL,
    seq BIGSERIAL,
    data JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (ns, id)
);

CREATE TABLE poll_state (
    key TEXT PRIMARY KEY,
    value JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE webhook_deliveries (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    received_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
