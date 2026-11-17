import type { RunEvent } from '../api';
import { isFailure, summarize } from '../lib/events';
import { absTime, clockTime } from '../lib/format';
import { Expandable } from './JsonView';

function kindClass(kind: string): string {
  if (kind.includes('error') || kind === 'runner_lost' || kind === 'stuck_detected') return 'k-bad';
  if (kind.startsWith('question') || kind === 'human_message' || kind === 'steering') return 'k-human';
  if (kind.startsWith('harness.tool') || kind === 'primitive_call') return 'k-tool';
  if (kind === 'state_changed' || kind.startsWith('runner_')) return 'k-state';
  if (kind === 'cost' || kind === 'harness.usage') return 'k-cost';
  return '';
}

/** Compact one-row rendering for the Activity tab; click to expand payload. */
export function ActivityRow({ e }: { e: RunEvent }) {
  return (
    <div className="ev-row">
      <span className="ev-seq">#{e.seq}</span>
      <span className="ev-time" title={absTime(e.ts)}>
        {clockTime(e.ts)}
      </span>
      <div className="ev-main">
        <Expandable
          label={
            <>
              <span className={`ev-kind ${kindClass(e.kind)}`}>{e.kind}</span>
              {e.actor ? <span className="ev-actor">{e.actor}</span> : null}
              <span className="ev-sum">{summarize(e)}</span>
            </>
          }
          value={e.payload}
        />
      </div>
    </div>
  );
}

/** Chat-bubble rendering for the Conversation tab. */
export function ChatRow({ e }: { e: RunEvent }) {
  const p = e.payload ?? {};
  const str = (k: string) => (typeof p[k] === 'string' ? (p[k] as string) : '');
  let side: 'agent' | 'human' | 'system' = 'system';
  let who = e.actor ?? '';
  let text = '';
  let tag = '';
  switch (e.kind) {
    case 'harness.agent_message':
      side = 'agent';
      who = who || 'agent';
      text = str('text');
      break;
    case 'human_message': {
      const dir = str('direction');
      side = dir === 'from_human' ? 'human' : 'agent';
      const user = p.user;
      who =
        dir === 'from_human'
          ? (typeof user === 'string' ? user : (user as { display_name?: string; email?: string } | null)?.display_name ?? e.actor ?? 'human')
          : 'agent → human';
      text = str('text');
      break;
    }
    case 'question_asked':
      side = 'agent';
      who = 'agent';
      tag = str('kind') || 'question';
      text = str('text');
      break;
    case 'question_answered':
      side = 'human';
      who = str('answered_by') || e.actor || 'human';
      tag = str('decision') || 'answer';
      text = str('answer') || str('decision') || JSON.stringify(p);
      break;
    case 'steering':
      side = 'human';
      who = e.actor ?? 'operator';
      tag = [p.interrupt ? 'steer · interrupt' : 'steer', str('via')].filter(Boolean).join(' · ');
      text = str('text');
      break;
    default:
      text = summarize(e);
  }
  return (
    <div className={`chat-row chat-${side}`}>
      <div className="chat-bubble">
        <div className="chat-meta">
          <span className="chat-who">{who}</span>
          {tag ? <span className="chat-tag">{tag}</span> : null}
          <span className="chat-time" title={absTime(e.ts)}>
            {clockTime(e.ts)}
          </span>
        </div>
        <div className="chat-text">{text}</div>
      </div>
    </div>
  );
}

/** Tool trace rendering: call / result / primitive with expandable JSON. */
export function ToolRow({ e }: { e: RunEvent }) {
  const p = e.payload ?? {};
  const name = typeof p.name === 'string' ? p.name : '';
  const callId = typeof p.call_id === 'string' ? p.call_id : '';
  let label: string;
  let cls = '';
  if (e.kind === 'harness.tool_call') {
    label = `→ ${name || 'tool'}`;
  } else if (e.kind === 'harness.tool_result') {
    label = `← ${name || 'result'}`;
    cls = isFailure(e) ? 'k-bad' : '';
  } else {
    label = `◆ ${name || 'primitive'} ${p.ok === false ? '✗' : '✓'}${p.replayed ? ' replayed' : ''}`;
    cls = p.ok === false ? 'k-bad' : 'k-prim';
  }
  return (
    <div className="ev-row tool-row">
      <span className="ev-seq">#{e.seq}</span>
      <span className="ev-time" title={absTime(e.ts)}>
        {clockTime(e.ts)}
      </span>
      <div className="ev-main">
        <Expandable
          label={
            <>
              <span className={`ev-kind k-tool ${cls}`}>{label}</span>
              {callId ? <span className="ev-actor">{callId}</span> : null}
              <span className="ev-sum">{summarize(e)}</span>
            </>
          }
          value={p}
        />
      </div>
    </div>
  );
}
