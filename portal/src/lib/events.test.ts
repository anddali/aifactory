import { describe, expect, it } from 'vitest';
import type { RunEvent } from '../api';
import { hasOlder, maxSeq, mergeEvents, minSeq } from './events';

const ev = (seq: number, tag = ''): RunEvent => ({ seq, ts: '2026-01-01T00:00:00Z', kind: 'cost', actor: null, payload: { tag } });
const seqs = (l: RunEvent[]) => l.map((e) => e.seq);
const range = (a: number, b: number) => Array.from({ length: b - a + 1 }, (_, i) => ev(a + i));

describe('mergeEvents', () => {
  it('returns incoming when existing is empty', () => {
    expect(seqs(mergeEvents([], range(5, 8)))).toEqual([5, 6, 7, 8]);
  });

  it('returns existing unchanged for empty incoming', () => {
    const e = range(1, 3);
    expect(mergeEvents(e, [])).toBe(e);
  });

  it('appends live SSE events after the newest', () => {
    expect(seqs(mergeEvents(range(1, 3), [ev(4)]))).toEqual([1, 2, 3, 4]);
  });

  it('prepends an older page', () => {
    expect(seqs(mergeEvents(range(201, 400), range(1, 200)))).toEqual(seqs(range(1, 400)));
  });

  it('drops duplicates when SSE replays events already in the page', () => {
    const merged = mergeEvents(range(1, 10), [ev(9, 'x'), ev(10, 'y'), ev(11)]);
    expect(seqs(merged)).toEqual(seqs(range(1, 11)));
    expect(merged[8].payload.tag).toBe('x');
  });

  it('handles overlapping pages and unsorted, duplicated input', () => {
    const merged = mergeEvents([ev(3), ev(5), ev(7)], [ev(6), ev(4), ev(6), ev(8), ev(1)]);
    expect(seqs(merged)).toEqual([1, 3, 4, 5, 6, 7, 8]);
  });

  it('fills a gap between loaded ranges', () => {
    const merged = mergeEvents([...range(1, 3), ...range(8, 10)], range(4, 7));
    expect(seqs(merged)).toEqual(seqs(range(1, 10)));
  });

  it('is idempotent', () => {
    const a = mergeEvents(range(1, 5), range(3, 9));
    expect(seqs(mergeEvents(a, a))).toEqual(seqs(a));
  });
});

describe('paging helpers', () => {
  it('reports min and max seq', () => {
    expect(minSeq([])).toBeNull();
    expect(maxSeq([])).toBeNull();
    expect(minSeq(range(10, 20))).toBe(10);
    expect(maxSeq(range(10, 20))).toBe(20);
  });

  it('knows when older pages remain', () => {
    expect(hasOlder([], null, 200)).toBe(false);
    expect(hasOlder(range(1, 50), null, 200)).toBe(false);
    expect(hasOlder(range(201, 400), null, 200)).toBe(true);
    expect(hasOlder(range(150, 400), 120, 200)).toBe(false);
  });
});
