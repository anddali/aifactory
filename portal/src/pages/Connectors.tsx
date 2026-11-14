import { useState, type FormEvent } from 'react';
import { api, fieldLabel, fieldName, type Connector, type ConnectorKind, type FieldSpec } from '../api';
import { Modal } from '../components/Modal';
import { useToast } from '../components/Toast';
import { absTime, relTime } from '../lib/format';
import { useLoad } from '../lib/usePoll';

export function ConnectorsPage() {
  const list = useLoad(() => api.listConnectors());
  const kinds = useLoad(() => api.connectorKinds());
  const toast = useToast();
  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<Connector | null>(null);
  const [testing, setTesting] = useState<string | null>(null);
  const [results, setResults] = useState<Record<string, { ok: boolean; message: string }>>({});

  const kindOf = (k: string) => kinds.data?.kinds.find((x) => x.kind === k);

  const test = async (c: Connector) => {
    setTesting(c.id);
    try {
      const r = await api.testConnector(c.id);
      setResults((m) => ({ ...m, [c.id]: r }));
      toast(`${c.name}: ${r.ok ? 'connection OK' : 'connection failed'}`, r.ok ? 'ok' : 'error');
    } catch {
      /* toast shown */
    } finally {
      setTesting(null);
    }
  };

  const remove = async (c: Connector) => {
    if (!window.confirm(`Delete connector "${c.name}"? Projects using it will stop working.`)) return;
    try {
      await api.deleteConnector(c.id);
      toast('Connector deleted', 'ok');
      void list.refresh();
    } catch {
      /* toast shown */
    }
  };

  const connectors = list.data?.connectors ?? [];
  return (
    <div className="page">
      <div className="page-head">
        <h1>Connectors</h1>
        <div className="spacer" />
        <button className="primary" disabled={!kinds.data} onClick={() => setCreating(true)}>
          + New connector
        </button>
      </div>
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>Name</th>
              <th>Kind</th>
              <th>Role</th>
              <th>Service identity</th>
              <th>Secrets</th>
              <th>Created</th>
              <th>Test</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {connectors.map((c) => (
              <tr key={c.id}>
                <td>
                  <strong>{c.name}</strong>
                </td>
                <td>
                  <code>{c.kind}</code>
                </td>
                <td>
                  <span className="tag">{c.role}</span>
                </td>
                <td>{c.service_identity ?? <span className="muted">—</span>}</td>
                <td>
                  {c.secrets.length ? (
                    c.secrets.map((s) => (
                      <div key={s.name} className="small" title={`updated ${absTime(s.updated_at)}`}>
                        {s.name} <code className="muted">{s.hint}</code>
                      </div>
                    ))
                  ) : (
                    <span className="muted">none</span>
                  )}
                </td>
                <td className="nowrap" title={absTime(c.created_at)}>
                  {relTime(c.created_at)}
                </td>
                <td>
                  <button className="small" disabled={testing === c.id} onClick={() => test(c)}>
                    {testing === c.id ? 'Testing…' : 'Test'}
                  </button>
                  {results[c.id] ? (
                    <div className={`small ${results[c.id].ok ? 'text-ok' : 'text-bad'}`}>{results[c.id].message || (results[c.id].ok ? 'OK' : 'Failed')}</div>
                  ) : null}
                </td>
                <td className="nowrap">
                  <button className="small ghost" onClick={() => setEditing(c)}>
                    Edit
                  </button>{' '}
                  <button className="small danger" onClick={() => remove(c)}>
                    Delete
                  </button>
                </td>
              </tr>
            ))}
            {!connectors.length ? (
              <tr>
                <td colSpan={8} className="muted empty-cell">
                  {list.loading ? 'Loading…' : 'No connectors yet.'}
                </td>
              </tr>
            ) : null}
          </tbody>
        </table>
      </div>
      {creating && kinds.data ? (
        <ConnectorForm
          kinds={kinds.data.kinds}
          onClose={() => setCreating(false)}
          onSaved={() => {
            setCreating(false);
            void list.refresh();
          }}
        />
      ) : null}
      {editing ? (
        <ConnectorForm
          kinds={kinds.data?.kinds ?? []}
          existing={editing}
          kindSpec={kindOf(editing.kind)}
          onClose={() => setEditing(null)}
          onSaved={() => {
            setEditing(null);
            void list.refresh();
          }}
        />
      ) : null}
    </div>
  );
}

function fieldType(f: FieldSpec): string {
  return typeof f === 'string' ? 'string' : (f.type ?? 'string');
}

function toInput(v: unknown): string {
  if (v == null) return '';
  if (typeof v === 'string') return v;
  return JSON.stringify(v);
}

function fromInput(raw: string, type: string, original: unknown): unknown {
  if (type === 'number' || type === 'integer' || typeof original === 'number') {
    const n = Number(raw);
    return raw.trim() === '' ? null : Number.isFinite(n) ? n : raw;
  }
  if (type === 'boolean' || type === 'bool' || typeof original === 'boolean') return raw === 'true';
  if (typeof original === 'object' && original !== null) {
    try {
      return JSON.parse(raw);
    } catch {
      return raw;
    }
  }
  return raw;
}

