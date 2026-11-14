import { useCallback, useEffect, useMemo, useRef, useState, type FormEvent, type ReactNode } from 'react';
import { Link, useParams } from 'react-router';
import { api, hasRole, TERMINAL_STATES, type Question, type RunDetail, type RunEvent, type RunSummary } from '../api';
import { ActivityRow, ChatRow, ToolRow } from '../components/EventViews';
import { JsonBlock } from '../components/JsonView';
import { QuestionCard } from '../components/QuestionCard';
import { NeedsYou, StateBadge } from '../components/StateBadge';
import { Tabs } from '../components/Tabs';
import { useToast } from '../components/Toast';
import { VirtualList } from '../components/VirtualList';
import { useAuth } from '../lib/auth';
import { isConversation, isTool } from '../lib/events';
import { absTime, num, optNum, relTime, usd, workflowLabel } from '../lib/format';
import { useRunEvents } from '../lib/useRunEvents';
import { CostCell } from './Dashboard';

type Tab = 'activity' | 'conversation' | 'tools' | 'artifacts' | 'cost' | 'questions';

/** Event kinds after which the run detail (questions, artifacts, cost) is refetched. */
const DETAIL_KINDS = new Set(['question_asked', 'question_answered', 'artifact', 'cost', 'harness.usage', 'state_changed', 'runner_launched', 'runner_exited', 'runner_lost']);

const eventKey = (e: RunEvent) => e.seq;

export function RunPage() {
  const { id = '' } = useParams();
  const { user } = useAuth();
  const [run, setRun] = useState<RunDetail | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>('activity');

  const refetch = useCallback(async () => {
    try {
      const d = await api.getRun(id);
      setRun(d);
      setLoadError(null);
    } catch (e) {
      setLoadError((e as Error).message);
    }
  }, [id]);

  useEffect(() => {
    setRun(null);
    void refetch();
    const t = window.setInterval(() => document.visibilityState === 'visible' && void refetch(), 10000);
    return () => window.clearInterval(t);
  }, [refetch]);

  const onState = useCallback((s: RunSummary) => setRun((r) => (r ? { ...r, ...s } : r)), []);
  const ev = useRunEvents(id, onState);

  // Debounced detail refresh when relevant events stream in.
  const lastSeen = useRef(0);
  const debounce = useRef<number | null>(null);
  useEffect(() => {
    const fresh = ev.events.filter((e) => e.seq > lastSeen.current);
    if (!fresh.length) return;
    const first = lastSeen.current === 0;
    lastSeen.current = ev.events[ev.events.length - 1].seq;
    if (first) return;
    if (fresh.some((e) => DETAIL_KINDS.has(e.kind))) {
      if (debounce.current) window.clearTimeout(debounce.current);
      debounce.current = window.setTimeout(() => void refetch(), 400);
    }
  }, [ev.events, refetch]);
  useEffect(() => () => void (debounce.current && window.clearTimeout(debounce.current)), []);

  const conversation = useMemo(() => ev.events.filter(isConversation), [ev.events]);
  const tools = useMemo(() => ev.events.filter(isTool), [ev.events]);

  if (!run) {
    return <div className="page">{loadError ? <div className="banner banner-error">{loadError}</div> : <div className="muted">Loading run…</div>}</div>;
  }

  const canOperate = hasRole(user, 'operate', run.project_id);
  const canApprove = hasRole(user, 'approve', run.project_id);
  const openQuestions = run.questions.filter((q) => q.state === 'open');
  const onAnswered = (q: Question) => setRun((r) => (r ? { ...r, questions: r.questions.map((x) => (x.id === q.id ? q : x)) } : r));

  const renderActivity = (e: RunEvent) => <ActivityRow e={e} />;
  const renderChat = (e: RunEvent) => <ChatRow e={e} />;
  const renderTool = (e: RunEvent) => <ToolRow e={e} />;
  const listProps = { hasMore: ev.hasMore, loadingOlder: ev.loadingOlder, onLoadOlder: ev.loadOlder, getKey: eventKey };

  return (
    <div className="page run-page">
      <RunHeader run={run} live={ev.live} />
      {canOperate ? <RunControls run={run} onDone={refetch} /> : null}
      {run.waiting_for === 'cap_decision' && run.state === 'waiting' && canOperate ? <CapDecision run={run} onDone={refetch} /> : null}
      {openQuestions.length ? (
        <section className="open-questions">
          {openQuestions.map((q) => (
            <QuestionCard key={q.id} q={q} canAnswer={q.kind === 'approval' ? canApprove : canOperate} onAnswered={onAnswered} />
          ))}
        </section>
      ) : null}
      <Tabs<Tab>
        tabs={[
          { key: 'activity', label: 'Activity', count: Math.max(ev.total, run.event_count) },
          { key: 'conversation', label: 'Conversation', count: conversation.length },
          { key: 'tools', label: 'Tool trace', count: tools.length },
          { key: 'artifacts', label: 'Artifacts', count: run.artifacts.length },
          { key: 'cost', label: 'Cost' },
          { key: 'questions', label: 'Questions', count: run.questions.length },
        ]}
        active={tab}
        onChange={setTab}
      />
      <div className="run-pane">
        {tab === 'activity' ? (
          <VirtualList key="activity" items={ev.events} render={renderActivity} estimateSize={30} empty={ev.initialLoaded ? 'No events yet.' : 'Loading events…'} {...listProps} />
        ) : null}
        {tab === 'conversation' ? (
          <VirtualList key="conversation" items={conversation} render={renderChat} estimateSize={72} empty="No conversation in the loaded events." {...listProps} />
        ) : null}
        {tab === 'tools' ? (
          <VirtualList key="tools" items={tools} render={renderTool} estimateSize={30} empty="No tool calls in the loaded events." {...listProps} />
        ) : null}
        {tab === 'artifacts' ? <ArtifactsPane run={run} /> : null}
        {tab === 'cost' ? <CostPane run={run} /> : null}
        {tab === 'questions' ? (
          <div className="scroll-pane">
            {run.questions.length === 0 ? <div className="muted">No questions or approvals.</div> : null}
            {run.questions.map((q) => (
              <QuestionCard key={q.id} q={q} canAnswer={q.kind === 'approval' ? canApprove : canOperate} onAnswered={onAnswered} />
            ))}
          </div>
        ) : null}
      </div>
    </div>
  );
}

