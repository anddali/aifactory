import { useEffect, useState } from 'react';
import { Link, useParams } from 'react-router';
import {
  api,
  IMAGE_OVERRIDES,
  PROJECT_CONFIG_DEFAULTS,
  WORKFLOWS,
  type Connector,
  type ProjectConfig,
  type ProjectDetail,
  type Repo,
  type Template,
  type Workflow,
} from '../api';
import { useToast } from '../components/Toast';
import { workflowLabel } from '../lib/format';
import { useLoad } from '../lib/usePoll';

type NumKey =
  | 'budget_usd'
  | 'max_turns'
  | 'turn_timeout_secs'
  | 'max_concurrent_runs'
  | 'daily_spend_usd'
  | 'review_round_limit'
  | 'idle_reminder_secs'
  | 'idle_warning_secs'
  | 'idle_expiry_secs'
  | 'idle_grace_secs'
  | 'checkpoint_retention_days';
type StrKey = 'default_model' | 'harness' | 'chat_channel' | 'ops_channel' | 'fallback_contact' | 'default_assignee_email';
type BoolKey = 'merge_by_agent' | 'review_own_prs';

const BUDGET_FIELDS: [NumKey, string][] = [
  ['budget_usd', 'Run budget (USD)'],
  ['max_turns', 'Max turns per run'],
  ['turn_timeout_secs', 'Turn timeout (s)'],
  ['max_concurrent_runs', 'Max concurrent runs'],
  ['daily_spend_usd', 'Daily spend cap (USD)'],
  ['review_round_limit', 'Review round limit'],
];
const LIFETIME_FIELDS: [NumKey, string][] = [
  ['idle_reminder_secs', 'Idle reminder after (s)'],
  ['idle_warning_secs', 'Expiry warning after (s)'],
  ['idle_expiry_secs', 'Expire after (s)'],
  ['idle_grace_secs', 'Container idle grace (s)'],
  ['checkpoint_retention_days', 'Checkpoint retention (days)'],
];

function secsHint(v: number | undefined): string {
  if (v == null) return '';
  if (v >= 86400) return `${(v / 86400).toFixed(v % 86400 ? 1 : 0)} d`;
  if (v >= 3600) return `${(v / 3600).toFixed(v % 3600 ? 1 : 0)} h`;
  if (v >= 60) return `${(v / 60).toFixed(v % 60 ? 1 : 0)} min`;
  return '';
}

