import { useState } from 'react';
import { api, type Decision, type Question } from '../api';
import { absTime, relTime } from '../lib/format';
import { useToast } from './Toast';

const PLAN_DECISION: Record<string, string> = { approved: 'Plan approved: building', changes: 'Changes requested', rejected: 'Not building' };
const PLAN_DECISION_LABEL: Record<string, string> = { approved: 'approved', changes: 'changes requested', rejected: 'not built' };

/** Inline card for an open question or approval; answered ones render read-only. */
export function QuestionCard({ q, canAnswer, onAnswered }: { q: Question; canAnswer: boolean; onAnswered: (q: Question) => void }) {
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const toast = useToast();

  const isPlan = q.kind === 'approval' && q.action === 'plan';
  const submit = async (body: { answer?: string; decision?: Decision }) => {
    setBusy(true);
    try {
      const updated = await api.answerQuestion(q.id, body);
      const done = isPlan ? PLAN_DECISION[body.decision ?? ''] ?? 'Sent' : q.kind === 'approval' ? `Approval ${body.decision}` : 'Answer sent';
      toast(done, 'ok');
      setText('');
      onAnswered(updated);
    } catch {
      /* toast shown by client; first answer may have won elsewhere */
    } finally {
      setBusy(false);
    }
  };

  const open = q.state === 'open';
  return (
    <div className={`qcard qcard-${q.kind}${open ? ' open' : ''}`}>
      <div className="qcard-head">
        <span className="qcard-kind">{isPlan ? 'Plan for review' : q.kind === 'approval' ? 'Approval needed' : 'Question'}</span>
        {q.action && !isPlan ? <code className="qcard-action">{q.action}</code> : null}
        {q.pr_number != null ? <span className="muted">PR #{q.pr_number}</span> : null}
        <span className="muted qcard-time" title={absTime(q.created_at)}>
          {relTime(q.created_at)}
        </span>
      </div>
      <div className={`qcard-text${isPlan ? ' qcard-plan' : ''}`}>{q.text}</div>
      {isPlan ? <div className="muted small">The full plan is on the Artifacts tab.</div> : null}
      {open ? (
        canAnswer ? (
          <div className="qcard-form">
            <textarea
              value={text}
              rows={isPlan ? 3 : 2}
              placeholder={isPlan ? 'What should change? (needed for Request changes; optional note otherwise)' : q.kind === 'approval' ? 'Optional comment' : 'Your answer'}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (q.kind === 'question' && e.key === 'Enter' && (e.ctrlKey || e.metaKey) && text.trim()) void submit({ answer: text.trim() });
              }}
            />
            <div className="row">
              {isPlan ? (
                <>
                  <button className="ok" disabled={busy} onClick={() => submit({ decision: 'approved', answer: text.trim() || undefined })}>
                    Approve plan
                  </button>
                  <button disabled={busy || !text.trim()} title={text.trim() ? '' : 'Write what should change first'} onClick={() => submit({ decision: 'changes', answer: text.trim() })}>
                    Request changes
                  </button>
                  <button className="danger" disabled={busy} onClick={() => submit({ decision: 'rejected', answer: text.trim() || undefined })}>
                    Don't build
                  </button>
                </>
              ) : q.kind === 'approval' ? (
                <>
                  <button className="ok" disabled={busy} onClick={() => submit({ decision: 'approved', answer: text.trim() || undefined })}>
                    Approve
                  </button>
                  <button className="danger" disabled={busy} onClick={() => submit({ decision: 'rejected', answer: text.trim() || undefined })}>
                    Reject
                  </button>
                </>
              ) : (
                <button className="primary" disabled={busy || !text.trim()} onClick={() => submit({ answer: text.trim() })}>
                  Send answer
                </button>
              )}
            </div>
          </div>
        ) : (
          <div className="muted small">You do not hold the role needed to answer this.</div>
        )
      ) : (
        <div className="qcard-answer">
          {q.decision ? (
            <span className={`badge ${q.decision === 'approved' ? 'state-finished' : q.decision === 'changes' ? 'state-paused' : 'state-failed'}`}>
              {isPlan ? PLAN_DECISION_LABEL[q.decision] ?? q.decision : q.decision}
            </span>
          ) : null}
          {q.answer ? <span>{q.answer}</span> : null}
          <span className="muted small">
            {q.answered_by ? ` by ${q.answered_by}` : ''}
            {q.answered_via ? ` via ${q.answered_via}` : ''}
            {q.answered_at ? ` · ${relTime(q.answered_at)}` : ''}
          </span>
        </div>
      )}
    </div>
  );
}
