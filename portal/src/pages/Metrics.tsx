import { useMemo, useState } from 'react';
import { api, type OutcomeRow } from '../api';
import { num, pct, usd } from '../lib/format';
import { useLoad } from '../lib/usePoll';

type SortKey = keyof OutcomeRow;
const COLUMNS: [SortKey, string, (r: OutcomeRow) => string, boolean][] = [
  ['project_key', 'Project', (r) => r.project_key, false],
  ['model', 'Model', (r) => r.model, false],
  ['harness', 'Harness', (r) => r.harness, false],
  ['runs', 'Runs', (r) => num(r.runs), true],
  ['approved_prs', 'Approved PRs', (r) => num(r.approved_prs), true],
  ['approval_rate', 'Approval rate', (r) => pct(r.approval_rate), true],
  ['avg_review_rounds', 'Avg review rounds', (r) => num(r.avg_review_rounds, 1), true],
  ['avg_hours_to_approval', 'Avg hours to approval', (r) => num(r.avg_hours_to_approval, 1), true],
  ['cost_per_approved_pr', 'Cost / approved PR', (r) => usd(r.cost_per_approved_pr), true],
];

export function MetricsPage() {
  const data = useLoad(() => api.outcomes());
  const [sort, setSort] = useState<{ key: SortKey; dir: 1 | -1 }>({ key: 'project_key', dir: 1 });
  const rows = useMemo(() => {
    const list = [...(data.data?.rows ?? [])];
    list.sort((a, b) => {
      const av = a[sort.key];
      const bv = b[sort.key];
      if (av == null && bv == null) return 0;
      if (av == null) return 1;
      if (bv == null) return -1;
      return (typeof av === 'number' && typeof bv === 'number' ? av - bv : String(av).localeCompare(String(bv))) * sort.dir;
    });
    return list;
  }, [data.data, sort]);

  return (
    <div className="page">
      <div className="page-head">
        <h1>Outcome metrics</h1>
        <div className="spacer" />
        <button className="ghost small" onClick={() => void data.refresh()}>
          Refresh
        </button>
      </div>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              {COLUMNS.map(([k, label, , numeric]) => (
                <th key={k} className={`sortable${numeric ? ' num' : ''}`} onClick={() => setSort((s) => ({ key: k, dir: s.key === k ? (s.dir === 1 ? -1 : 1) : 1 }))}>
                  {label}
                  {sort.key === k ? (sort.dir === 1 ? ' ▲' : ' ▼') : ''}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={`${r.project_key}|${r.model}|${r.harness}`}>
                {COLUMNS.map(([k, , fmt, numeric]) => (
                  <td key={k} className={numeric ? 'num' : ''}>
                    {fmt(r)}
                  </td>
                ))}
              </tr>
            ))}
            {!rows.length ? (
              <tr>
                <td colSpan={COLUMNS.length} className="muted empty-cell">
                  {data.loading ? 'Loading…' : 'No outcomes recorded yet.'}
                </td>
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
    </div>
  );
}
