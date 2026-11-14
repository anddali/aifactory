import { useState, type FormEvent } from 'react';
import { api, IDENTITY_PROVIDERS, type Grant, type Identity, type IdentityProvider, type Project, type Role, type UserWithIdentities } from '../api';
import { Modal } from '../components/Modal';
import { useToast } from '../components/Toast';
import { useLoad } from '../lib/usePoll';

const ROLES: Role[] = ['view', 'operate', 'approve', 'administer'];

export function UsersPage() {
  const users = useLoad(() => api.listUsers());
  const projects = useLoad(() => api.listProjects());
  const toast = useToast();
  const [email, setEmail] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [globalAdmin, setGlobalAdmin] = useState(false);
  const [editIdentities, setEditIdentities] = useState<UserWithIdentities | null>(null);
  const [editGrants, setEditGrants] = useState<UserWithIdentities | null>(null);
  const [filter, setFilter] = useState('');

  const projectKey = (id: string | null) => (id === null ? 'all projects' : projects.data?.projects.find((p) => p.id === id)?.key ?? id.slice(0, 8));

  const create = async (e: FormEvent) => {
    e.preventDefault();
    try {
      await api.createUser({ email: email.trim(), display_name: displayName.trim() || undefined, is_global_admin: globalAdmin || undefined });
      toast('User added', 'ok');
      setEmail('');
      setDisplayName('');
      setGlobalAdmin(false);
      void users.refresh();
    } catch {
      /* toast shown */
    }
  };

  const list = (users.data?.users ?? []).filter((u) => {
    const f = filter.toLowerCase();
    return !f || u.email.toLowerCase().includes(f) || u.display_name.toLowerCase().includes(f) || u.identities.some((i) => i.external_id.toLowerCase().includes(f));
  });

  return (
    <div className="page">
      <div className="page-head">
        <h1>Users</h1>
        <div className="spacer" />
        <input value={filter} onChange={(e) => setFilter(e.target.value)} placeholder="Filter…" aria-label="Filter users" />
      </div>
      <form className="card row wrap" onSubmit={create}>
        <label className="inline grow">
          Email <input type="email" required value={email} onChange={(e) => setEmail(e.target.value)} />
        </label>
        <label className="inline grow">
          Display name <input value={displayName} onChange={(e) => setDisplayName(e.target.value)} />
        </label>
        <label className="check">
          <input type="checkbox" checked={globalAdmin} onChange={(e) => setGlobalAdmin(e.target.checked)} /> global admin
        </label>
        <button className="primary" type="submit" disabled={!email.trim()}>
          + Add user
        </button>
      </form>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>User</th>
              <th>Identities</th>
              <th>Role grants</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {list.map((u) => (
              <tr key={u.id}>
                <td>
                  <strong>{u.display_name || u.email}</strong>
                  {u.is_global_admin ? <span className="tag">global admin</span> : null}
                  <div className="muted small">{u.email}</div>
                </td>
                <td className="small">
                  {u.identities.length ? (
                    u.identities.map((i) => (
                      <div key={`${i.provider}:${i.external_id}`}>
                        <span className="tag">{i.provider}</span> <code>{i.external_id}</code>
                      </div>
                    ))
                  ) : (
                    <span className="muted">none linked</span>
                  )}
                </td>
                <td className="small">
                  {u.grants.length ? (
                    u.grants.map((g, i) => (
                      <div key={i}>
                        <span className={`tag role-${g.role}`}>{g.role}</span> {projectKey(g.project_id)}
                      </div>
                    ))
                  ) : (
                    <span className="muted">no grants</span>
                  )}
                </td>
                <td className="nowrap">
                  <button className="small ghost" onClick={() => setEditIdentities(u)}>
                    Identities
                  </button>{' '}
                  <button className="small ghost" onClick={() => setEditGrants(u)}>
                    Roles
                  </button>
                </td>
              </tr>
            ))}
            {!list.length ? (
              <tr>
                <td colSpan={4} className="muted empty-cell">
                  {users.loading ? 'Loading…' : 'No users.'}
                </td>
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
      {editIdentities ? (
        <IdentitiesDialog
          user={editIdentities}
          onClose={() => setEditIdentities(null)}
          onSaved={() => {
            setEditIdentities(null);
            void users.refresh();
          }}
        />
      ) : null}
      {editGrants ? (
        <GrantsDialog
          user={editGrants}
          projects={projects.data?.projects ?? []}
          onClose={() => setEditGrants(null)}
          onSaved={() => {
            setEditGrants(null);
            void users.refresh();
          }}
        />
      ) : null}
    </div>
  );
}

function IdentitiesDialog({ user, onClose, onSaved }: { user: UserWithIdentities; onClose: () => void; onSaved: () => void }) {
  const toast = useToast();
  const [rows, setRows] = useState<Identity[]>(user.identities.map((i) => ({ ...i })));
  const [busy, setBusy] = useState(false);
  const save = async () => {
    setBusy(true);
    try {
      await api.setIdentities(
        user.id,
        rows.filter((r) => r.external_id.trim()).map((r) => ({ provider: r.provider, external_id: r.external_id.trim() })),
      );
      toast('Identities saved', 'ok');
      onSaved();
    } catch {
      /* toast shown */
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title={`Identities · ${user.display_name || user.email}`} onClose={onClose}>
      <div className="form">
        <p className="help">Link this person's Jira, repo-host, Slack and Entra identities.</p>
        {rows.map((r, idx) => (
          <div className="row" key={idx}>
            <select value={r.provider} onChange={(e) => setRows((l) => l.map((x, i) => (i === idx ? { ...x, provider: e.target.value as IdentityProvider } : x)))} aria-label="Provider">
              {IDENTITY_PROVIDERS.map((p) => (
                <option key={p} value={p}>
                  {p}
                </option>
              ))}
            </select>
            <input
              className="grow"
              value={r.external_id}
              placeholder="external id"
              aria-label="External id"
              onChange={(e) => setRows((l) => l.map((x, i) => (i === idx ? { ...x, external_id: e.target.value } : x)))}
            />
            <button className="icon-btn" aria-label="Remove" onClick={() => setRows((l) => l.filter((_, i) => i !== idx))}>
              ×
            </button>
          </div>
        ))}
        <button className="ghost small" onClick={() => setRows((l) => [...l, { provider: 'slack', external_id: '' }])}>
          + Add identity
        </button>
        <div className="row end">
          <button className="ghost" onClick={onClose}>
            Cancel
          </button>
          <button className="primary" disabled={busy} onClick={save}>
            Save
          </button>
        </div>
      </div>
    </Modal>
  );
}

function GrantsDialog({ user, projects, onClose, onSaved }: { user: UserWithIdentities; projects: Project[]; onClose: () => void; onSaved: () => void }) {
  const toast = useToast();
  const [rows, setRows] = useState<Grant[]>(user.grants.map((g) => ({ ...g })));
  const [busy, setBusy] = useState(false);
  const save = async () => {
    setBusy(true);
    try {
      // Drop exact duplicates.
      const seen = new Set<string>();
      const grants = rows.filter((g) => {
        const k = `${g.project_id ?? '*'}:${g.role}`;
        if (seen.has(k)) return false;
        seen.add(k);
        return true;
      });
      await api.setGrants(user.id, grants);
      toast('Role grants saved', 'ok');
      onSaved();
    } catch {
      /* toast shown */
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal title={`Roles · ${user.display_name || user.email}`} onClose={onClose}>
      <div className="form">
        <p className="help">Roles are granted per project; "all projects" grants apply everywhere.</p>
        {rows.map((g, idx) => (
          <div className="row" key={idx}>
            <select
              className="grow"
              value={g.project_id ?? ''}
              aria-label="Project"
              onChange={(e) => setRows((l) => l.map((x, i) => (i === idx ? { ...x, project_id: e.target.value || null } : x)))}
            >
              <option value="">all projects</option>
              {projects.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.key} — {p.name}
                </option>
              ))}
            </select>
            <select value={g.role} aria-label="Role" onChange={(e) => setRows((l) => l.map((x, i) => (i === idx ? { ...x, role: e.target.value as Role } : x)))}>
              {ROLES.map((r) => (
                <option key={r} value={r}>
                  {r}
                </option>
              ))}
            </select>
            <button className="icon-btn" aria-label="Remove" onClick={() => setRows((l) => l.filter((_, i) => i !== idx))}>
              ×
            </button>
          </div>
        ))}
        <button className="ghost small" onClick={() => setRows((l) => [...l, { project_id: projects[0]?.id ?? null, role: 'view' }])}>
          + Add grant
        </button>
        <div className="row end">
          <button className="ghost" onClick={onClose}>
            Cancel
          </button>
          <button className="primary" disabled={busy} onClick={save}>
            Save
          </button>
        </div>
      </div>
    </Modal>
  );
}