function RunHeader({ run, live }: { run: RunDetail; live: boolean }) {
  return (
    <section className="run-header">
      <div className="run-title-row">
        <Link to="/" className="muted back">
          ← Runs
        </Link>
        <h1>{run.title || '(untitled run)'}</h1>
        <StateBadge state={run.state} waitingFor={run.waiting_for} />
        {run.needs_you ? <NeedsYou /> : null}
        <span className={`live-dot${live ? ' on' : ''}`} title={live ? 'Live stream connected' : 'Live stream disconnected'}>
          {live ? 'live' : 'offline'}
        </span>
      </div>
      {run.pause_reason && run.state === 'paused' ? <div className="banner banner-warn">Paused: {run.pause_reason}</div> : null}
      <dl className="facts">
        <Fact k="Project">{run.project_key}</Fact>
        <Fact k="Workflow">{workflowLabel(run.workflow)}</Fact>
        <Fact k="Cost">
          <CostCell spent={run.spent_usd} budget={run.budget_usd} />
        </Fact>
        <Fact k="Turns">
          {run.turns_used}/{run.max_turns}
        </Fact>
        <Fact k="Model">{run.model}</Fact>
        <Fact k="Harness">{run.harness}</Fact>
        {run.ticket_key ? <Fact k="Ticket">{run.ticket_key}</Fact> : null}
        {run.repo ? <Fact k="Repo">{run.repo}</Fact> : null}
        {run.branch ? (
          <Fact k="Branch">
            <code>{run.branch}</code>
          </Fact>
        ) : null}
        {run.pr_number != null || run.pr_url ? (
          <Fact k="PR">
            {run.pr_url ? (
              <a href={run.pr_url} target="_blank" rel="noreferrer">
                #{run.pr_number ?? 'open'} ↗
              </a>
            ) : (
              `#${run.pr_number}`
            )}
          </Fact>
        ) : null}
        {run.assignee ? <Fact k="Assignee">{run.assignee.display_name || run.assignee.email}</Fact> : null}
        {run.runner ? (
          <Fact k="Runner">
            {run.runner.state} · gen {run.runner.generation}
            {run.runner.last_heartbeat_at ? <span className="muted"> · ♥ {relTime(run.runner.last_heartbeat_at)}</span> : null}
          </Fact>
        ) : null}
        <Fact k="Last activity">
          <span title={absTime(run.last_activity_at)}>{relTime(run.last_activity_at)}</span>
        </Fact>
      </dl>
    </section>
  );
}

