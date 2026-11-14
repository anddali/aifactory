import { useMemo, useState } from 'react';
import { Link, useNavigate, useSearchParams } from 'react-router';
import { api, hasRole, type Project, type RunSummary } from '../api';
import { NewRunDialog } from '../components/NewRunDialog';
import { NeedsYou, StateBadge } from '../components/StateBadge';
import { Tabs } from '../components/Tabs';
import { useAuth } from '../lib/auth';
import { absTime, relTime, usd, workflowLabel } from '../lib/format';
import { useLoad, usePoll } from '../lib/usePoll';

type Tab = 'active' | 'done';

export function DashboardPage() {
  const { user } = useAuth();
  const [params, setParams] = useSearchParams();
  const tab: Tab = params.get('tab') === 'done' ? 'done' : 'active';
  const projectId = params.get('project') ?? '';
  const [showNew, setShowNew] = useState(false);

  const active = usePoll(() => api.listRuns('active', projectId || undefined), 5000, [projectId]);
  const done = usePoll(() => api.listRuns('done', projectId || undefined), tab === 'done' ? 5000 : 30000, [projectId]);
  const projects = useLoad(() => api.listProjects(true).catch(() => ({ projects: [] as Project[] })));

  // Projects for the filter: from the API when visible to this user, else derived from runs.
  const projectOptions = useMemo(() => {
    const map = new Map<string, string>();
    for (const p of projects.data?.projects ?? []) map.set(p.id, p.key);
    for (const r of [...(active.data?.runs ?? []), ...(done.data?.runs ?? [])]) if (!map.has(r.project_id)) map.set(r.project_id, r.project_key);
    return [...map.entries()].sort((a, b) => a[1].localeCompare(b[1]));
  }, [projects.data, active.data, done.data]);

  const runs = (tab === 'active' ? active.data?.runs : done.data?.runs) ?? [];
  const sorted = useMemo(() => (tab === 'active' ? [...runs].sort((a, b) => Number(b.needs_you) - Number(a.needs_you)) : runs), [runs, tab]);
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
        <select value={projectId} onChange={(e) => setParam('project', e.target.value)} aria-label="Filter by project">
          <option value="">All projects</option>
          {projectOptions.map(([id, key]) => (
            <option key={id} value={id}>
              {key}
            </option>
          ))}
        </select>
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
        onChange={(k) => setParam('tab', k === 'active' ? '' : k)}
      />
      {error && !runs.length ? <div className="banner banner-error">{error}</div> : null}
      <RunsTable runs={sorted} loading={loading} empty={tab === 'active' ? 'No active runs.' : 'No past runs.'} />
      {showNew ? <NewRunDialog onClose={() => setShowNew(false)} projects={projects.data?.projects ?? []} fallbackProjects={projectOptions} /> : null}
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
