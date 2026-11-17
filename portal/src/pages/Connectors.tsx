import { useState, type FormEvent } from 'react';
import { api, type Connector, type ConnectorKind, type FieldSpec, type ShowIf } from '../api';
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
                  {kindOf(c.kind)?.label ?? <code>{c.kind}</code>}
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

type Spec = Exclude<FieldSpec, string>;
type TestResult = { ok: boolean; message: string };

const ROLE_LABELS: Record<string, string> = { ticket: 'Tickets', repo: 'Repositories', chat: 'Chat', model: 'Models', feed: 'Package feeds' };

function spec(f: FieldSpec): Spec {
  return typeof f === 'string' ? { name: f } : f;
}

function toInput(v: unknown): string {
  if (v == null) return '';
  if (typeof v === 'string') return v;
  if (Array.isArray(v) && v.every((x) => typeof x === 'string')) return v.join(', ');
  return JSON.stringify(v);
}

function fromInput(raw: string, type: string, original: unknown): unknown {
  if (type === 'list' || Array.isArray(original)) {
    return raw
      .split(',')
      .map((s) => s.trim())
      .filter(Boolean);
  }
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
  return raw.trim();
}

function inputType(type: string | undefined): string {
  return type === 'url' ? 'url' : type === 'email' ? 'email' : 'text';
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
  const [kind, setKind] = useState(existing?.kind ?? kinds.find((k) => !k.kind.startsWith('fake_'))?.kind ?? kinds[0]?.kind ?? '');
  const kspec = existing ? kindSpec : kinds.find((k) => k.kind === kind);
  const [name, setName] = useState(existing?.name ?? '');
  const [config, setConfig] = useState<Record<string, string>>(() => {
    const out: Record<string, string> = {};
    for (const [k, v] of Object.entries(existing?.config ?? {})) out[k] = toInput(v);
    return out;
  });
  const [secrets, setSecrets] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [testing, setTesting] = useState(false);
  const [result, setResult] = useState<TestResult | null>(null);

  // Fields from the kind spec, plus any extra keys already present on the connector.
  const configFields: Spec[] = (kspec?.config_fields ?? []).map(spec);
  for (const k of Object.keys(existing?.config ?? {})) if (!configFields.some((f) => f.name === k)) configFields.push({ name: k });
  const secretFields: Spec[] = (kspec?.secret_fields ?? []).map(spec);
  for (const s of existing?.secrets ?? []) if (!secretFields.some((f) => f.name === s.name)) secretFields.push({ name: s.name });

  const hintFor = (n: string) => existing?.secrets.find((s) => s.name === n);
  // Effective value of a config field (typed, else its default) and conditional visibility.
  const valueOf = (n: string) => (config[n] ?? '').trim() || String(configFields.find((f) => f.name === n)?.default ?? '');
  const holds = (c?: ShowIf | null) => !c || valueOf(c.field) === c.equals;
  const shownConfig = configFields.filter((f) => holds(f.show_if));
  const shownSecrets = secretFields.filter((f) => holds(f.show_if));
  const missing = [
    ...shownConfig.filter((f) => f.required && !(config[f.name] ?? '').trim()),
    ...shownSecrets.filter((f) => f.required && !secrets[f.name] && !hintFor(f.name)),
  ];

  const changeKind = (k: string) => {
    setKind(k);
    setConfig({});
    setSecrets({});
    setResult(null);
  };
  const setCfg = (n: string, v: string) => {
    setConfig((c) => ({ ...c, [n]: v }));
    setResult(null);
  };
  const setSecret = (n: string, v: string) => {
    setSecrets((s) => ({ ...s, [n]: v }));
    setResult(null);
  };

  const buildConfig = () => {
    const cfg: Record<string, unknown> = {};
    for (const f of configFields) {
      // Selects store their default explicitly, so the saved connector says which mode it is in.
      const raw = config[f.name] ?? (f.options?.length && existing?.config[f.name] === undefined && f.default != null ? String(f.default) : undefined);
      if (raw === undefined || (raw.trim() === '' && existing?.config[f.name] === undefined)) continue;
      cfg[f.name] = fromInput(raw, f.type ?? 'string', existing?.config[f.name]);
    }
    return cfg;
  };
  const filledSecrets = () => Object.fromEntries(Object.entries(secrets).filter(([k, v]) => v && shownSecrets.some((f) => f.name === k)));

  const runTest = async () => {
    setTesting(true);
    setResult(null);
    try {
      setResult(await api.testConnectorDraft({ id: existing?.id, kind, config: buildConfig(), secrets: filledSecrets() }));
    } catch (e) {
      setResult({ ok: false, message: e instanceof Error ? e.message : 'Request failed' });
    } finally {
      setTesting(false);
    }
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    const cfg = buildConfig();
    const sec = filledSecrets();
    const finalName = name.trim() || kspec?.label || kind;
    setBusy(true);
    try {
      let id: string;
      if (existing) {
        await api.updateConnector(existing.id, { name: finalName, config: cfg });
        id = existing.id;
      } else {
        const c = await api.createConnector({ kind, name: finalName, config: cfg });
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

  const roles = [...new Set(kinds.map((k) => k.role))];
  const webhookUrl = kspec?.webhook_url && holds(kspec.webhook_show_if) ? (existing ? kspec.webhook_url.replace('{id}', existing.id) : null) : undefined;

  const renderField = (f: Spec, secret: boolean) => {
    const n = f.name;
    const hint = secret ? hintFor(n) : undefined;
    const isBool = !secret && (f.type === 'boolean' || f.type === 'bool' || typeof existing?.config[n] === 'boolean');
    return (
      <label key={n} className="cf-field">
        <span className="cf-label">
          {f.label ?? n}
          {f.required ? <span className="req">*</span> : f.options?.length ? null : <span className="cf-opt">optional</span>}
          {secret && hint ? <span className="cf-saved">saved · {hint.hint}</span> : null}
        </span>
        {!secret && f.options?.length ? (
          <select value={valueOf(n)} onChange={(e) => setCfg(n, e.target.value)}>
            {f.options.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </select>
        ) : isBool ? (
          <select value={config[n] ?? 'false'} onChange={(e) => setCfg(n, e.target.value)}>
            <option value="false">false</option>
            <option value="true">true</option>
          </select>
        ) : secret ? (
          <input
            type="password"
            autoComplete="new-password"
            value={secrets[n] ?? ''}
            placeholder={hint ? 'Leave blank to keep the saved value' : (f.placeholder ?? '')}
            onChange={(e) => setSecret(n, e.target.value)}
          />
        ) : (
          <input
            type={inputType(f.type)}
            value={config[n] ?? ''}
            placeholder={f.placeholder ?? (f.default != null ? String(f.default) : '')}
            onChange={(e) => setCfg(n, e.target.value)}
          />
        )}
        {f.help ? <span className="help">{f.help}</span> : null}
      </label>
    );
  };

  return (
    <Modal title={existing ? `Edit ${existing.name}` : 'New connector'} onClose={onClose}>
      <form className="form cf" onSubmit={submit}>
        <div className="grid2">
          <label>
            Type
            <select value={kind} disabled={!!existing} onChange={(e) => changeKind(e.target.value)}>
              {roles.map((r) => (
                <optgroup key={r} label={ROLE_LABELS[r] ?? r}>
                  {kinds
                    .filter((k) => k.role === r)
                    .map((k) => (
                      <option key={k.kind} value={k.kind}>
                        {k.label ?? k.kind}
                      </option>
                    ))}
                </optgroup>
              ))}
              {existing && !kinds.some((k) => k.kind === existing.kind) ? <option value={existing.kind}>{existing.kind}</option> : null}
            </select>
          </label>
          <label>
            Name
            <input value={name} placeholder={kspec?.label ?? kind} onChange={(e) => setName(e.target.value)} />
          </label>
        </div>
        {kspec?.description ? <p className="help cf-desc">{kspec.description}</p> : null}

        {configFields.length ? (
          <fieldset className="cf-section">
            <legend>Connection</legend>
            {shownConfig.map((f) => renderField(f, false))}
          </fieldset>
        ) : null}

        {secretFields.length ? (
          <fieldset className="cf-section">
            <legend>Credentials</legend>
            <p className="help">Stored encrypted and never shown again.</p>
            {shownSecrets.map((f) => renderField(f, true))}
          </fieldset>
        ) : null}

        {webhookUrl !== undefined ? (
          <fieldset className="cf-section">
            <legend>Webhook</legend>
            {webhookUrl ? (
              <>
                <div className="cf-copy">
                  <code>{webhookUrl}</code>
                  <button
                    type="button"
                    className="small ghost"
                    onClick={() => {
                      void navigator.clipboard?.writeText(webhookUrl);
                      toast('Webhook URL copied', 'ok');
                    }}
                  >
                    Copy
                  </button>
                </div>
                <p className="help">Point the service's webhook here. Optional: without it, changes are picked up by polling.</p>
              </>
            ) : (
              <p className="help">The webhook URL is shown here once the connector is created. Optional: without it, changes are picked up by polling.</p>
            )}
          </fieldset>
        ) : null}

        {result ? (
          <div className={`cf-result ${result.ok ? 'ok' : 'bad'}`} role="status">
            <strong>{result.ok ? 'Connected' : 'Connection failed'}</strong>
            <span>{result.message}</span>
          </div>
        ) : null}

        <div className="cf-foot">
          <button type="button" disabled={testing || missing.length > 0 || !kind} onClick={runTest}>
            {testing ? 'Testing…' : 'Test connection'}
          </button>
          <span className="help cf-missing">
            {missing.length ? `Needs ${missing.map((f) => f.label ?? f.name).join(', ')}` : 'Checks the settings above without saving'}
          </span>
          <div className="spacer" />
          <button type="button" className="ghost" onClick={onClose}>
            Cancel
          </button>
          <button type="submit" className="primary" disabled={busy || !kind || missing.length > 0}>
            {busy ? 'Saving…' : existing ? 'Save' : 'Create'}
          </button>
        </div>
      </form>
    </Modal>
  );
}

