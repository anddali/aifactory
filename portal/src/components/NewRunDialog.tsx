import { useEffect, useMemo, useState, type FormEvent } from 'react';
import { useNavigate } from 'react-router';
import { api, WORKFLOWS, type CreateRunRequest, type Project, type Repo, type Workflow } from '../api';
import { optNum, workflowLabel } from '../lib/format';
import { Modal } from './Modal';
import { useToast } from './Toast';

interface Props {
  onClose: () => void;
  projects: Project[];
  /** [id, key] pairs used when the full project list is not visible to this user. */
  fallbackProjects: [string, string][];
}

/** Which inputs each workflow takes, and which are required. */
const FIELDS: Record<Workflow, { ticket?: 'req' | 'opt'; repo?: 'req' | 'opt'; pr?: 'req'; title?: 'req' | 'opt'; brief?: 'req' | 'opt'; scope?: 'opt' }> = {
  ticket_build: { ticket: 'req', repo: 'opt', title: 'opt', brief: 'opt' },
  pr_review: { repo: 'req', pr: 'req', title: 'opt', brief: 'opt' },
  ad_hoc: { title: 'req', brief: 'req', repo: 'opt' },
  security_audit: { repo: 'req', scope: 'opt', title: 'opt', brief: 'opt' },
};

export function NewRunDialog({ onClose, projects, fallbackProjects }: Props) {
  const navigate = useNavigate();
  const toast = useToast();
  const options: { id: string; key: string; name?: string; project?: Project }[] = useMemo(
    () =>
      projects.length
        ? projects.map((p) => ({ id: p.id, key: p.key, name: p.name, project: p }))
        : fallbackProjects.map(([id, key]) => ({ id, key })),
    [projects, fallbackProjects],
  );

  const [projectId, setProjectId] = useState(options[0]?.id ?? '');
  const project = options.find((o) => o.id === projectId)?.project;
  const enabled = (project?.config.enabled_workflows?.length ? project.config.enabled_workflows : WORKFLOWS) as Workflow[];
  const [workflow, setWorkflow] = useState<Workflow>('ticket_build');
  const [ticketKey, setTicketKey] = useState('');
  const [repo, setRepo] = useState('');
  const [prNumber, setPrNumber] = useState('');
  const [title, setTitle] = useState('');
  const [brief, setBrief] = useState('');
  const [scope, setScope] = useState('');
  const [model, setModel] = useState('');
  const [budget, setBudget] = useState('');
  const [maxTurns, setMaxTurns] = useState('');
  const [repos, setRepos] = useState<Repo[] | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!enabled.includes(workflow)) setWorkflow(enabled[0] ?? 'ad_hoc');
  }, [enabled, workflow]);

  useEffect(() => {
    setRepos(null);
    if (!projectId) return;
    let cancelled = false;
    api
      .projectRepos(projectId, true)
      .then((r) => !cancelled && setRepos(r.repos))
      .catch(() => !cancelled && setRepos(null));
    return () => {
      cancelled = true;
    };
  }, [projectId]);

  const f = FIELDS[workflow];
  const valid =
    !!projectId &&
    (f.ticket !== 'req' || !!ticketKey.trim()) &&
    (f.repo !== 'req' || !!repo.trim()) &&
    (f.pr !== 'req' || optNum(prNumber) !== undefined) &&
    (f.title !== 'req' || !!title.trim()) &&
    (f.brief !== 'req' || !!brief.trim());

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    if (!valid) return;
    const req: CreateRunRequest = { project_id: projectId, workflow };
    if (f.ticket && ticketKey.trim()) req.ticket_key = ticketKey.trim();
    if (f.repo && repo.trim()) req.repo = repo.trim();
    if (f.pr) req.pr_number = optNum(prNumber);
    if (f.title && title.trim()) req.title = title.trim();
    if (f.brief && brief.trim()) req.brief = brief.trim();
    if (f.scope && scope.trim()) req.scope = scope.trim();
    if (model.trim()) req.model = model.trim();
    const b = optNum(budget);
    if (b !== undefined) req.budget_usd = b;
    const t = optNum(maxTurns);
    if (t !== undefined) req.max_turns = Math.round(t);
    setBusy(true);
    try {
      const run = await api.createRun(req);
      toast('Run started', 'ok');
      onClose();
      navigate(`/runs/${run.id}`);
    } catch {
      /* toast shown */
    } finally {
      setBusy(false);
    }
  };

  const req = (k: 'req' | 'opt' | undefined) => (k === 'req' ? <span className="req">*</span> : <span className="muted"> (optional)</span>);

  return (
    <Modal title="New run" onClose={onClose}>
      <form className="form" onSubmit={submit}>
        <div className="grid2">
          <label>
            Project
            <select value={projectId} onChange={(e) => setProjectId(e.target.value)} required>
              {options.length === 0 ? <option value="">No projects</option> : null}
              {options.map((o) => (
                <option key={o.id} value={o.id}>
                  {o.key}
                  {o.name ? ` — ${o.name}` : ''}
                </option>
              ))}
            </select>
          </label>
          <label>
            Workflow
            <select value={workflow} onChange={(e) => setWorkflow(e.target.value as Workflow)}>
              {enabled.map((w) => (
                <option key={w} value={w}>
                  {workflowLabel(w)}
                </option>
              ))}
            </select>
          </label>
        </div>
        {f.ticket ? (
          <label>
            Ticket key{req(f.ticket)}
            <input value={ticketKey} onChange={(e) => setTicketKey(e.target.value)} placeholder="PROJ-123" />
          </label>
        ) : null}
        {f.repo || f.pr ? (
          <div className="grid2">
            {f.repo ? (
              <label>
                Repository{req(f.repo)}
                {repos && repos.length ? (
                  <select value={repo} onChange={(e) => setRepo(e.target.value)}>
                    <option value="">{f.repo === 'req' ? 'Select…' : '(project default)'}</option>
                    {repos.map((r) => (
                      <option key={r.id} value={r.full_name}>
                        {r.full_name}
                      </option>
                    ))}
                  </select>
                ) : (
                  <input value={repo} onChange={(e) => setRepo(e.target.value)} placeholder="org/repo" />
                )}
              </label>
            ) : null}
            {f.pr ? (
              <label>
                PR number{req(f.pr)}
                <input inputMode="numeric" value={prNumber} onChange={(e) => setPrNumber(e.target.value)} placeholder="42" />
              </label>
            ) : null}
          </div>
        ) : null}
        {f.title ? (
          <label>
            Title{req(f.title)}
            <input value={title} onChange={(e) => setTitle(e.target.value)} />
          </label>
        ) : null}
        {f.brief ? (
          <label>
            Brief{req(f.brief)}
            <textarea rows={4} value={brief} onChange={(e) => setBrief(e.target.value)} placeholder="What should the agent do?" />
          </label>
        ) : null}
        {f.scope ? (
          <label>
            Scope{req(f.scope)}
            <input value={scope} onChange={(e) => setScope(e.target.value)} placeholder="paths, components or areas to audit" />
          </label>
        ) : null}
        <details className="overrides">
          <summary>Overrides</summary>
          <div className="grid3">
            <label>
              Model
              <input value={model} onChange={(e) => setModel(e.target.value)} placeholder={project?.config.default_model ?? 'default'} />
            </label>
            <label>
              Budget (USD)
              <input inputMode="decimal" value={budget} onChange={(e) => setBudget(e.target.value)} placeholder={String(project?.config.budget_usd ?? 'default')} />
            </label>
            <label>
              Max turns
              <input inputMode="numeric" value={maxTurns} onChange={(e) => setMaxTurns(e.target.value)} placeholder={String(project?.config.max_turns ?? 'default')} />
            </label>
          </div>
        </details>
        <div className="row end">
          <button type="button" className="ghost" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={!valid || busy}>
            {busy ? 'Starting…' : 'Start run'}
          </button>
        </div>
      </form>
    </Modal>
  );
}
