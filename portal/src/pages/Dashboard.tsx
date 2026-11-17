import { useEffect, useMemo, useState } from 'react';
import { Link, useNavigate, useSearchParams } from 'react-router';
import { api, hasRole, WORKFLOWS, type Project, type RunFacets, type RunFilters, type RunState, type RunSummary } from '../api';
import { NewRunDialog } from '../components/NewRunDialog';
import { NeedsYou, StateBadge } from '../components/StateBadge';
import { Tabs } from '../components/Tabs';
import { useAuth } from '../lib/auth';
import { absTime, relTime, usd, workflowLabel } from '../lib/format';
import { useLoad, usePoll } from '../lib/usePoll';

type Tab = 'active' | 'done';

const ACTIVE_STATES: RunState[] = ['queued', 'starting', 'running', 'waiting', 'paused'];
const DONE_STATES: RunState[] = ['finished', 'failed', 'cancelled', 'expired'];
/** URL parameters that are server-side run filters. */
const FILTER_KEYS = ['q', 'status', 'workflow', 'assignee', 'repo', 'model', 'harness'] as const;

export function DashboardPage() {
  const { user } = useAuth();
  const [params, setParams] = useSearchParams();
  const tab: Tab = params.get('tab') === 'done' ? 'done' : 'active';
  const projectId = params.get('project') ?? '';
  const needsYouOnly = params.get('needs') === '1';
  const [showNew, setShowNew] = useState(false);

  const filters: RunFilters = Object.fromEntries(FILTER_KEYS.map((k) => [k, params.get(k) ?? undefined]));
  // A state filter only applies to the tab it belongs to.
  const statusFor = (t: Tab) => {
    const s = filters.status as RunState | undefined;
    if (!s) return undefined;
    return (t === 'active' ? ACTIVE_STATES : DONE_STATES).includes(s) ? s : 'none';
  };
  const filterKey = FILTER_KEYS.map((k) => params.get(k) ?? '').join('|');
  const active = usePoll(() => api.listRuns('active', projectId || undefined, 100, { ...filters, status: statusFor('active') }), 5000, [projectId, filterKey]);
  const done = usePoll(() => api.listRuns('done', projectId || undefined, 200, { ...filters, status: statusFor('done') }), tab === 'done' ? 5000 : 30000, [projectId, filterKey]);
  const projects = useLoad(() => api.listProjects(true).catch(() => ({ projects: [] as Project[] })));
  const facets = useLoad(() => api.runFacets().catch(() => null));

  // Projects for the filter: from the API when visible to this user, else derived from runs.
  const projectOptions = useMemo(() => {
    const map = new Map<string, string>();
    for (const p of projects.data?.projects ?? []) map.set(p.id, p.key);
    for (const r of [...(active.data?.runs ?? []), ...(done.data?.runs ?? [])]) if (!map.has(r.project_id)) map.set(r.project_id, r.project_key);
    return [...map.entries()].sort((a, b) => a[1].localeCompare(b[1]));
  }, [projects.data, active.data, done.data]);

  const allRuns = (tab === 'active' ? active.data?.runs : done.data?.runs) ?? [];
  const runs = needsYouOnly ? allRuns.filter((r) => r.needs_you) : allRuns;
  const sorted = useMemo(() => (tab === 'active' ? [...runs].sort((a, b) => Number(b.needs_you) - Number(a.needs_you)) : runs), [runs, tab]);
  const filtering = FILTER_KEYS.some((k) => params.get(k)) || needsYouOnly || !!projectId;
  const needsYouCount = (active.data?.runs ?? []).filter((r) => r.needs_you).length;
  const loading = tab === 'active' ? active.loading : done.loading;
  const error = tab === 'active' ? active.error : done.error;

  const canStart = !!user && (user.is_global_admin || user.grants.some((g) => hasRole(user, 'operate', g.project_id)));

  const setParam = (k: string, v: string) => {
    const next = new URLSearchParams(params);
    if (v) next.set(k, v);
    else next.delete(k);
    setParams(next, { replace: true });
  };
  const clearFilters = () => {
    const next = new URLSearchParams(params);
    for (const k of [...FILTER_KEYS, 'needs', 'project']) next.delete(k);
    setParams(next, { replace: true });
  };

  return (
    <div className="page">
      <div className="page-head">
        <h1>Runs</h1>
        {needsYouCount > 0 ? (
          <span className="needs-you-summary">
            {needsYouCount} run{needsYouCount === 1 ? '' : 's'} need{needsYouCount === 1 ? 's' : ''} you
          </span>
        ) : null}
        <div className="spacer" />
        {canStart ? (
          <button className="primary" onClick={() => setShowNew(true)}>
            + New run
          </button>
        ) : null}
      </div>
      <Tabs<Tab>
        tabs={[
          { key: 'active', label: 'Active', count: active.data?.runs.length },
          { key: 'done', label: 'Past', count: done.data?.runs.length },
        ]}
        active={tab}
        onChange={(k) => {
          const next = new URLSearchParams(params);
          if (k === 'active') next.delete('tab');
          else next.set('tab', k);
          next.delete('status'); // states differ per tab
          setParams(next, { replace: true });
        }}
      />
      <RunFiltersBar
        params={params}
        setParam={setParam}
        clear={clearFilters}
        filtering={filtering}
        tab={tab}
        projectOptions={projectOptions}
        facets={facets.data}
        meId={user?.id}
      />
      {error && !runs.length ? <div className="banner banner-error">{error}</div> : null}
      <RunsTable
        runs={sorted}
        loading={loading}
        empty={filtering ? 'No runs match these filters.' : tab === 'active' ? 'No active runs.' : 'No past runs.'}
      />
      {showNew ? <NewRunDialog onClose={() => setShowNew(false)} projects={projects.data?.projects ?? []} fallbackProjects={projectOptions} /> : null}
    </div>
  );
}

