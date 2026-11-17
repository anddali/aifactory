// Typed client for the AI Factory plane HTTP API (see docs/API.md).

export type Role = 'view' | 'operate' | 'approve' | 'administer';
export type Workflow = 'ticket_build' | 'pr_review' | 'ad_hoc' | 'security_audit';
export const WORKFLOWS: Workflow[] = ['ticket_build', 'pr_review', 'ad_hoc', 'security_audit'];
export type RunState =
  | 'queued'
  | 'starting'
  | 'running'
  | 'waiting'
  | 'paused'
  | 'finished'
  | 'failed'
  | 'cancelled'
  | 'expired';
export const TERMINAL_STATES: RunState[] = ['finished', 'failed', 'cancelled', 'expired'];
export type WaitingFor = 'human_answer' | 'approval' | 'review' | 'ci' | 'cap_decision' | 'input';

export interface Grant {
  project_id: string | null;
  role: Role;
}

export interface User {
  id: string;
  email: string;
  display_name: string;
  is_global_admin: boolean;
  grants: Grant[];
}

export type IdentityProvider = 'jira' | 'github' | 'azure_devops' | 'slack' | 'entra';
export const IDENTITY_PROVIDERS: IdentityProvider[] = ['jira', 'github', 'azure_devops', 'slack', 'entra'];

export interface Identity {
  provider: IdentityProvider;
  external_id: string;
}

export interface UserWithIdentities extends User {
  identities: Identity[];
}

export interface AuthConfig {
  dev_login: boolean;
  entra: boolean;
}

export interface PersonRef {
  id: string;
  email: string;
  display_name: string;
}

/** Server-side filters for the runs list; empty values are ignored. */
/** Approval decisions; `changes` only for plan reviews (with the changes as the answer). */
export type Decision = 'approved' | 'rejected' | 'changes';

export interface RunFilters {
  /** Free text: title, ticket key, branch, run id or PR number. */
  q?: string;
  /** Exact run states, comma-separated. */
  status?: string;
  workflow?: string;
  harness?: string;
  model?: string;
  repo?: string;
  /** A user id, or `me`. */
  assignee?: string;
}

export interface RunFacets {
  models: string[];
  harnesses: string[];
  repos: string[];
  assignees: { id: string; email: string; display_name: string }[];
}

export interface RunSummary {
  id: string;
  title: string;
  project_id: string;
  project_key: string;
  workflow: Workflow;
  state: RunState;
  waiting_for: WaitingFor | null;
  pause_reason: string | null;
  spent_usd: number;
  budget_usd: number;
  turns_used: number;
  max_turns: number;
  needs_you: boolean;
  ticket_key: string | null;
  repo: string | null;
  pr_number: number | null;
  pr_url: string | null;
  branch: string | null;
  assignee: PersonRef | null;
  model: string;
  harness: string;
  /** Two-phase workflows: `plan` until the plan is approved, then `build`. */
  phase?: 'plan' | 'build' | null;
  created_at: string;
  updated_at: string;
  last_activity_at: string;
}

export interface Artifact {
  id: string;
  kind: string;
  title: string;
  url: string | null;
  content: string | null;
  created_at: string;
}

export interface Question {
  id: string;
  run_id: string;
  kind: 'question' | 'approval';
  text: string;
  action: string | null;
  pr_number: number | null;
  state: 'open' | 'answered';
  answer: string | null;
  decision: string | null;
  answered_by: string | null;
  answered_via: 'slack' | 'portal' | null;
  created_at: string;
  answered_at: string | null;
}

export interface ModelCost {
  model: string;
  input_tokens: number;
  output_tokens: number;
  cost_usd: number;
}

export interface RunnerInfo {
  state: string;
  generation: number;
  handle: string | null;
  last_heartbeat_at: string | null;
}

export interface RunDetail extends RunSummary {
  artifacts: Artifact[];
  questions: Question[];
  cost_by_model: ModelCost[];
  event_count: number;
  chat_thread: Record<string, unknown> | null;
  template_version: string | number | null;
  policy_version: string | number | null;
  config_version: string | number | null;
  runner: RunnerInfo | null;
}

