import type { ReactNode } from 'react';

/** Filter bar above a Run tab: a search box, the tab's own controls, a match count and Clear. */
export function FilterBar({
  q,
  onQ,
  placeholder,
  shown,
  total,
  noun,
  active,
  onClear,
  children,
}: {
  q: string;
  onQ: (q: string) => void;
  placeholder: string;
  shown: number;
  total: number;
  noun: string;
  active: boolean;
  onClear: () => void;
  children?: ReactNode;
}) {
  return (
    <div className="run-filters list-filters" role="search">
      <input
        type="search"
        value={q}
        onChange={(e) => onQ(e.target.value)}
        placeholder={placeholder}
        aria-label={placeholder}
        className={`run-filters-q${q ? ' filter-on' : ''}`}
      />
      {children}
      {active ? (
        <>
          <span className="muted small list-filters-count">
            {shown} of {total} {noun}
          </span>
          <button className="ghost small" onClick={onClear}>
            Clear filters
          </button>
        </>
      ) : null}
    </div>
  );
}

/** A select whose empty value means "no filter". A selected value missing from the options stays selectable. */
export function FilterSelect({
  value,
  onChange,
  label,
  all,
  options,
}: {
  value: string;
  onChange: (v: string) => void;
  label: string;
  all: string;
  options: [string, string][];
}) {
  const opts = value && !options.some(([v]) => v === value) ? [...options, [value, value] as [string, string]] : options;
  return (
    <select value={value} onChange={(e) => onChange(e.target.value)} aria-label={label} className={value ? 'filter-on' : undefined}>
      <option value="">{all}</option>
      {opts.map(([v, l]) => (
        <option key={v} value={v}>
          {l}
        </option>
      ))}
    </select>
  );
}

/** A checkbox filter styled like the runs list's. */
export function FilterCheck({ checked, onChange, children }: { checked: boolean; onChange: (v: boolean) => void; children: ReactNode }) {
  return (
    <label className="run-filters-check">
      <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} />
      {children}
    </label>
  );
}
