import { useEffect, useState } from 'react';
import { api, type Launcher, type ModelPrice, type Settings } from '../api';
import { ListEditor } from '../components/ListEditor';
import { useToast } from '../components/Toast';
import { useLoad } from '../lib/usePoll';

const LAUNCHERS: Launcher[] = ['process', 'docker', 'fargate'];
type NumKey = 'event_retention_days' | 'tick_ms' | 'heartbeat_secs' | 'stuck_repeat_threshold' | 'stuck_noprogress_turns';
const NUM_FIELDS: [NumKey, string, string][] = [
  ['event_retention_days', 'Event retention (days)', 'Events older than this are dropped.'],
  ['tick_ms', 'Scheduler tick (ms)', ''],
  ['heartbeat_secs', 'Runner heartbeat (s)', ''],
  ['stuck_repeat_threshold', 'Stuck: repeated call threshold', 'Identical tool calls before a run is flagged stuck.'],
  ['stuck_noprogress_turns', 'Stuck: turns without progress', ''],
];

interface PriceRow {
  model: string;
  input: string;
  output: string;
}

function toRows(p: Record<string, ModelPrice>): PriceRow[] {
  return Object.entries(p)
    .sort((a, b) => a[0].localeCompare(b[0]))
    .map(([model, v]) => ({ model, input: String(v.input_per_mtok), output: String(v.output_per_mtok) }));
}

