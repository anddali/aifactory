import { useState, type ReactNode } from 'react';

export function JsonBlock({ value }: { value: unknown }) {
  let text: string;
  try {
    text = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
  } catch {
    text = String(value);
  }
  return <pre className="json">{text}</pre>;
}

/** A one-line label that expands to pretty-printed JSON. */
export function Expandable({ label, value, defaultOpen = false }: { label: ReactNode; value: unknown; defaultOpen?: boolean }) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className="expandable">
      <button className="link-btn expand-toggle" onClick={() => setOpen((o) => !o)} aria-expanded={open}>
        <span className="caret">{open ? '▾' : '▸'}</span> {label}
      </button>
      {open ? <JsonBlock value={value} /> : null}
    </div>
  );
}
