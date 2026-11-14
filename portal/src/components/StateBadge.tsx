import type { RunState, WaitingFor } from '../api';

const WAITING_LABEL: Record<WaitingFor, string> = {
  human_answer: 'answer',
  approval: 'approval',
  review: 'review',
  ci: 'CI',
  cap_decision: 'cap decision',
  input: 'input',
};

export function StateBadge({ state, waitingFor }: { state: RunState; waitingFor?: WaitingFor | null }) {
  return (
    <span className={`badge state-${state}`} title={waitingFor ? `waiting for ${waitingFor}` : state}>
      {state}
      {state === 'waiting' && waitingFor ? <span className="badge-sub"> · {WAITING_LABEL[waitingFor] ?? waitingFor}</span> : null}
    </span>
  );
}

export function NeedsYou() {
  return (
    <span className="needs-you" title="Waiting on you">
      ● needs you
    </span>
  );
}
