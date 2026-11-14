import { useState } from 'react';

/** Edit a list of strings: chips with remove buttons plus an add box. */
export function ListEditor({ value, onChange, placeholder }: { value: string[]; onChange: (v: string[]) => void; placeholder?: string }) {
  const [draft, setDraft] = useState('');
  const add = () => {
    const items = draft
      .split(/[\n,]/)
      .map((s) => s.trim())
      .filter(Boolean)
      .filter((s) => !value.includes(s));
    if (items.length) onChange([...value, ...items]);
    setDraft('');
  };
  return (
    <div className="list-editor">
      <ul className="chips">
        {value.map((v) => (
          <li key={v} className="chip">
            <code>{v}</code>
            <button className="icon-btn" aria-label={`Remove ${v}`} onClick={() => onChange(value.filter((x) => x !== v))}>
              ×
            </button>
          </li>
        ))}
        {value.length === 0 ? <li className="muted">empty</li> : null}
      </ul>
      <div className="row">
        <input
          value={draft}
          placeholder={placeholder ?? 'Add…'}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              e.preventDefault();
              add();
            }
          }}
        />
        <button onClick={add} disabled={!draft.trim()}>
          Add
        </button>
      </div>
    </div>
  );
}