export interface RunEvent {
  seq: number;
  ts: string;
  kind: string;
  actor: string | null;
  payload: Record<string, unknown>;
}

export interface EventsPage {
  events: RunEvent[];
  total: number;
  max_seq: number;
}

export interface CreateRunRequest {
  project_id: string;
  workflow: Workflow;
  ticket_key?: string;
  repo?: string;
  pr_number?: number;
  title?: string;
  brief?: string;
  scope?: string;
  model?: string;
  budget_usd?: number;
  max_turns?: number;
}

export type ConnectorRole = 'ticket' | 'repo' | 'chat' | 'model';

export interface SecretHint {
  name: string;
  hint: string;
  updated_at: string;
}

export interface Connector {
  id: string;
  kind: string;
  role: ConnectorRole;
  name: string;
  config: Record<string, unknown>;
  secrets: SecretHint[];
  service_identity: string | null;
  created_at: string;
}

/** Field descriptors are not fully specified by the contract: accept plain names or objects. */
export type FieldSpec =
  | string
  | {
      name: string;
      label?: string;
      type?: string;
      required?: boolean;
      default?: unknown;
      help?: string;
      placeholder?: string;
      /** Choices of a `select` field. */
      options?: { value: string; label: string }[];
      /** Shown, and required, only while another config field has this value. */
      show_if?: ShowIf;
    };

export interface ShowIf {
  field: string;
  equals: string;
}

export interface ConnectorKind {
  kind: string;
  label?: string;
  role: ConnectorRole;
  description?: string;
  config_fields: FieldSpec[];
  secret_fields: FieldSpec[];
  /** Inbound webhook URL with `{id}` standing for the connector id; null when the kind takes none. */
  webhook_url?: string | null;
  /** The webhook applies only while this holds (e.g. not in Slack Socket Mode). */
  webhook_show_if?: ShowIf | null;
}

export function fieldName(f: FieldSpec): string {
  return typeof f === 'string' ? f : f.name;
}

export function fieldLabel(f: FieldSpec): string {
  return typeof f === 'string' ? f : f.label ?? f.name;
}

export interface JiraTrigger {
  label: string | null;
  transition: string | null;
}

export interface ProjectConfig {
  enabled_workflows?: Workflow[];
  ticket_connector_id?: string | null;
  chat_connector_id?: string | null;
  chat_channel?: string;
  default_model?: string;
  harness?: string;
  budget_usd?: number;
  max_turns?: number;
  turn_timeout_secs?: number;
  idle_reminder_secs?: number;
  idle_warning_secs?: number;
  idle_expiry_secs?: number;
  checkpoint_retention_days?: number;
  idle_grace_secs?: number;
  max_concurrent_runs?: number;
  daily_spend_usd?: number;
  review_round_limit?: number;
  /** Share of the run budget the plan phase may spend before the run pauses (0 = no limit). */
  plan_budget_share?: number;
  merge_by_agent?: boolean;
  fallback_contact?: string | null;
  ops_channel?: string | null;
  jira_trigger?: JiraTrigger;
  review_own_prs?: boolean;
  default_assignee_email?: string | null;
  [key: string]: unknown;
}

export const PROJECT_CONFIG_DEFAULTS: ProjectConfig = {
  enabled_workflows: ['ticket_build', 'pr_review', 'ad_hoc', 'security_audit'],
  ticket_connector_id: null,
  chat_connector_id: null,
  chat_channel: '#factory',
  default_model: 'gpt-4.1',
  harness: 'scripted',
  budget_usd: 10,
  max_turns: 30,
  turn_timeout_secs: 1800,
  idle_reminder_secs: 86400,
  idle_warning_secs: 172800,
  idle_expiry_secs: 259200,
  checkpoint_retention_days: 14,
  idle_grace_secs: 1200,
  max_concurrent_runs: 5,
  daily_spend_usd: 100,
  review_round_limit: 5,
  merge_by_agent: false,
  fallback_contact: null,
  ops_channel: null,
  jira_trigger: { label: 'factory', transition: null },
  review_own_prs: false,
  default_assignee_email: null,
};