function Fact({ k, children }: { k: string; children: ReactNode }) {
  return (
    <div className="fact">
      <dt>{k}</dt>
      <dd>{children}</dd>
    </div>
  );
}

function RunControls({ run, onDone }: { run: RunDetail; onDone: () => void }) {
  const toast = useToast();
  const [text, setText] = useState('');
  const [interrupt, setInterrupt] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const terminal = TERMINAL_STATES.includes(run.state);

  const act = async (name: string, fn: () => Promise<unknown>, confirmText?: string) => {
    if (confirmText && !window.confirm(confirmText)) return;
    setBusy(name);
    try {
      await fn();
      toast(`${name} requested`, 'ok');
      onDone();
    } catch {
      /* toast shown */
    } finally {
      setBusy(null);
    }
  };

  const steer = async (e: FormEvent) => {
    e.preventDefault();
    if (!text.trim()) return;
    await act('Steer', () => api.steer(run.id, text.trim(), interrupt));
    setText('');
    setInterrupt(false);
  };

  return (
    <section className="controls">
      <div className="control-buttons">
        {!terminal && run.state !== 'paused' ? (
          <button disabled={!!busy} onClick={() => act('Pause', () => api.pause(run.id))}>
            Pause
          </button>
        ) : null}
        {run.state === 'paused' ? (
          <button className="primary" disabled={!!busy} onClick={() => act('Resume', () => api.resume(run.id))}>
            Resume
          </button>
        ) : null}
        {run.state === 'expired' ? (
          <button className="primary" disabled={!!busy} onClick={() => act('Revive', () => api.revive(run.id))}>
            Revive
          </button>
        ) : null}
        {!terminal ? (
          <button className="danger" disabled={!!busy} onClick={() => act('Cancel', () => api.cancel(run.id), 'Cancel this run? This cannot be undone.')}>
            Cancel
          </button>
        ) : null}
        <button
          className="ghost"
          disabled={!!busy}
          title="Stop the runner container now; the run keeps its state"
          onClick={() => act('Tear down', () => api.teardown(run.id), 'Stop the runner container now? The run keeps its state.')}
        >
          Tear down
        </button>
      </div>
      {!terminal ? (
        <form className="steer" onSubmit={steer}>
          <input value={text} onChange={(e) => setText(e.target.value)} placeholder="Steer the agent… (delivered at the next turn boundary)" aria-label="Steering message" />
          <label className="check" title="Stop the current turn and start a new one with this message">
            <input type="checkbox" checked={interrupt} onChange={(e) => setInterrupt(e.target.checked)} /> interrupt now
          </label>
          <button type="submit" className="primary" disabled={!text.trim() || !!busy}>
            Steer
          </button>
        </form>
      ) : null}
    </section>
  );
}

