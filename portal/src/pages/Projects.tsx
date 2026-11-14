import { useState, type FormEvent } from 'react';
import { Link, useNavigate } from 'react-router';
import { api } from '../api';
import { useToast } from '../components/Toast';
import { usd, workflowLabel } from '../lib/format';
import { useLoad } from '../lib/usePoll';

export function ProjectsPage() {
  const list = useLoad(() => api.listProjects());
  const toast = useToast();
  const navigate = useNavigate();
  const [key, setKey] = useState('');
  const [name, setName] = useState('');
  const [busy, setBusy] = useState(false);

  const create = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    try {
      const p = await api.createProject({ key: key.trim(), name: name.trim() });
      toast('Project created', 'ok');
      navigate(`/admin/projects/${p.id}`);
    } catch {
      /* toast shown */
    } finally {
      setBusy(false);
    }
  };

  const projects = list.data?.projects ?? [];
  return (
    <div className="page">
      <div className="page-head">
        <h1>Projects</h1>
      </div>
      <form className="card row wrap" onSubmit={create}>
        <label className="inline">
          Key <input required value={key} onChange={(e) => setKey(e.target.value.toUpperCase())} placeholder="PAY" size={8} />
        </label>
        <label className="inline grow">
          Name <input required value={name} onChange={(e) => setName(e.target.value)} placeholder="Payments" />
        </label>
        <button className="primary" type="submit" disabled={busy || !key.trim() || !name.trim()}>
          + Create project
        </button>
      </form>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>Key</th>
              <th>Name</th>
              <th>Workflows</th>
              <th>Default model</th>
              <th className="num">Budget</th>
              <th className="num">Max turns</th>
              <th className="num">Daily spend</th>
            </tr>
          </thead>
          <tbody>
            {projects.map((p) => (
              <tr key={p.id} className="clickable" onClick={() => navigate(`/admin/projects/${p.id}`)}>
                <td>
                  <Link to={`/admin/projects/${p.id}`} onClick={(e) => e.stopPropagation()}>
                    <strong>{p.key}</strong>
                  </Link>
                </td>
                <td>{p.name}</td>
                <td className="small">{(p.config.enabled_workflows ?? []).map(workflowLabel).join(', ') || <span className="muted">default</span>}</td>
                <td>{p.config.default_model ?? <span className="muted">default</span>}</td>
                <td className="num">{p.config.budget_usd != null ? usd(p.config.budget_usd) : '—'}</td>
                <td className="num">{p.config.max_turns ?? '—'}</td>
                <td className="num">{p.config.daily_spend_usd != null ? usd(p.config.daily_spend_usd) : '—'}</td>
              </tr>
            ))}
            {!projects.length ? (
              <tr>
                <td colSpan={7} className="muted empty-cell">
                  {list.loading ? 'Loading…' : 'No projects yet.'}
                </td>
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
    </div>
  );
}