export interface Project {
  id: string;
  key: string;
  name: string;
  config: ProjectConfig;
  created_at?: string;
}

export type Stack = 'python' | 'dotnet' | 'node' | 'rust' | 'polyglot';
export const IMAGE_OVERRIDES: Stack[] = ['python', 'dotnet', 'node', 'rust', 'polyglot'];

export interface Repo {
  id: string;
  project_id: string;
  connector_id: string;
  full_name: string;
  default_branch: string;
  protected_branches: string[];
  stack: Stack;
  stack_profile: Record<string, unknown>;
  image_override: string | null;
  monitor_prs: boolean;
  runner_config: Record<string, unknown> | null;
}

export interface ProjectDetail extends Project {
  repos: Repo[];
}

export interface Template {
  workflow: Workflow;
  version: string | number;
  body: string;
}

export interface ModelPrice {
  input_per_mtok: number;
  output_per_mtok: number;
}

export type Launcher = 'process' | 'docker' | 'fargate';

export interface EgressProbe {
  host: string | null;
  ok: boolean;
  detail: string;
}

export interface EgressTest {
  ok: boolean;
  proxy: string;
  allowed: EgressProbe;
  blocked: EgressProbe;
}

export interface Settings {
  kill_switch: boolean;
  model_connector_id: string | null;
  model_prices: Record<string, ModelPrice>;
  egress_allowlist: string[];
  /** Proxy given to runners as HTTP(S)_PROXY; null: runners get none. */
  egress_proxy_url: string | null;
  deny_patterns: string[];
  event_retention_days: number;
  tick_ms: number;
  heartbeat_secs: number;
  stuck_repeat_threshold: number;
  stuck_noprogress_turns: number;
  ops_channel: string | null;
  launcher: Launcher;
}

export interface AuditEntry {
  id: string;
  ts: string;
  actor: string | null;
  action: string;
  project_id: string | null;
  run_id: string | null;
  details: unknown;
}

export interface OutcomeRow {
  project_key: string;
  model: string;
  harness: string;
  runs: number;
  approved_prs: number;
  approval_rate: number | null;
  avg_review_rounds: number | null;
  avg_hours_to_approval: number | null;
  cost_per_approved_pr: number | null;
}

// ---------------------------------------------------------------------------
// Transport

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
    this.name = 'ApiError';
  }
}

type Listener<T> = (v: T) => void;
const unauthorizedListeners = new Set<Listener<void>>();
const errorListeners = new Set<Listener<string>>();

/** Subscribe to 401 responses (the app redirects to sign-in). */
export function onUnauthorized(fn: () => void): () => void {
  unauthorizedListeners.add(fn);
  return () => unauthorizedListeners.delete(fn);
}

/** Subscribe to API errors to show in a toast. */
export function onApiError(fn: (msg: string) => void): () => void {
  errorListeners.add(fn);
  return () => errorListeners.delete(fn);
}

export function reportError(msg: string): void {
  errorListeners.forEach((l) => l(msg));
}

interface RequestOpts {
  /** Do not broadcast errors to the toast (caller handles them). */
  quiet?: boolean;
  /** Do not trigger the sign-in redirect on 401. */
  allow401?: boolean;
}

async function request<T>(method: string, path: string, body?: unknown, opts: RequestOpts = {}): Promise<T> {
  let res: Response;
  try {
    res = await fetch(path, {
      method,
      credentials: 'same-origin',
      headers: body !== undefined ? { 'Content-Type': 'application/json', Accept: 'application/json' } : { Accept: 'application/json' },
      body: body !== undefined ? JSON.stringify(body) : undefined,
    });
  } catch (e) {
    const msg = `Network error: ${(e as Error).message}`;
    if (!opts.quiet) reportError(msg);
    throw new ApiError(0, msg);
  }
  if (res.status === 401) {
    if (!opts.allow401) unauthorizedListeners.forEach((l) => l());
    throw new ApiError(401, 'Not signed in');
  }
  const text = await res.text();
  let data: unknown = undefined;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch {
      data = undefined;
    }
  }
  if (!res.ok) {
    const msg =
      (data && typeof data === 'object' && 'error' in data && typeof (data as { error: unknown }).error === 'string'
        ? (data as { error: string }).error
        : text || res.statusText) || `HTTP ${res.status}`;
    const full = `${method} ${path}: ${msg}`;
    if (!opts.quiet) reportError(full);
    throw new ApiError(res.status, msg);
  }
  return data as T;
}