function CapDecision({ run, onDone }: { run: RunDetail; onDone: () => void }) {
  const toast = useToast();
  const [turns, setTurns] = useState(String(run.max_turns + 10));
  const [budget, setBudget] = useState(String(Math.ceil(run.budget_usd * 1.5)));
  const [busy, setBusy] = useState(false);

  const send = async (action: 'raise' | 'end') => {
    if (action === 'end' && !window.confirm('End this run at its cap?')) return;
    setBusy(true);
    try {
      await api.cap(run.id, action === 'raise' ? { action, max_turns: optNum(turns), budget_usd: optNum(budget) } : { action });
      toast(action === 'raise' ? 'Caps raised' : 'Run ended', 'ok');
      onDone();
    } catch {
      /* toast shown */
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="cap-decision">
      <div className="cap-head">
        <strong>Cap reached.</strong> The run used {run.turns_used}/{run.max_turns} turns and {usd(run.spent_usd)} of {usd(run.budget_usd)}. Raise the caps or end it.
      </div>
      <div className="row wrap">
        <label className="inline">
          Max turns <input inputMode="numeric" value={turns} onChange={(e) => setTurns(e.target.value)} />
        </label>
        <label className="inline">
          Budget USD <input inputMode="decimal" value={budget} onChange={(e) => setBudget(e.target.value)} />
        </label>
        <button className="primary" disabled={busy} onClick={() => send('raise')}>
          Raise and continue
        </button>
        <button className="danger" disabled={busy} onClick={() => send('end')}>
          End run
        </button>
      </div>
    </section>
  );
}

function ArtifactsPane({ run }: { run: RunDetail }) {
  if (!run.artifacts.length) return <div className="scroll-pane muted">No artifacts yet.</div>;
  return (
    <div className="scroll-pane">
      {run.artifacts.map((a) => (
        <div key={a.id} className="artifact">
          <div className="artifact-head">
            <span className="tag">{a.kind}</span>
            {a.url ? (
              <a href={a.url} target="_blank" rel="noreferrer">
                {a.title || a.url} ↗
              </a>
            ) : (
              <strong>{a.title}</strong>
            )}
            <span className="muted small" title={absTime(a.created_at)}>
              {relTime(a.created_at)}
            </span>
          </div>
          {a.content ? (
            <details>
              <summary>Content</summary>
              <JsonBlock value={a.content} />
            </details>
          ) : null}
        </div>
      ))}
    </div>
  );
}

function CostPane({ run }: { run: RunDetail }) {
  const totals = run.cost_by_model.reduce(
    (t, r) => ({ i: t.i + r.input_tokens, o: t.o + r.output_tokens, c: t.c + r.cost_usd }),
    { i: 0, o: 0, c: 0 },
  );
  return (
    <div className="scroll-pane">
      <div className="table-wrap">
        <table className="table">
          <thead>
            <tr>
              <th>Model</th>
              <th className="num">Input tokens</th>
              <th className="num">Output tokens</th>
              <th className="num">Cost</th>
            </tr>
          </thead>
          <tbody>
            {run.cost_by_model.map((r) => (
              <tr key={r.model}>
                <td>{r.model}</td>
                <td className="num">{num(r.input_tokens)}</td>
                <td className="num">{num(r.output_tokens)}</td>
                <td className="num">{usd(r.cost_usd, 4)}</td>
              </tr>
            ))}
            {!run.cost_by_model.length ? (
              <tr>
                <td colSpan={4} className="muted empty-cell">
                  No model usage yet.
                </td>
              </tr>
            ) : (
              <tr className="total-row">
                <td>Total</td>
                <td className="num">{num(totals.i)}</td>
                <td className="num">{num(totals.o)}</td>
                <td className="num">{usd(totals.c, 4)}</td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
      <dl className="facts small-facts">
        <Fact k="Budget">{usd(run.budget_usd)}</Fact>
        <Fact k="Spent">{usd(run.spent_usd, 4)}</Fact>
        <Fact k="Template">{String(run.template_version ?? '—')}</Fact>
        <Fact k="Policy">{String(run.policy_version ?? '—')}</Fact>
        <Fact k="Config">{String(run.config_version ?? '—')}</Fact>
      </dl>
    </div>
  );
}
