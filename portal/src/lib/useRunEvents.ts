import { useCallback, useEffect, useRef, useState } from 'react';
import { api, type RunEvent, type RunSummary } from '../api';
import { hasOlder, maxSeq, mergeEvents, minSeq } from './events';

export const PAGE_LIMIT = 200;

export interface RunEventsState {
  events: RunEvent[];
  total: number;
  initialLoaded: boolean;
  loadingOlder: boolean;
  hasMore: boolean;
  live: boolean;
  loadOlder: () => Promise<void>;
}

/**
 * Event log of one run: opens at the newest page, pages backwards on demand
 * with `before=<min seq>`, and live-appends from the SSE stream. Gaps between
 * the loaded tail and a live event (or after a reconnect) are filled with
 * `after=<max seq>`.
 */
export function useRunEvents(runId: string, onState?: (s: RunSummary) => void): RunEventsState {
  const [events, setEvents] = useState<RunEvent[]>([]);
  const [total, setTotal] = useState(0);
  const [initialLoaded, setInitialLoaded] = useState(false);
  const [loadingOlder, setLoadingOlder] = useState(false);
  const [lastOlderSize, setLastOlderSize] = useState<number | null>(null);
  const [live, setLive] = useState(false);

  const eventsRef = useRef<RunEvent[]>([]);
  const onStateRef = useRef(onState);
  onStateRef.current = onState;
  const catchingUp = useRef(false);
  const olderInFlight = useRef(false);
  const initialRef = useRef(false);

  const apply = useCallback((incoming: RunEvent[]) => {
    if (!incoming.length) return;
    const next = mergeEvents(eventsRef.current, incoming);
    eventsRef.current = next;
    setEvents(next);
    setTotal((t) => Math.max(t, next.length));
  }, []);

  const catchUp = useCallback(async () => {
    if (catchingUp.current || !initialRef.current) return;
    catchingUp.current = true;
    try {
      for (let guard = 0; guard < 50; guard++) {
        const after = maxSeq(eventsRef.current) ?? 0;
        const page = await api.events(runId, { after, limit: PAGE_LIMIT });
        apply(page.events);
        setTotal((t) => Math.max(t, page.total));
        if (page.events.length < PAGE_LIMIT) break;
      }
    } catch {
      /* reported by the client */
    } finally {
      catchingUp.current = false;
    }
  }, [runId, apply]);

  // Reset and load newest page whenever the run changes.
  useEffect(() => {
    let cancelled = false;
    eventsRef.current = [];
    initialRef.current = false;
    setEvents([]);
    setTotal(0);
    setInitialLoaded(false);
    setLastOlderSize(null);
    api
      .events(runId, { limit: PAGE_LIMIT })
      .then((page) => {
        if (cancelled) return;
        apply(page.events);
        setTotal(page.total);
        initialRef.current = true;
        setInitialLoaded(true);
        // Anything that arrived on SSE beyond the page tail is already merged;
        // fill any hole between them.
        const hi = maxSeq(eventsRef.current) ?? 0;
        if (page.max_seq > hi) void catchUp();
      })
      .catch(() => {
        if (!cancelled) setInitialLoaded(true);
      });
    return () => {
      cancelled = true;
    };
  }, [runId, apply, catchUp]);

  // Live stream.
  useEffect(() => {
    if (typeof EventSource === 'undefined') return;
    const es = new EventSource(api.streamUrl(runId), { withCredentials: true });
    let hadError = false;
    es.onopen = () => {
      setLive(true);
      if (hadError) void catchUp();
      hadError = false;
    };
    es.onerror = () => {
      setLive(false);
      hadError = true;
    };
    const onEvent = (msg: MessageEvent) => {
      let ev: RunEvent;
      try {
        ev = JSON.parse(msg.data as string) as RunEvent;
      } catch {
        return;
      }
      if (typeof ev?.seq !== 'number') return;
      const hi = maxSeq(eventsRef.current);
      apply([ev]);
      if (initialRef.current && hi !== null && ev.seq > hi + 1) void catchUp();
    };
    const onStateMsg = (msg: MessageEvent) => {
      try {
        onStateRef.current?.(JSON.parse(msg.data as string) as RunSummary);
      } catch {
        /* ignore malformed */
      }
    };
    es.addEventListener('event', onEvent as EventListener);
    es.addEventListener('message', onEvent as EventListener);
    es.addEventListener('state', onStateMsg as EventListener);
    return () => {
      es.close();
      setLive(false);
    };
  }, [runId, apply, catchUp]);

  const loadOlder = useCallback(async () => {
    if (olderInFlight.current || !initialRef.current) return;
    const lo = minSeq(eventsRef.current);
    if (lo === null || !hasOlder(eventsRef.current, lastOlderSize, PAGE_LIMIT)) return;
    olderInFlight.current = true;
    setLoadingOlder(true);
    try {
      const page = await api.events(runId, { before: lo, limit: PAGE_LIMIT });
      setLastOlderSize(page.events.length);
      apply(page.events);
      setTotal((t) => Math.max(t, page.total));
    } catch {
      setLastOlderSize(0);
    } finally {
      olderInFlight.current = false;
      setLoadingOlder(false);
    }
  }, [runId, apply, lastOlderSize]);

  return {
    events,
    total,
    initialLoaded,
    loadingOlder,
    hasMore: initialLoaded && hasOlder(events, lastOlderSize, PAGE_LIMIT),
    live,
    loadOlder,
  };
}
