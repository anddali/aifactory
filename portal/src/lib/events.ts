import type { RunEvent } from '../api';

/**
 * Merge two lists of events into one list sorted ascending by seq with no
 * duplicate seqs. Either input may be unsorted. When the same seq appears in
 * both, the incoming event wins (it is the fresher copy).
 *
 * Fast paths keep the common cases (append a live SSE event after the newest,
 * prepend an older page before the oldest) O(n) without re-sorting.
 */
export function mergeEvents(existing: readonly RunEvent[], incoming: readonly RunEvent[]): RunEvent[] {
  if (incoming.length === 0) return existing as RunEvent[];
  const inc = isSortedUnique(incoming) ? incoming : dedupeSorted([...incoming].sort((a, b) => a.seq - b.seq));
  if (existing.length === 0) return inc.slice();
  const firstExisting = existing[0].seq;
  const lastExisting = existing[existing.length - 1].seq;
  if (inc[0].seq > lastExisting) return existing.concat(inc);
  if (inc[inc.length - 1].seq < firstExisting) return inc.concat(existing);

  // General case: two-way merge of sorted lists.
  const out: RunEvent[] = [];
  let i = 0;
  let j = 0;
  while (i < existing.length || j < inc.length) {
    const a = existing[i];
    const b = inc[j];
    if (b === undefined || (a !== undefined && a.seq < b.seq)) {
      out.push(a);
      i++;
    } else if (a === undefined || b.seq < a.seq) {
      out.push(b);
      j++;
    } else {
      out.push(b);
      i++;
      j++;
    }
  }
  return out;
}

function isSortedUnique(list: readonly RunEvent[]): boolean {
  for (let k = 1; k < list.length; k++) if (list[k].seq <= list[k - 1].seq) return false;
  return true;
}

function dedupeSorted(list: RunEvent[]): RunEvent[] {
  const out: RunEvent[] = [];
  for (const e of list) {
    if (out.length && out[out.length - 1].seq === e.seq) out[out.length - 1] = e;
    else out.push(e);
  }
  return out;
}

/** Lowest seq loaded, or null when nothing is loaded. Used as `before=` for the next older page. */
export function minSeq(list: readonly RunEvent[]): number | null {
  return list.length ? list[0].seq : null;
}

/** Highest seq loaded, or null. Used as `after=` to catch up after an SSE reconnect. */
export function maxSeq(list: readonly RunEvent[]): number | null {
  return list.length ? list[list.length - 1].seq : null;
}

/**
 * Whether older events may exist on the server. Seqs are assumed to start at 1;
 * we also stop when an older page came back short.
 */
export function hasOlder(list: readonly RunEvent[], lastOlderPageSize: number | null, pageLimit: number): boolean {
  const lo = minSeq(list);
  if (lo === null) return false;
  if (lo <= 1) return false;
  if (lastOlderPageSize !== null && lastOlderPageSize < pageLimit) return false;
  return true;
}

// Event classification for the Run view tabs.

export const CONVERSATION_KINDS = new Set(['human_message', 'question_asked', 'question_answered', 'harness.agent_message', 'steering']);
export const TOOL_KINDS = new Set(['harness.tool_call', 'harness.tool_result', 'primitive_call']);

export function isConversation(e: RunEvent): boolean {
  return CONVERSATION_KINDS.has(e.kind);
}

export function isTool(e: RunEvent): boolean {
  return TOOL_KINDS.has(e.kind);
}

/** One-line human summary of an event for the activity list. */
export function summarize(e: RunEvent): string {
  const p = e.payload ?? {};
  const s = (k: string) => (typeof p[k] === 'string' ? (p[k] as string) : p[k] == null ? '' : JSON.stringify(p[k]));
  switch (e.kind) {
    case 'state_changed':
      return `${s('from')} → ${s('to')}${p.waiting_for ? ` (waiting for ${s('waiting_for')})` : ''}${p.reason ? ` — ${s('reason')}` : ''}`;
    case 'harness.tool_call':
      return `${s('name')}(${truncate(s('args'), 140)})`;
    case 'harness.agent_message':
    case 'human_message':
    case 'steering':
      return truncate(s('text'), 240);
    case 'question_asked':
      return `[${s('kind')}] ${truncate(s('text'), 220)}`;
    case 'primitive_call':
      return `${s('name')} ${p.ok === false ? 'failed' : 'ok'}${p.replayed ? ' (replayed)' : ''}`;
    default: {
      const json = JSON.stringify(p);
      return json === '{}' ? '' : truncate(json, 200);
    }
  }
}

export function truncate(s: string, n: number): string {
  return s.length > n ? `${s.slice(0, n - 1)}…` : s;
}
