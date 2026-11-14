import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from 'react';
import { onApiError } from '../api';

type Tone = 'error' | 'ok' | 'info';
interface ToastItem {
  id: number;
  tone: Tone;
  text: string;
}

const ToastContext = createContext<(text: string, tone?: Tone) => void>(() => {});

export function useToast() {
  return useContext(ToastContext);
}

export function ToastProvider({ children }: { children: ReactNode }) {
  const [items, setItems] = useState<ToastItem[]>([]);
  const nextId = useRef(1);

  const dismiss = useCallback((id: number) => setItems((l) => l.filter((t) => t.id !== id)), []);

  const push = useCallback(
    (text: string, tone: Tone = 'info') => {
      const id = nextId.current++;
      setItems((l) => {
        // Collapse repeats of the same message (e.g. a failing poll).
        const rest = l.filter((t) => t.text !== text);
        return [...rest, { id, tone, text }].slice(-5);
      });
      window.setTimeout(() => dismiss(id), tone === 'error' ? 8000 : 3500);
    },
    [dismiss],
  );

  useEffect(() => onApiError((msg) => push(msg, 'error')), [push]);

  return (
    <ToastContext.Provider value={push}>
      {children}
      <div className="toasts" role="status" aria-live="polite">
        {items.map((t) => (
          <div key={t.id} className={`toast toast-${t.tone}`}>
            <span>{t.text}</span>
            <button className="icon-btn" aria-label="Dismiss" onClick={() => dismiss(t.id)}>
              ×
            </button>
          </div>
        ))}
      </div>
    </ToastContext.Provider>
  );
}