export function ProjectEditPage() {
  const { id = '' } = useParams();
  const toast = useToast();
  const project = useLoad(() => api.getProject(id), [id]);
  const connectors = useLoad(() => api.listConnectors());
  const [name, setName] = useState('');
  const [cfg, setCfg] = useState<ProjectConfig>({});
  const [dirty, setDirty] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!project.data) return;
    setName(project.data.name);
    setCfg({ ...PROJECT_CONFIG_DEFAULTS, ...project.data.config, jira_trigger: { ...PROJECT_CONFIG_DEFAULTS.jira_trigger!, ...(project.data.config.jira_trigger ?? {}) } });
    setDirty(false);
  }, [project.data]);

  if (!project.data) return <div className="page">{project.error ? <div className="banner banner-error">{project.error}</div> : <div className="muted">Loading…</div>}</div>;
  const p = project.data;
  const conns = connectors.data?.connectors ?? [];

  const set = <K extends keyof ProjectConfig>(k: K, v: ProjectConfig[K]) => {
    setCfg((c) => ({ ...c, [k]: v }));
    setDirty(true);
  };
  const numInput = (k: NumKey, label: string, secs = false) => (
    <label key={k}>
      {label}
      <input
        type="number"
        step="any"
        min={0}
        value={cfg[k] ?? ''}
        onChange={(e) => set(k, e.target.value === '' ? undefined : Number(e.target.value))}
      />
      {secs ? <span className="help">{secsHint(cfg[k])}</span> : null}
    </label>
  );
  const strInput = (k: StrKey, label: string, placeholder = '') => (
    <label key={k}>
      {label}
      <input value={(cfg[k] as string | null | undefined) ?? ''} placeholder={placeholder} onChange={(e) => set(k, e.target.value === '' ? null : e.target.value)} />
    </label>
  );
  const boolInput = (k: BoolKey, label: string, help: string) => (
    <label className="check" key={k}>
      <input type="checkbox" checked={!!cfg[k]} onChange={(e) => set(k, e.target.checked)} /> {label}
      <span className="help">{help}</span>
    </label>
  );
  const connectorSelect = (k: 'ticket_connector_id' | 'chat_connector_id', label: string, role: Connector['role']) => (
    <label key={k}>
      {label}
      <select value={cfg[k] ?? ''} onChange={(e) => set(k, e.target.value || null)}>
        <option value="">(none)</option>
        {conns
          .filter((c) => c.role === role)
          .map((c) => (
            <option key={c.id} value={c.id}>
              {c.name} ({c.kind})
            </option>
          ))}
      </select>
    </label>
  );

  const save = async () => {
    setBusy(true);
    try {
      // Normalise "" / null strings to null so the server stores defaults consistently.
      await api.updateProject(p.id, { name: name.trim() || p.name, config: cfg });
      toast('Project saved', 'ok');
      void project.refresh();
    } catch {
      /* toast shown */
    } finally {
      setBusy(false);
    }
  };

  const enabled = cfg.enabled_workflows ?? [];
  const toggleWorkflow = (w: Workflow) => set('enabled_workflows', enabled.includes(w) ? enabled.filter((x) => x !== w) : [...enabled, w]);

  return (
    <div className="page">
      <div className="page-head">
        <Link to="/admin/projects" className="muted back">
          ← Projects
        </Link>
        <h1>
          {p.key} <span className="muted">· {p.name}</span>
        </h1>
        <div className="spacer" />
        <button className="primary" disabled={!dirty || busy} onClick={save}>
          {busy ? 'Saving…' : dirty ? 'Save changes' : 'Saved'}
        </button>
      </div>

      <div className="sections">
        <section className="card">
          <h2>General</h2>
          <div className="grid3">
            <label>
              Name
              <input
                value={name}
                onChange={(e) => {
                  setName(e.target.value);
                  setDirty(true);
                }}
              />
            </label>
            {strInput('default_model', 'Default model', 'gpt-4.1')}
            {strInput('harness', 'Harness', 'scripted')}
          </div>
          <div className="field-label">Enabled workflows</div>
          <div className="row wrap">
            {WORKFLOWS.map((w) => (
              <label className="check" key={w}>
                <input type="checkbox" checked={enabled.includes(w)} onChange={() => toggleWorkflow(w)} /> {workflowLabel(w)}
              </label>
            ))}
          </div>
        </section>

        <section className="card">
          <h2>Budgets and limits</h2>
          <div className="grid3">{BUDGET_FIELDS.map(([k, l]) => numInput(k, l, k === 'turn_timeout_secs'))}</div>
        </section>

        <section className="card">
          <h2>Lifetimes</h2>
          <div className="grid3">{LIFETIME_FIELDS.map(([k, l]) => numInput(k, l, k.endsWith('_secs')))}</div>
        </section>

        <section className="card">
          <h2>Connectors and people</h2>
          <div className="grid3">
            {connectorSelect('ticket_connector_id', 'Ticket connector', 'ticket')}
            {connectorSelect('chat_connector_id', 'Chat connector', 'chat')}
            {strInput('chat_channel', 'Chat channel', '#factory')}
            {strInput('ops_channel', 'Ops channel')}
            {strInput('fallback_contact', 'Fallback contact', 'person or #channel')}
            {strInput('default_assignee_email', 'Default assignee email')}
          </div>
        </section>

        <section className="card">
          <h2>Triggers</h2>
          <div className="grid3">
            <label>
              Jira label
              <input
                value={cfg.jira_trigger?.label ?? ''}
                placeholder="factory"
                onChange={(e) => set('jira_trigger', { label: e.target.value || null, transition: cfg.jira_trigger?.transition ?? null })}
              />
            </label>
            <label>
              Jira transition
              <input
                value={cfg.jira_trigger?.transition ?? ''}
                placeholder="(any)"
                onChange={(e) => set('jira_trigger', { label: cfg.jira_trigger?.label ?? null, transition: e.target.value || null })}
              />
            </label>
          </div>
        </section>

        <section className="card">
          <h2>Policy</h2>
          <div className="stack">
            {boolInput('merge_by_agent', 'Agent may merge', 'Merge still sits behind a hard approval gate.')}
            {boolInput('review_own_prs', 'Review own PRs', 'Let the PR review workflow review PRs opened by the factory.')}
          </div>
        </section>

        <ReposSection project={p} connectors={conns} onChange={() => void project.refresh()} />
        <TemplatesSection project={p} />
      </div>
    </div>
  );
}

