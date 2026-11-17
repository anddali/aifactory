import { useCallback, useEffect, useRef, useState } from 'react';

/**
 * Run `fn` immediately and then every `ms` while the tab is visible (ms <= 0
 * loads once). Returns the latest value, the last error and a manual refresh.
 */
export function usePoll<T>(fn: () => Promise<T>, ms: number, deps: unknown[] = []) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const fnRef = useRef(fn);
  fnRef.current = fn;
  // Only the latest request may update state, so a slow response for old deps (e.g. a
  // previous search) cannot overwrite a newer one.
  const seq = useRef(0);

  const refresh = useCallback(async () => {
    const mine = ++seq.current;
    try {
      const v = await fnRef.current();
      if (mine !== seq.current) return;
      setData(v);
      setError(null);
    } catch (e) {
      if (mine !== seq.current) return;
      setError((e as Error).message);
    } finally {
      if (mine === seq.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    setLoading(true);
    void refresh();
    if (ms <= 0) return;
    const id = window.setInterval(() => {
      if (document.visibilityState === 'visible') void refresh();
    }, ms);
    return () => window.clearInterval(id);
  }, [ms, refresh, ...deps]); // eslint-disable-line react-hooks/exhaustive-deps

  return { data, error, loading, refresh, setData };
}

/** Load once (and again whenever deps change). */
export function useLoad<T>(fn: () => Promise<T>, deps: unknown[] = []) {
  return usePoll(fn, 0, deps);
}
