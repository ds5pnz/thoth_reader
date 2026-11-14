// Reader session lifecycle: open with a session Channel, apply ordered events
// (ignoring stale generation / old sequence), resync from DB snapshots (spec §8.3).
import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { Channel } from '@tauri-apps/api/core';
import { call, errorMessage } from '../../ipc/client';
import type { StreamEnvelope } from '../../ipc/generated/StreamEnvelope';
import type { ReaderSessionDto } from '../../ipc/generated/ReaderSessionDto';
import type { SectionRow } from '../../ipc/generated/SectionRow';
import type { Block } from '../../ipc/generated/Block';
import type { BlocksPage } from '../../ipc/generated/BlocksPage';
import type { BlockDisplay } from '../../ipc/generated/BlockDisplay';

export type JobView = { jobId: string; state: string; errorCode?: string | null; attempt?: number; sentUnknown?: boolean };

const MAX_PREVIEW = 256 * 1024;

// A reopen of the same document must observe the final position written by the previous
// session's document_close (which is fire-and-forget during unmount).
const pendingCloses = new Map<string, Promise<unknown>>();
export function trackClose(documentId: string, p: Promise<unknown>) {
  const done = p.catch(() => {}).finally(() => { if (pendingCloses.get(documentId) === done) pendingCloses.delete(documentId); });
  pendingCloses.set(documentId, done);
}

export interface ReaderModel {
  session: ReaderSessionDto | null;
  error: string | null;
  sections: SectionRow[];
  blocks: Map<string, Block[]>; // sectionId -> blocks
  display: Map<string, BlockDisplay>;
  jobs: Map<string, JobView>;
  previews: Map<string, string>;
  indexProgress: { ready: number; failed: number; total: number } | null;
}