function ConnectorForm({
  kinds,
  existing,
  kindSpec,
  onClose,
  onSaved,
}: {
  kinds: ConnectorKind[];
  existing?: Connector;
  kindSpec?: ConnectorKind;
  onClose: () => void;
  onSaved: () => void;
}) {
  const toast = useToast();
  const [kind, setKind] = useState(existing?.kind ?? kinds[0]?.kind ?? '');
  const spec = existing ? kindSpec : kinds.find((k) => k.kind === kind);
  const [name, setName] = useState(existing?.name ?? '');
  const [config, setConfig] = useState<Record<string, string>>(() => {
    const out: Record<string, string> = {};
    for (const [k, v] of Object.entries(existing?.config ?? {})) out[k] = toInput(v);
    return out;
  });
  const [secrets, setSecrets] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);

  // Fields from the kind spec, plus any extra keys already present on the connector.
  const configFields: FieldSpec[] = [...(spec?.config_fields ?? [])];
  for (const k of Object.keys(existing?.config ?? {})) if (!configFields.some((f) => fieldName(f) === k)) configFields.push(k);
  const secretFields: FieldSpec[] = [...(spec?.secret_fields ?? [])];
  for (const s of existing?.secrets ?? []) if (!secretFields.some((f) => fieldName(f) === s.name)) secretFields.push(s.name);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const cfg: Record<string, unknown> = {};
    for (const f of configFields) {
      const n = fieldName(f);
      const raw = config[n];
      if (raw === undefined || (raw === '' && existing?.config[n] === undefined)) continue;
      cfg[n] = fromInput(raw, fieldType(f), existing?.config[n]);
    }
    const sec: Record<string, string> = {};
    for (const [k, v] of Object.entries(secrets)) if (v) sec[k] = v;
    setBusy(true);
    try {
      let id: string;
      if (existing) {
        await api.updateConnector(existing.id, { name: name.trim(), config: cfg });
        id = existing.id;
      } else {
        const c = await api.createConnector({ kind, name: name.trim(), config: cfg });
        id = c.id;
      }
      if (Object.keys(sec).length) await api.setConnectorSecrets(id, sec);
      toast(existing ? 'Connector saved' : 'Connector created', 'ok');
      onSaved();
    } catch {
      /* toast shown */
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal title={existing ? `Edit ${existing.name}` : 'New connector'} onClose={onClose}>
      <form className="form" onSubmit={submit}>
        <div className="grid2">
          <label>
            Kind
            <select value={kind} disabled={!!existing} onChange={(e) => setKind(e.target.value)}>
              {kinds.map((k) => (
                <option key={k.kind} value={k.kind}>
                  {k.kind} ({k.role})
                </option>
              ))}
              {existing && !kinds.some((k) => k.kind === existing.kind) ? <option value={existing.kind}>{existing.kind}</option> : null}
            </select>
          </label>
          <label>
            Name<span className="req">*</span>
            <input required value={name} onChange={(e) => setName(e.target.value)} />
          </label>
        </div>
        {configFields.length ? <h3>Configuration</h3> : null}
        {configFields.map((f) => {
          const n = fieldName(f);
          const t = fieldType(f);
          const isBool = t === 'boolean' || t === 'bool' || typeof existing?.config[n] === 'boolean';
          return (
            <label key={n}>
              {fieldLabel(f)}
              {typeof f !== 'string' && f.required ? <span className="req">*</span> : null}
              {isBool ? (
                <select value={config[n] ?? 'false'} onChange={(e) => setConfig((c) => ({ ...c, [n]: e.target.value }))}>
                  <option value="false">false</option>
                  <option value="true">true</option>
                </select>
              ) : (
                <input
                  value={config[n] ?? ''}
                  required={typeof f !== 'string' && !!f.required}
                  placeholder={typeof f !== 'string' && f.default != null ? String(f.default) : ''}
                  onChange={(e) => setConfig((c) => ({ ...c, [n]: e.target.value }))}
                />
              )}
              {typeof f !== 'string' && f.help ? <span className="help">{f.help}</span> : null}
            </label>
          );
        })}
        {secretFields.length ? <h3>Secrets</h3> : null}
        {secretFields.length ? <p className="help">Secrets are write-only. Leave blank to keep the current value.</p> : null}
        {secretFields.map((f) => {
          const n = fieldName(f);
          const hint = existing?.secrets.find((s) => s.name === n);
          return (
            <label key={n}>
              {fieldLabel(f)} {hint ? <code className="muted small">current {hint.hint}</code> : <span className="muted small">not set</span>}
              <input
                type="password"
                autoComplete="new-password"
                value={secrets[n] ?? ''}
                placeholder={hint ? 'unchanged' : ''}
                onChange={(e) => setSecrets((s) => ({ ...s, [n]: e.target.value }))}
              />
            </label>
          );
        })}
        <div className="row end">
          <button type="button" className="ghost" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={busy || !name.trim() || !kind}>
            {busy ? 'Saving…' : existing ? 'Save' : 'Create'}
          </button>
        </div>
      </form>
    </Modal>
  );
}