export function SettingsPage() {
  const toast = useToast();
  const loaded = useLoad(() => api.getSettings());
  const connectors = useLoad(() => api.listConnectors());
  const [s, setS] = useState<Settings | null>(null);
  const [prices, setPrices] = useState<PriceRow[]>([]);
  const [dirty, setDirty] = useState(false);
  const [busy, setBusy] = useState(false);
  const [killBusy, setKillBusy] = useState(false);

  useEffect(() => {
    if (!loaded.data) return;
    setS(loaded.data);
    setPrices(toRows(loaded.data.model_prices ?? {}));
    setDirty(false);
  }, [loaded.data]);

  if (!s) return <div className="page">{loaded.error ? <div className="banner banner-error">{loaded.error}</div> : <div className="muted">Loading…</div>}</div>;

  const set = <K extends keyof Settings>(k: K, v: Settings[K]) => {
    setS((cur) => (cur ? { ...cur, [k]: v } : cur));
    setDirty(true);
  };

  const toggleKill = async () => {
    const next = !s.kill_switch;
    const msg = next
      ? 'Engage the kill switch? No new turns will run and new runs are refused until it is released.'
      : 'Release the kill switch and let runs continue?';
    if (!window.confirm(msg)) return;
    setKillBusy(true);
    try {
      const updated = await api.updateSettings({ kill_switch: next });
      setS((cur) => (cur ? { ...cur, kill_switch: updated?.kill_switch ?? next } : cur));
      toast(next ? 'Kill switch engaged' : 'Kill switch released', next ? 'error' : 'ok');
    } catch {
      /* toast shown */
    } finally {
      setKillBusy(false);
    }
  };

  const priceErrors = prices.some((r) => r.model.trim() && (!Number.isFinite(Number(r.input)) || !Number.isFinite(Number(r.output)) || r.input === '' || r.output === ''));
  const duplicateModels = new Set(prices.map((r) => r.model.trim())).size !== prices.length;

  const save = async () => {
    const model_prices: Record<string, ModelPrice> = {};
    for (const r of prices) {
      const m = r.model.trim();
      if (!m) continue;
      model_prices[m] = { input_per_mtok: Number(r.input), output_per_mtok: Number(r.output) };
    }
    const { kill_switch: _ignored, ...rest } = s;
    void _ignored;
    setBusy(true);
    try {
      const updated = await api.updateSettings({ ...rest, model_prices });
      toast('Settings saved', 'ok');
      if (updated) {
        setS(updated);
        setPrices(toRows(updated.model_prices ?? {}));
      }
      setDirty(false);
    } catch {
      /* toast shown */
    } finally {
      setBusy(false);
    }
  };

  const modelConnectors = (connectors.data?.connectors ?? []).filter((c) => c.role === 'model');

  return (
    <div className="page">
      <div className="page-head">
        <h1>Settings</h1>
        <div className="spacer" />
        <button className="primary" disabled={!dirty || busy || priceErrors || duplicateModels} onClick={save}>
          {busy ? 'Saving…' : dirty ? 'Save changes' : 'Saved'}
        </button>
      </div>

      <section className={`kill-switch${s.kill_switch ? ' engaged' : ''}`}>
        <div>
          <h2>Kill switch</h2>
          <p>{s.kill_switch ? 'ENGAGED — no turns run and new runs are refused.' : 'Released — the factory is running normally.'}</p>
        </div>
        <button className={s.kill_switch ? 'ok big' : 'danger big'} disabled={killBusy} onClick={toggleKill}>
          {s.kill_switch ? 'Release kill switch' : 'Engage kill switch'}
        </button>
      </section>

      <div className="sections">
        <section className="card">
          <h2>Models</h2>
          <div className="grid3">
            <label>
              Model connector
              <select value={s.model_connector_id ?? ''} onChange={(e) => set('model_connector_id', e.target.value || null)}>
                <option value="">(none)</option>
                {modelConnectors.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name} ({c.kind})
                  </option>
                ))}
              </select>
            </label>
          </div>
          <h3>Price table (USD per million tokens)</h3>
          <div className="table-wrap">
            <table className="table compact">
              <thead>
                <tr>
                  <th>Model</th>
                  <th className="num">Input / Mtok</th>
                  <th className="num">Output / Mtok</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {prices.map((r, idx) => {
                  const upd = (patch: Partial<PriceRow>) => {
                    setPrices((l) => l.map((x, i) => (i === idx ? { ...x, ...patch } : x)));
                    setDirty(true);
                  };
                  return (
                    <tr key={idx}>
                      <td>
                        <input value={r.model} onChange={(e) => upd({ model: e.target.value })} aria-label="Model" />
                      </td>
                      <td className="num">
                        <input type="number" step="any" min={0} value={r.input} onChange={(e) => upd({ input: e.target.value })} aria-label="Input price" />
                      </td>
                      <td className="num">
                        <input type="number" step="any" min={0} value={r.output} onChange={(e) => upd({ output: e.target.value })} aria-label="Output price" />
                      </td>
                      <td>
                        <button
                          className="icon-btn"
                          aria-label="Remove"
                          onClick={() => {
                            setPrices((l) => l.filter((_, i) => i !== idx));
                            setDirty(true);
                          }}
                        >
                          ×
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
          {duplicateModels ? <div className="text-bad small">Duplicate model names.</div> : null}
          <button
            className="ghost small"
            onClick={() => {
              setPrices((l) => [...l, { model: '', input: '0', output: '0' }]);
              setDirty(true);
            }}
          >
            + Add model
          </button>
        </section>

        <section className="card">
          <h2>Egress allowlist</h2>
          <p className="help">Hosts runner containers may reach. Everything else is blocked.</p>
          <ListEditor value={s.egress_allowlist} onChange={(v) => set('egress_allowlist', v)} placeholder="host.example.com" />
        </section>

        <section className="card">
          <h2>Deny patterns</h2>
          <p className="help">Commands matching these patterns are refused.</p>
          <ListEditor value={s.deny_patterns} onChange={(v) => set('deny_patterns', v)} placeholder="pattern" />
        </section>

        <section className="card">
          <h2>Runtime</h2>
          <div className="grid3">
            <label>
              Launcher
              <select value={s.launcher} onChange={(e) => set('launcher', e.target.value as Launcher)}>
                {LAUNCHERS.map((l) => (
                  <option key={l} value={l}>
                    {l}
                  </option>
                ))}
              </select>
            </label>
            <label>
              Ops channel
              <input value={s.ops_channel ?? ''} onChange={(e) => set('ops_channel', e.target.value || null)} placeholder="#factory-ops" />
            </label>
            {NUM_FIELDS.map(([k, label, help]) => (
              <label key={k}>
                {label}
                <input type="number" min={0} value={s[k]} onChange={(e) => set(k, Number(e.target.value))} />
                {help ? <span className="help">{help}</span> : null}
              </label>
            ))}
          </div>
        </section>
      </div>
    </div>
  );
}