export function useReaderSession(documentId: string) {
  const [session, setSession] = useState<ReaderSessionDto | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [sections, setSections] = useState<SectionRow[]>([]);
  const [blocks, setBlocks] = useState<Map<string, Block[]>>(new Map());
  const [display, setDisplay] = useState<Map<string, BlockDisplay>>(new Map());
  const [jobs, setJobs] = useState<Map<string, JobView>>(new Map());
  const [previews, setPreviews] = useState<Map<string, string>>(new Map());
  const [indexProgress, setIndexProgress] = useState<ReaderModel['indexProgress']>(null);
  const sessionRef = useRef<ReaderSessionDto | null>(null);
  const lastSeq = useRef(0);
  const loadingSections = useRef(new Set<string>());
  const wanted = useRef(new Set<string>());
  const sectionState = useRef(new Map<string, string>());
  const loadSectionRef = useRef<(id: string, force?: boolean) => Promise<void>>(async () => {});
  const blocksRef = useRef(blocks);
  blocksRef.current = blocks;

  const refreshSections = useCallback(async () => {
    const s = sessionRef.current;
    if (!s) return [];
    try {
      const rows = await call<SectionRow[]>('sections_list', { sessionId: s.sessionId });
      sectionState.current = new Map(rows.map((r) => [r.id, r.state]));
      setSections(rows);
      // Sections that were asked for before they were published (progressive extraction)
      // are loaded as soon as they become ready.
      for (const id of wanted.current) {
        if (sectionState.current.get(id) === 'ready' && !blocksRef.current.has(id)) void loadSectionRef.current(id);
      }
      return rows;
    } catch {
      return [];
    }
  }, []);

  const loadSection = useCallback(async (sectionId: string, force = false) => {
    const s = sessionRef.current;
    wanted.current.add(sectionId);
    if (!s || loadingSections.current.has(sectionId)) return;
    if (!force && blocksRef.current.has(sectionId)) return;
    // Never cache an empty result for a section that is not published yet.
    const st = sectionState.current.get(sectionId);
    if (st !== undefined && st !== 'ready') return;
    loadingSections.current.add(sectionId);
    try {
      let cursor: number | null = null;
      const all: Block[] = [];
      const disp: BlockDisplay[] = [];
      for (let i = 0; i < 50; i++) {
        const page: BlocksPage = await call<BlocksPage>('blocks_get', { sessionId: s.sessionId, sectionId, cursor, limit: 1000 });
        all.push(...page.blocks);
        disp.push(...page.display);
        if (page.nextCursor == null) break;
        cursor = page.nextCursor;
      }
      setBlocks((prev) => new Map(prev).set(sectionId, all));
      setDisplay((prev) => {
        const m = new Map(prev);
        for (const d of disp) m.set(d.blockId, d);
        return m;
      });
    } catch {
      // The session may have been closed while loading (document switched): nothing to show.
    } finally {
      loadingSections.current.delete(sectionId);
    }
  }, []);

  loadSectionRef.current = loadSection;

  const onEvent = useCallback((env: StreamEnvelope) => {
    const s = sessionRef.current;
    if (!s || env.sessionId !== s.sessionId) return;
    if (env.generation !== s.generation) return; // never apply an older generation's events
    if (env.sequence <= lastSeq.current) return;
    lastSeq.current = env.sequence;
    const p = env.payload as Record<string, unknown>;
    switch (env.type) {
      case 'job_state': {
        if (!env.blockId) return;
        const st = p.state as string;
        setJobs((prev) => {
          const m = new Map(prev);
          if (['COMPLETED', 'CANCELLED'].includes(st)) m.delete(env.blockId!);
          else m.set(env.blockId!, { jobId: env.jobId!, state: st, errorCode: p.errorCode as string | null, attempt: p.attempt as number, sentUnknown: p.sentUnknown as boolean });
          return m;
        });
        if (st === 'CANCELLED' || st === 'FAILED') setPreviews((prev) => { const m = new Map(prev); m.delete(env.blockId!); return m; });
        break;
      }
      case 'translation_delta': {
        if (!env.blockId) return;
        const text = String(p.text ?? '');
        setPreviews((prev) => {
          const m = new Map(prev);
          const cur = (p.reset ? '' : m.get(env.blockId!) ?? '') + text;
          m.set(env.blockId!, cur.length > MAX_PREVIEW ? cur.slice(-MAX_PREVIEW) : cur);
          return m;
        });
        break;
      }
      case 'translation_committed': {
        if (!env.blockId) return;
        const d = p.display as BlockDisplay | undefined;
        if (d) setDisplay((prev) => new Map(prev).set(d.blockId, d));
        setPreviews((prev) => { const m = new Map(prev); m.delete(env.blockId!); return m; });
        setJobs((prev) => { const m = new Map(prev); m.delete(env.blockId!); return m; });
        break;
      }
      case 'index_progress': {
        setIndexProgress({ ready: Number(p.ready), failed: Number(p.failed), total: Number(p.total) });
        if (p.extractionId && sessionRef.current) {
          const next = { ...sessionRef.current, extractionId: String(p.extractionId) };
          sessionRef.current = next;
          setSession(next);
        }
        void refreshSections();
        break;
      }
      case 'session_closed':
        break;
    }
  }, [refreshSections]);

  useEffect(() => {
    let disposed = false;
    const channel = new Channel<StreamEnvelope>();
    channel.onmessage = (m) => { if (!disposed) onEvent(m); };
    (async () => {
      try {
        await pendingCloses.get(documentId);
        const s = await call<ReaderSessionDto>('document_open', { documentId }, { channel });
        if (disposed) {
          void call('document_close', { sessionId: s.sessionId, generation: s.generation, finalAnchor: null, view: null });
          return;
        }
        sessionRef.current = s;
        lastSeq.current = s.sequence;
        setSession(s);
        if (s.extractionId) await refreshSections();
      } catch (e) {
        setError(errorMessage(e));
      }
    })();
    return () => {
      disposed = true;
      channel.onmessage = () => {};
    };
  }, [documentId, onEvent, refreshSections]);

  /** Generation bump (provider/model/language change) is reported by the backend. */
  const setGeneration = useCallback((generation: number) => {
    const s = sessionRef.current;
    if (!s) return;
    const next = { ...s, generation };
    sessionRef.current = next;
    setSession(next);
    setJobs(new Map());
    setPreviews(new Map());
  }, []);

  const setBlockDisplay = useCallback((d: BlockDisplay) => setDisplay((prev) => new Map(prev).set(d.blockId, d)), []);

  /** Re-read display state for loaded blocks (after reconnect or settings change). */
  const resync = useCallback(async () => {
    const ids = [...blocksRef.current.keys()];
    for (const id of ids) await loadSection(id, true);
  }, [loadSection]);

  const updateSession = useCallback((next: ReaderSessionDto) => {
    sessionRef.current = next;
    setSession(next);
  }, []);

  const model: ReaderModel = useMemo(() => ({ session, error, sections, blocks, display, jobs, previews, indexProgress }), [session, error, sections, blocks, display, jobs, previews, indexProgress]);
  return { model, sessionRef, loadSection, refreshSections, setGeneration, setBlockDisplay, resync, setSession: updateSession };
}
