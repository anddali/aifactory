import { useState } from 'react';
import { api, type Question } from '../api';
import { absTime, relTime } from '../lib/format';
import { useToast } from './Toast';

/** Inline card for an open question or approval; answered ones render read-only. */
export function QuestionCard({ q, canAnswer, onAnswered }: { q: Question; canAnswer: boolean; onAnswered: (q: Question) => void }) {
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const toast = useToast();

  const submit = async (body: { answer?: string; decision?: 'approved' | 'rejected' }) => {
    setBusy(true);
    try {
      const updated = await api.answerQuestion(q.id, body);
      toast(q.kind === 'approval' ? `Approval ${body.decision}` : 'Answer sent', 'ok');
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
        <span className="qcard-kind">{q.kind === 'approval' ? 'Approval needed' : 'Question'}</span>
        {q.action ? <code className="qcard-action">{q.action}</code> : null}
        {q.pr_number != null ? <span className="muted">PR #{q.pr_number}</span> : null}
        <span className="muted qcard-time" title={absTime(q.created_at)}>
          {relTime(q.created_at)}
        </span>
      </div>
      <div className="qcard-text">{q.text}</div>
      {open ? (
        canAnswer ? (
          <div className="qcard-form">
            <textarea
              rows={2}
              value={text}
              placeholder={q.kind === 'approval' ? 'Optional comment' : 'Your answer'}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (q.kind === 'question' && e.key === 'Enter' && (e.ctrlKey || e.metaKey) && text.trim()) void submit({ answer: text.trim() });
              }}
            />
            <div className="row">
              {q.kind === 'approval' ? (
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
          {q.decision ? <span className={`badge ${q.decision === 'approved' ? 'state-finished' : 'state-failed'}`}>{q.decision}</span> : null}
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