function RunFiltersBar({
  params,
  setParam,
  clear,
  filtering,
  tab,
  projectOptions,
  facets,
  meId,
}: {
  params: URLSearchParams;
  setParam: (k: string, v: string) => void;
  clear: () => void;
  filtering: boolean;
  tab: Tab;
  projectOptions: [string, string][];
  facets: RunFacets | null;
  meId?: string;
}) {
  // The search box updates the URL (and so the query) after a short pause in typing.
  const urlQ = params.get('q') ?? '';
  const [q, setQ] = useState(urlQ);
  useEffect(() => setQ(urlQ), [urlQ]);
  useEffect(() => {
    if (q === urlQ) return;
    const t = window.setTimeout(() => setParam('q', q.trim()), 300);
    return () => window.clearTimeout(t);
  }, [q]); // eslint-disable-line react-hooks/exhaustive-deps

  const states = tab === 'active' ? ACTIVE_STATES : DONE_STATES;
  const status = params.get('status') ?? '';
  const select = (key: string, label: string, options: [string, string][], all: string) => {
    const value = params.get(key) ?? '';
    const known = !value || options.some(([v]) => v === value);
    return (
      <select value={value} onChange={(e) => setParam(key, e.target.value)} aria-label={label} className={value ? 'filter-on' : undefined}>
        <option value="">{all}</option>
        {options.map(([v, l]) => (
          <option key={v} value={v}>
            {l}
          </option>
        ))}
        {!known ? <option value={value}>{value}</option> : null}
      </select>
    );
  };
  const assignees: [string, string][] = [
    ...(meId ? ([['me', 'Me']] as [string, string][]) : []),
    ...(facets?.assignees ?? []).filter((a) => a.id !== meId).map((a) => [a.id, a.display_name || a.email] as [string, string]),
  ];

  return (
    <div className="run-filters" role="search">
      <input
        type="search"
        value={q}
        onChange={(e) => setQ(e.target.value)}
        placeholder="Search runs…"
        title="Searches title, ticket key, branch, PR number and run id"
        aria-label="Search runs by title, ticket key, branch, PR number or run id"
        className={`run-filters-q${q ? ' filter-on' : ''}`}
      />
      {select('project', 'Project', projectOptions, 'All projects')}
      <select
        value={status && states.includes(status as RunState) ? status : ''}
        onChange={(e) => setParam('status', e.target.value)}
        aria-label="State"
        className={status ? 'filter-on' : undefined}
      >
        <option value="">Any state</option>
        {states.map((s) => (
          <option key={s} value={s}>
            {s}
          </option>
        ))}
      </select>
      {select('workflow', 'Workflow', WORKFLOWS.map((w) => [w, workflowLabel(w)]), 'All workflows')}
      {select('assignee', 'Assignee', assignees, 'Anyone')}
      {select('repo', 'Repository', (facets?.repos ?? []).map((r) => [r, r]), 'All repositories')}
      {select('model', 'Model', (facets?.models ?? []).map((m) => [m, m]), 'All models')}
      {select('harness', 'Harness', (facets?.harnesses ?? []).map((h) => [h, h]), 'All harnesses')}
      <label className="run-filters-check">
        <input type="checkbox" checked={params.get('needs') === '1'} onChange={(e) => setParam('needs', e.target.checked ? '1' : '')} />
        Needs you
      </label>
      {filtering ? (
        <button className="ghost small" onClick={clear}>
          Clear filters
        </button>
      ) : null}
    </div>
  );
}

