import { useState } from 'react';
import { Link, useSearchParams } from 'react-router';
import { api } from '../api';
import { Expandable } from '../components/JsonView';
import { absTime } from '../lib/format';
import { useLoad } from '../lib/usePoll';

export function AuditPage() {
  const [params, setParams] = useSearchParams();
  const runId = params.get('run_id') ?? '';
  const [limit, setLimit] = useState(200);
  const [runDraft, setRunDraft] = useState(runId);
  const [filter, setFilter] = useState('');
  const audit = useLoad(() => api.audit({ run_id: runId || undefined, limit }), [runId, limit]);

  const entries = (audit.data?.entries ?? []).filter((e) => {
    const f = filter.toLowerCase();
    return !f || e.action.toLowerCase().includes(f) || (e.actor ?? '').toLowerCase().includes(f) || JSON.stringify(e.details ?? '').toLowerCase().includes(f);
  });

  return (
    <div className="page">
      <div className="page-head">
        <h1>Audit log</h1>
        <div className="spacer" />
        <button className="ghost small" onClick={() => void audit.refresh()}>
          Refresh
        </button>
      </div>
      <form
        className="card row wrap"
        onSubmit={(e) => {
          e.preventDefault();
          const next = new URLSearchParams(params);
          if (runDraft.trim()) next.set('run_id', runDraft.trim());
          else next.delete('run_id');
          setParams(next);
        }}
      >
        <label className="inline grow">
          Run id <input value={runDraft} onChange={(e) => setRunDraft(e.target.value)} placeholder="all runs" />
        </label>
        <label className="inline">
          Limit
          <select value={limit} onChange={(e) => setLimit(Number(e.target.value))}>
            {[100, 200, 500, 1000].map((n) => (
              <option key={n} value={n}>
                {n}
              </option>
            ))}
          </select>
        </label>
        <button type="submit">Apply</button>
        <input className="grow" value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="Filter action, actor, details…" aria-label="Filter" />
      </form>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>Time</th>
              <th>Actor</th>
              <th>Action</th>
              <th>Run</th>
              <th>Details</th>
            </tr>
          </thead>
          <tbody>
            {entries.map((e) => (
              <tr key={e.id}>
                <td className="nowrap small">{absTime(e.ts)}</td>
                <td>{e.actor ?? <span className="muted">system</span>}</td>
                <td>
                  <code>{e.action}</code>
                </td>
                <td className="small">{e.run_id ? <Link to={`/runs/${e.run_id}`}>{e.run_id.slice(0, 8)}</Link> : <span className="muted">—</span>}</td>
                <td className="details-cell">
                  {e.details == null || (typeof e.details === 'object' && Object.keys(e.details as object).length === 0) ? (
                    <span className="muted">—</span>
                  ) : (
                    <Expandable label={<span className="ev-sum">{JSON.stringify(e.details).slice(0, 120)}</span>} value={e.details} />
                  )}
                </td>
              </tr>
            ))}
            {!entries.length ? (
              <tr>
                <td colSpan={5} className="muted empty-cell">
                  {audit.loading ? 'Loading…' : 'No entries.'}
                </td>
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
    </div>
  );
}
