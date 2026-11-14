import { useVirtualizer } from '@tanstack/react-virtual';
import { useCallback, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react';

const END_THRESHOLD = 48;
const LOAD_OLDER_THRESHOLD = 300;

export interface VirtualListProps<T> {
  items: T[];
  getKey: (item: T) => string | number;
  render: (item: T, index: number) => ReactNode;
  estimateSize?: number;
  hasMore?: boolean;
  loadingOlder?: boolean;
  onLoadOlder?: () => void;
  empty?: ReactNode;
  className?: string;
}

/**
 * Windowed, bottom-anchored list for event logs:
 * - opens scrolled to the newest item,
 * - keeps following new items while the user is at the bottom,
 * - keeps the viewport steady when older items are prepended,
 * - asks for older items when scrolled near the top,
 * - shows "Jump to latest" when scrolled away from the bottom.
 */
export function VirtualList<T>({
  items,
  getKey,
  render,
  estimateSize = 40,
  hasMore,
  loadingOlder,
  onLoadOlder,
  empty,
  className,
}: VirtualListProps<T>) {
  const scrollRef = useRef<HTMLDivElement>(null);
  const [atEnd, setAtEnd] = useState(true);
  const [unseen, setUnseen] = useState(0);
  const initialScrollDone = useRef(false);
  const lastCount = useRef(items.length);
  const lastKey = useRef<string | number | null>(null);

  const virtualizer = useVirtualizer({
    count: items.length,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => estimateSize,
    getItemKey: (i) => getKey(items[i]),
    overscan: 12,
    anchorTo: 'end',
    followOnAppend: true,
    scrollEndThreshold: END_THRESHOLD,
  });

  // Pinned to the bottom: true on open and whenever the user is at the end.
  // Re-applied after every size change, because rows are measured lazily and
  // estimated offsets alone undershoot the true end.
  const stickRef = useRef(true);
  // Geometry right after our last pin, to tell a user scroll apart from growth.
  const pinned = useRef<{ top: number; height: number; client: number } | null>(null);
  const totalSize = virtualizer.getTotalSize();
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el || items.length === 0) return;
    if (stickRef.current) {
      const p = pinned.current;
      // If the position moved away from where we pinned it (and is no longer
      // at the old end), the user scrolled before the scroll event reached us.
      // A change of viewport height (e.g. a card above the list disappearing)
      // moves scrollTop too; that is not the user scrolling.
      const userMoved = p !== null && p.client === el.clientHeight && Math.abs(el.scrollTop - p.top) > 2 && el.scrollTop + el.clientHeight < p.height - END_THRESHOLD;
      if (userMoved) {
        stickRef.current = false;
      } else {
        el.scrollTop = el.scrollHeight;
        pinned.current = { top: el.scrollTop, height: el.scrollHeight, client: el.clientHeight };
      }
    }
    initialScrollDone.current = true;
  }, [items.length, totalSize]);

  // Keep the pin when the viewport itself is resized.
  useEffect(() => {
    const el = scrollRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(() => {
      if (stickRef.current) {
        el.scrollTop = el.scrollHeight;
        pinned.current = { top: el.scrollTop, height: el.scrollHeight, client: el.clientHeight };
      }
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Count items that arrived at the tail while scrolled away.
  useEffect(() => {
    const newLast = items.length ? getKey(items[items.length - 1]) : null;
    if (items.length > lastCount.current && newLast !== lastKey.current && !atEnd) {
      setUnseen((u) => u + 1);
    }
    lastCount.current = items.length;
    lastKey.current = newLast;
  }, [items, getKey, atEnd]);

  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    const end = el.scrollHeight - el.scrollTop - el.clientHeight <= END_THRESHOLD;
    stickRef.current = end;
    if (end) pinned.current = { top: el.scrollTop, height: el.scrollHeight, client: el.clientHeight };
    setAtEnd(end);
    if (end) setUnseen(0);
    if (el.scrollTop < LOAD_OLDER_THRESHOLD && hasMore && !loadingOlder && initialScrollDone.current) onLoadOlder?.();
  }, [hasMore, loadingOlder, onLoadOlder]);

  // If the first page doesn't fill the viewport, keep loading older pages.
  useEffect(() => {
    const el = scrollRef.current;
    if (!el || !hasMore || loadingOlder || !initialScrollDone.current) return;
    if (el.scrollHeight <= el.clientHeight + LOAD_OLDER_THRESHOLD && el.scrollTop < LOAD_OLDER_THRESHOLD) onLoadOlder?.();
  }, [items.length, hasMore, loadingOlder, onLoadOlder]);

  const jumpToLatest = () => {
    if (!items.length) return;
    stickRef.current = true;
    const el = scrollRef.current;
    if (el) {
      el.scrollTop = el.scrollHeight;
      pinned.current = { top: el.scrollTop, height: el.scrollHeight, client: el.clientHeight };
    }
    setAtEnd(true);
    setUnseen(0);
  };

  const vitems = virtualizer.getVirtualItems();

  return (
    <div className={`vlist-wrap ${className ?? ''}`}>
      <div className="vlist" ref={scrollRef} onScroll={onScroll}>
        {/* Zero-height sticky overlay so it does not shift virtual item offsets. */}
        <div className="vlist-top">
          {loadingOlder ? (
            <span className="vlist-pill">Loading older events…</span>
          ) : hasMore ? (
            <button className="vlist-pill link-btn" onClick={onLoadOlder}>
              Load older
            </button>
          ) : null}
        </div>
        {items.length === 0 ? <div className="vlist-empty muted">{empty ?? 'Nothing yet.'}</div> : null}
        <div style={{ height: virtualizer.getTotalSize(), position: 'relative', width: '100%' }}>
          {vitems.map((vi) => (
            <div
              key={vi.key}
              data-index={vi.index}
              ref={virtualizer.measureElement}
              className="vlist-row"
              style={{ position: 'absolute', top: 0, left: 0, width: '100%', transform: `translateY(${vi.start}px)` }}
            >
              {render(items[vi.index], vi.index)}
            </div>
          ))}
        </div>
      </div>
      {!atEnd && items.length > 0 ? (
        <button className="jump-latest primary" onClick={jumpToLatest}>
          ↓ Jump to latest{unseen > 0 ? ` (${unseen} new)` : ''}
        </button>
      ) : null}
    </div>
  );
}