function RunsTable({ runs, loading, empty }: { runs: RunSummary[]; loading: boolean; empty: string }) {
  const navigate = useNavigate();
  return (
    <div className="table-wrap">
      <table className="table runs-table">
        <thead>
          <tr>
            <th className="col-needs" aria-label="Needs you" />
            <th>Title</th>
            <th>Project</th>
            <th>Workflow</th>
            <th>State</th>
            <th className="num">Cost</th>
            <th className="num">Turns</th>
            <th>Updated</th>
          </tr>
        </thead>
        <tbody>
          {runs.map((r) => (
            <tr key={r.id} className={`clickable${r.needs_you ? ' row-needs-you' : ''}`} onClick={() => navigate(`/runs/${r.id}`)}>
              <td className="col-needs">{r.needs_you ? <NeedsYou /> : null}</td>
              <td className="col-title">
                <Link to={`/runs/${r.id}`} onClick={(e) => e.stopPropagation()}>
                  {r.title || '(untitled)'}
                </Link>
                <div className="sub muted">
                  {r.ticket_key ? <span>{r.ticket_key}</span> : null}
                  {r.repo ? <span>{r.repo}</span> : null}
                  {r.pr_number != null ? <span>PR #{r.pr_number}</span> : null}
                </div>
              </td>
              <td>{r.project_key}</td>
              <td className="nowrap">{workflowLabel(r.workflow)}</td>
              <td>
                <StateBadge state={r.state} waitingFor={r.waiting_for} />
                {r.phase === 'plan' && !['finished', 'failed', 'cancelled', 'expired'].includes(r.state) ? (
                  <span className="phase-chip" title="Planning: nothing is built until the plan is approved">
                    plan
                  </span>
                ) : null}
              </td>
              <td className="num nowrap">
                <CostCell spent={r.spent_usd} budget={r.budget_usd} />
              </td>
              <td className="num nowrap">
                {r.turns_used}/{r.max_turns}
              </td>
              <td className="nowrap" title={absTime(r.updated_at)}>
                {relTime(r.updated_at)}
              </td>
            </tr>
          ))}
          {!runs.length ? (
            <tr>
              <td colSpan={8} className="muted empty-cell">
                {loading ? 'Loading…' : empty}
              </td>
            </tr>
          ) : null}
        </tbody>
      </table>
    </div>
  );
}

export function CostCell({ spent, budget }: { spent: number; budget: number }) {
  const ratio = budget > 0 ? Math.min(1, spent / budget) : 0;
  return (
    <span className="cost-cell" title={`${usd(spent, 4)} of ${usd(budget)}`}>
      {usd(spent)} <span className="muted">/ {usd(budget, 0)}</span>
      <span className="meter">
        <span className={`meter-fill${ratio > 0.9 ? ' hot' : ratio > 0.7 ? ' warm' : ''}`} style={{ width: `${ratio * 100}%` }} />
      </span>
    </span>
  );
}