function ReposSection({ project, connectors, onChange }: { project: ProjectDetail; connectors: Connector[]; onChange: () => void }) {
  const toast = useToast();
  const repoConnectors = connectors.filter((c) => c.role === 'repo');
  const [connectorId, setConnectorId] = useState('');
  const [available, setAvailable] = useState<{ full_name: string; default_branch: string }[] | null>(null);
  const [picked, setPicked] = useState('');
  const [filter, setFilter] = useState('');
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!connectorId && repoConnectors.length) setConnectorId(repoConnectors[0].id);
  }, [repoConnectors, connectorId]);

  useEffect(() => {
    setAvailable(null);
    setPicked('');
    if (!connectorId) return;
    let cancelled = false;
    setLoading(true);
    api
      .connectorRepos(connectorId)
      .then((r) => !cancelled && setAvailable(r.repos))
      .catch(() => !cancelled && setAvailable([]))
      .finally(() => !cancelled && setLoading(false));
    return () => {
      cancelled = true;
    };
  }, [connectorId]);

  const onboarded = new Set(project.repos.map((r) => r.full_name));
  const candidates = (available ?? []).filter((r) => !onboarded.has(r.full_name) && r.full_name.toLowerCase().includes(filter.toLowerCase()));

  const onboard = async () => {
    if (!picked) return;
    setBusy(true);
    try {
      await api.onboardRepo(project.id, connectorId, picked);
      toast(`Onboarded ${picked}`, 'ok');
      setPicked('');
      onChange();
    } catch {
      /* toast shown */
    } finally {
      setBusy(false);
    }
  };

  const update = async (r: Repo, body: { image_override?: string | null; monitor_prs?: boolean }) => {
    try {
      await api.updateRepo(r.id, body);
      onChange();
    } catch {
      /* toast shown */
    }
  };

  const remove = async (r: Repo) => {
    if (!window.confirm(`Remove ${r.full_name} from ${project.key}?`)) return;
    try {
      await api.deleteRepo(r.id);
      toast('Repository removed', 'ok');
      onChange();
    } catch {
      /* toast shown */
    }
  };

  return (
    <section className="card">
      <h2>Repositories</h2>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>Repository</th>
              <th>Default branch</th>
              <th>Protected</th>
              <th>Detected stack</th>
              <th>Image</th>
              <th>Monitor PRs</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {project.repos.map((r) => (
              <tr key={r.id}>
                <td>
                  <strong>{r.full_name}</strong>
                </td>
                <td>
                  <code>{r.default_branch}</code>
                </td>
                <td className="small">{r.protected_branches.join(', ') || <span className="muted">—</span>}</td>
                <td>
                  <span className="tag">{r.stack}</span>
                </td>
                <td>
                  <select value={r.image_override ?? ''} onChange={(e) => update(r, { image_override: e.target.value || null })} aria-label="Image override">
                    <option value="">auto</option>
                    {IMAGE_OVERRIDES.map((s) => (
                      <option key={s} value={s}>
                        {s}
                      </option>
                    ))}
                  </select>
                </td>
                <td>
                  <label className="switch">
                    <input type="checkbox" checked={r.monitor_prs} onChange={(e) => update(r, { monitor_prs: e.target.checked })} />
                    <span className="switch-track" />
                  </label>
                </td>
                <td>
                  <button className="small danger" onClick={() => remove(r)}>
                    Remove
                  </button>
                </td>
              </tr>
            ))}
            {!project.repos.length ? (
              <tr>
                <td colSpan={7} className="muted empty-cell">
                  No repositories onboarded.
                </td>
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
      <h3>Onboard a repository</h3>
      {repoConnectors.length === 0 ? (
        <div className="muted">
          No repository connectors. <Link to="/admin/connectors">Create one</Link> first.
        </div>
      ) : (
        <div className="row wrap">
          <select value={connectorId} onChange={(e) => setConnectorId(e.target.value)} aria-label="Repository connector">
            {repoConnectors.map((c) => (
              <option key={c.id} value={c.id}>
                {c.name} ({c.kind})
              </option>
            ))}
          </select>
          <input className="grow" value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="Filter repositories…" aria-label="Filter repositories" />
          <select className="grow" value={picked} onChange={(e) => setPicked(e.target.value)} aria-label="Repository" disabled={loading}>
            <option value="">{loading ? 'Loading…' : `Select (${candidates.length} available)`}</option>
            {candidates.map((r) => (
              <option key={r.full_name} value={r.full_name}>
                {r.full_name} · {r.default_branch}
              </option>
            ))}
          </select>
          <button className="primary" disabled={!picked || busy} onClick={onboard}>
            Onboard
          </button>
        </div>
      )}
      <p className="help">Default branch, protected branches and stack are detected on onboarding.</p>
    </section>
  );
}

function TemplatesSection({ project }: { project: ProjectDetail }) {
  const toast = useToast();
  const templates = useLoad(() => api.templates());
  const [workflow, setWorkflow] = useState<Workflow>('ticket_build');
  const existingOverrides = (project.config.templates ?? project.config.template_overrides ?? {}) as Record<string, unknown>;
  const [drafts, setDrafts] = useState<Record<string, string>>(() => {
    const d: Record<string, string> = {};
    for (const [k, v] of Object.entries(existingOverrides)) {
      if (typeof v === 'string') d[k] = v;
      else if (v && typeof v === 'object' && typeof (v as { body?: unknown }).body === 'string') d[k] = (v as { body: string }).body;
    }
    return d;
  });
  const [busy, setBusy] = useState(false);
  const builtIn: Template | undefined = templates.data?.templates.find((t) => t.workflow === workflow);
  const draft = drafts[workflow] ?? '';

  const save = async (body: string) => {
    setBusy(true);
    try {
      await api.setTemplateOverride(project.id, workflow, body);
      setDrafts((d) => ({ ...d, [workflow]: body }));
      toast(body ? `Override saved for ${workflowLabel(workflow)}` : `Override removed for ${workflowLabel(workflow)}`, 'ok');
    } catch {
      /* toast shown */
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="card">
      <h2>Workflow templates</h2>
      <div className="row wrap">
        <select value={workflow} onChange={(e) => setWorkflow(e.target.value as Workflow)} aria-label="Workflow">
          {WORKFLOWS.map((w) => (
            <option key={w} value={w}>
              {workflowLabel(w)}
              {drafts[w] ? ' (overridden)' : ''}
            </option>
          ))}
        </select>
        {builtIn ? <span className="muted small">built-in version {String(builtIn.version)}</span> : null}
        <div className="spacer" />
        <button className="ghost small" disabled={!builtIn} onClick={() => builtIn && setDrafts((d) => ({ ...d, [workflow]: builtIn.body }))}>
          Start from built-in
        </button>
      </div>
      <div className="template-grid">
        <label>
          Override (empty = use built-in)
          <textarea className="mono" rows={16} value={draft} onChange={(e) => setDrafts((d) => ({ ...d, [workflow]: e.target.value }))} spellCheck={false} />
        </label>
        <label>
          Built-in
          <textarea className="mono" rows={16} value={builtIn?.body ?? (templates.loading ? 'Loading…' : '')} readOnly />
        </label>
      </div>
      <div className="row end">
        <button className="danger ghost" disabled={busy} onClick={() => save('')}>
          Remove override
        </button>
        <button className="primary" disabled={busy || !draft.trim()} onClick={() => save(draft)}>
          Save override
        </button>
      </div>
    </section>
  );
}
