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

  const refresh = useCallback(async () => {
    try {
      const v = await fnRef.current();
      setData(v);
      setError(null);
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setLoading(false);
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