function qs(params: Record<string, string | number | undefined | null>): string {
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== '') p.set(k, String(v));
  }
  const s = p.toString();
  return s ? `?${s}` : '';
}

const enc = encodeURIComponent;

export const api = {
  // Auth
  authConfig: () => request<AuthConfig>('GET', '/auth/config', undefined, { allow401: true }),
  devLogin: (email: string, name?: string) =>
    request<User>('POST', '/auth/dev-login', name ? { email, name } : { email }, { allow401: true }),
  logout: () => request<Record<string, never>>('POST', '/auth/logout', {}, { allow401: true }),
  me: () => request<User>('GET', '/auth/me', undefined, { allow401: true, quiet: true }),
  entraLoginUrl: '/auth/entra/login',

  // Runs
  listRuns: (state: 'active' | 'done' | 'all', projectId?: string, limit = 100, filters: RunFilters = {}) =>
    request<{ runs: RunSummary[] }>('GET', `/api/runs${qs({ state, project_id: projectId, limit, ...filters })}`),
  runFacets: () => request<RunFacets>('GET', '/api/runs/facets'),
  createRun: (req: CreateRunRequest) => request<RunSummary>('POST', '/api/runs', req),
  getRun: (id: string) => request<RunDetail>('GET', `/api/runs/${enc(id)}`),
  events: (id: string, p: { after?: number; before?: number; limit?: number } = {}) =>
    request<EventsPage>('GET', `/api/runs/${enc(id)}/events${qs({ after: p.after, before: p.before, limit: p.limit ?? 200 })}`),
  streamUrl: (id: string) => `/api/runs/${enc(id)}/stream`,
  steer: (id: string, text: string, interrupt: boolean) => request<unknown>('POST', `/api/runs/${enc(id)}/steer`, { text, interrupt }),
  pause: (id: string) => request<unknown>('POST', `/api/runs/${enc(id)}/pause`, {}),
  resume: (id: string) => request<unknown>('POST', `/api/runs/${enc(id)}/resume`, {}),
  cancel: (id: string) => request<unknown>('POST', `/api/runs/${enc(id)}/cancel`, {}),
  revive: (id: string) => request<unknown>('POST', `/api/runs/${enc(id)}/revive`, {}),
  teardown: (id: string) => request<unknown>('POST', `/api/runs/${enc(id)}/teardown`, {}),
  cap: (id: string, body: { action: 'raise' | 'end'; max_turns?: number; budget_usd?: number }) =>
    request<unknown>('POST', `/api/runs/${enc(id)}/cap`, body),
  answerQuestion: (id: string, body: { answer?: string; decision?: Decision }) =>
    request<Question>('POST', `/api/questions/${enc(id)}/answer`, body),

  // Connectors
  listConnectors: () => request<{ connectors: Connector[] }>('GET', '/api/connectors'),
  createConnector: (body: { kind: string; name: string; config: Record<string, unknown> }) =>
    request<Connector>('POST', '/api/connectors', body),
  updateConnector: (id: string, body: { name?: string; config?: Record<string, unknown> }) =>
    request<Connector>('PUT', `/api/connectors/${enc(id)}`, body),
  deleteConnector: (id: string) => request<unknown>('DELETE', `/api/connectors/${enc(id)}`),
  setConnectorSecrets: (id: string, secrets: Record<string, string>) =>
    request<unknown>('PUT', `/api/connectors/${enc(id)}/secrets`, { secrets }),
  testConnector: (id: string) => request<{ ok: boolean; message: string }>('POST', `/api/connectors/${enc(id)}/test`, {}),
  /** Test settings as entered, without saving. Blank secrets fall back to the saved ones when `id` is given. */
  testConnectorDraft: (body: { id?: string; kind: string; config: Record<string, unknown>; secrets: Record<string, string> }) =>
    request<{ ok: boolean; message: string }>('POST', '/api/connectors/test', body),
  connectorRepos: (id: string) =>
    request<{ repos: { full_name: string; default_branch: string }[] }>('GET', `/api/connectors/${enc(id)}/repos`),
  connectorKinds: () => request<{ kinds: ConnectorKind[] }>('GET', '/api/connector-kinds'),

  // Projects
  listProjects: (quiet = false) => request<{ projects: Project[] }>('GET', '/api/projects', undefined, { quiet }),
  createProject: (body: { key: string; name: string; config?: ProjectConfig }) => request<Project>('POST', '/api/projects', body),
  getProject: (id: string) => request<ProjectDetail>('GET', `/api/projects/${enc(id)}`),
  updateProject: (id: string, body: { name?: string; config?: ProjectConfig }) =>
    request<Project>('PUT', `/api/projects/${enc(id)}`, body),
  projectRepos: (id: string, quiet = false) => request<{ repos: Repo[] }>('GET', `/api/projects/${enc(id)}/repos`, undefined, { quiet }),
  onboardRepo: (projectId: string, connectorId: string, fullName: string) =>
    request<Repo>('POST', `/api/projects/${enc(projectId)}/repos`, { connector_id: connectorId, full_name: fullName }),
  updateRepo: (id: string, body: { image_override?: string | null; monitor_prs?: boolean }) =>
    request<Repo>('PUT', `/api/repos/${enc(id)}`, body),
  deleteRepo: (id: string) => request<unknown>('DELETE', `/api/repos/${enc(id)}`),
  templates: () => request<{ templates: Template[] }>('GET', '/api/templates'),
  setTemplateOverride: (projectId: string, workflow: Workflow, body: string) =>
    request<unknown>('PUT', `/api/projects/${enc(projectId)}/templates/${enc(workflow)}`, { body }),

  // Users
  listUsers: () => request<{ users: UserWithIdentities[] }>('GET', '/api/users'),
  createUser: (body: { email: string; display_name?: string; is_global_admin?: boolean }) =>
    request<User>('POST', '/api/users', body),
  setIdentities: (id: string, identities: Identity[]) => request<unknown>('PUT', `/api/users/${enc(id)}/identities`, { identities }),
  setGrants: (id: string, grants: Grant[]) => request<unknown>('PUT', `/api/users/${enc(id)}/grants`, { grants }),

  // Settings, audit, metrics
  getSettings: () => request<Settings>('GET', '/api/settings'),
  updateSettings: (body: Partial<Settings>) => request<Settings>('PUT', '/api/settings', body),
  testEgress: (url: string | null) => request<EgressTest>('POST', '/api/settings/egress-test', { url }),
  audit: (p: { run_id?: string; limit?: number } = {}) =>
    request<{ entries: AuditEntry[] }>('GET', `/api/audit${qs({ run_id: p.run_id, limit: p.limit ?? 200 })}`),
  outcomes: () => request<{ rows: OutcomeRow[] }>('GET', '/api/metrics/outcomes'),
};

// ---------------------------------------------------------------------------
// Role helpers

const ROLE_RANK: Record<Role, number> = { view: 0, operate: 1, approve: 2, administer: 3 };

/** Whether the user holds `role` or a higher one (view < operate < approve < administer) for a project. */
export function hasRole(user: User | null, role: Role, projectId?: string | null): boolean {
  if (!user) return false;
  if (user.is_global_admin) return true;
  return user.grants.some(
    (g) => (g.project_id === null || (projectId != null && g.project_id === projectId)) && ROLE_RANK[g.role] >= ROLE_RANK[role],
  );
}

export function isAdmin(user: User | null): boolean {
  if (!user) return false;
  return user.is_global_admin || user.grants.some((g) => g.role === 'administer');
}
