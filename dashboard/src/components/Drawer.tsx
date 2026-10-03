import { useEffect, useRef, useState } from 'preact/hooks';
import { logStreamUrl } from '../api/client';
import type { AppStatus } from '../api/types';
import { STATE_HINT, fmtAge, fmtCpu, isRunning } from '../format';
import { closeDrawer, cpu, modal, nowSecs, selected, tab } from '../store';
import { Actions, MemoryBar, Status, portUrl } from './AppBits';

export function Drawer() {
  const app = selected.value;
  useEffect(() => {
    if (!app) return;
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && !modal.value && closeDrawer();
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [!!app]);

  return (
    <>
      <aside class={`drawer${app ? ' open' : ''}`} aria-hidden={!app}>
        {app && (
          <>
            <div class="drawer-head">
              <div class="drawer-title">
                <div class="d-name">
                  {app.name} <Status app={app} />
                </div>
                <div class="mono muted">
                  {app.id} · v{app.version}
                </div>
              </div>
              <button type="button" class="icon-btn" title="Close (Esc)" onClick={closeDrawer}>
                ✕
              </button>
            </div>
            <div class="d-actions">
              <Actions app={app} full />
            </div>
            <nav class="tabs">
              <button type="button" class={`tab${tab.value === 'overview' ? ' active' : ''}`} onClick={() => (tab.value = 'overview')}>
                Overview
              </button>
              <button type="button" class={`tab${tab.value === 'logs' ? ' active' : ''}`} onClick={() => (tab.value = 'logs')}>
                Log
              </button>
            </nav>
            {tab.value === 'overview' ? <Overview app={app} /> : <Logs key={app.id} id={app.id} />}
          </>
        )}
      </aside>
      {app && <div class="backdrop" onClick={closeDrawer} />}
    </>
  );
}

function Kv({ rows }: { rows: [string, preact.ComponentChildren][] }) {
  return (
    <dl class="kv">
      {rows.map(([k, v]) => (
        <>
          <dt>{k}</dt>
          <dd>{v}</dd>
        </>
      ))}
    </dl>
  );
}

function Overview({ app }: { app: AppStatus }) {
  const m = app.metrics;
  const granted = app.permissions.filter((p) => p.granted).length;
  const cleanExit = app.last_exit != null && /exit status: 0\b/.test(app.last_exit);
  return (
    <div class="tab-panel">
      <Kv
        rows={[
          [
            'Status',
            <span>
              <Status app={app} /> <span class="muted">{STATE_HINT[app.state]}</span>
            </span>,
          ],
          ['PID', app.pid ?? '–'],
          ['Age', app.started_at ? `${fmtAge(nowSecs() - app.started_at)} (since ${new Date(app.started_at * 1000).toLocaleString()})` : '–'],
          ['Starts / failures', `${app.starts} / ${app.failures}`],
          ['CPU', isRunning(app) ? fmtCpu(cpu.value.get(app.id)) : '–'],
          ['Memory', m ? <MemoryBar app={app} /> : '–'],
          ['WASM limit', `${app.memory_limit_mb} Mi`],
          ['Processes', m ? m.pids : '–'],
          ['Metrics source', m ? (m.source === 'cgroup' ? 'cgroup v2' : '/proc (no cgroups)') : '–'],
          app.kind === 'command'
            ? [
                'Kind',
                'program with main (wasi:cli/command): its own server, no Shell bridge or hooks; not stopped when idle',
              ]
            : ['UI', app.has_ui ? 'yes' : 'no (background service)'],
          ...(app.ports.length
            ? [
                [
                  'Ports',
                  <span>
                    {app.ports.map((p, i) => (
                      <>
                        {i > 0 && ', '}
                        <a href={portUrl(p)} target="_blank" rel="noopener">
                          {p}
                        </a>
                      </>
                    ))}{' '}
                    <span class="muted">— access is not controlled by Shell</span>
                  </span>,
                ] as [string, preact.ComponentChildren],
              ]
            : []),
        ]}
      />

      <h3>Lifecycle</h3>
      <Lifecycle app={app} />

      <h3>Last exit</h3>
      {app.last_exit ? (
        <div class={`exit-box${cleanExit ? ' ok' : ''}`}>{app.last_exit}</div>
      ) : (
        <p class="muted">The app has not exited yet.</p>
      )}

      <h3>
        Permissions ({granted} of {app.permissions.length})
      </h3>
      {app.permissions.length ? (
        app.permissions.map((p) => (
          <div class={`perm ${p.granted ? 'granted' : 'denied'}`} title={p.granted ? 'granted' : 'not granted'}>
            <span class="mark">{p.granted ? '✓' : '–'}</span>
            <span class="key">
              {p.key}
              {p.optional && <span class="tag">optional</span>}
              {!p.granted && <span class="tag">not granted</span>}
            </span>
            <span class="desc">{p.description}</span>
            {p.reason && <span class="reason">why: {p.reason}</span>}
          </div>
        ))
      ) : (
        <p class="muted">The app requests no permissions.</p>
      )}
    </div>
  );
}

function Lifecycle({ app }: { app: AppStatus }) {
  const hint = <T extends string>(declared: T, applied: T, fmt: (v: T) => string) =>
    declared === applied ? fmt(applied) : `${fmt(applied)} — device policy (app requests: ${fmt(declared)})`;
  const idleText = (v: string) => (v === 'keep' ? 'do not stop' : 'may be stopped');
  const busy = app.busy.length ? (
    <div>
      {app.busy.map((b) => (
        <div class={b.expired ? 'muted' : ''}>
          🔒 {b.reason} — {fmtAge(nowSecs() - b.since)}
          {b.expired && ' (longer than max_busy_s, not counted)'}
        </div>
      ))}
    </div>
  ) : (
    'no — can be stopped without waiting'
  );
  return (
    <Kv
      rows={[
        ['Priority', hint(app.hints.priority, app.priority, (v) => v)],
        ['When idle', hint(app.hints.idle, app.idle, idleText)],
        ['Cold start', app.on_demand ? 'yes — starts when the UI is opened or on a call' : 'no — manual only'],
        ['Busy', busy],
        ['Last activity', app.last_activity ? `${fmtAge(Math.max(0, nowSecs() - app.last_activity))} ago` : '–'],
        ['Eviction queue', app.evict_rank ? `#${app.evict_rank} under memory pressure` : '–'],
      ]}
    />
  );
}

// --- Log ---

const LEVEL: Record<string, string> = { ERROR: 'l-error', WARN: 'l-warn', stderr: 'l-stderr', shell: 'l-shell' };
const MAX_LINES = 2000;

function LogLine({ line }: { line: string }) {
  // Format: 2026-09-26T19:20:11Z <source> <text>
  const m = /^(\S+Z) (\S+) (.*)$/.exec(line);
  if (!m) return <div>{line}</div>;
  const [, ts, src, msg] = m;
  return (
    <div class={LEVEL[src] ?? ''}>
      <span class="ts">{ts.replace('T', ' ').replace('Z', '')}</span> <span class="src">{src.padEnd(6)}</span>{' '}
      <span class="msg">{msg}</span>
    </div>
  );
}

function Logs({ id }: { id: string }) {
  const [lines, setLines] = useState<string[]>([]);
  const [follow, setFollow] = useState(true);
  const [query, setQuery] = useState('');
  const [status, setStatus] = useState('loading…');
  const pre = useRef<HTMLPreElement>(null);
  const stick = useRef(true);

  // A single stream (SSE): the server atomically sends the last lines and then new ones,
  // with no gap in between. On reconnect the tail arrives again.
  useEffect(() => {
    if (!follow) {
      setStatus('paused');
      return;
    }
    const es = new EventSource(logStreamUrl(id, 300));
    es.onopen = () => {
      setLines([]);
      setStatus('stream open');
    };
    es.onmessage = (ev) => setLines((prev) => [...prev, ev.data].slice(-MAX_LINES));
    es.onerror = () => setStatus('reconnecting…');
    return () => es.close();
  }, [id, follow]);

  useEffect(() => {
    const el = pre.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  }, [lines]);

  const q = query.toLowerCase();
  const shown = q ? lines.filter((l) => l.toLowerCase().includes(q)) : lines;
  return (
    <div class="tab-panel" id="tab-logs">
      <div class="log-tools">
        <label>
          <input type="checkbox" checked={follow} onChange={(e) => setFollow(e.currentTarget.checked)} /> follow
        </label>
        <input type="search" placeholder="filter lines…" value={query} onInput={(e) => setQuery(e.currentTarget.value)} />
        <span class="muted">{status}</span>
      </div>
      <pre
        class="log"
        ref={pre}
        onScroll={(e) => {
          const el = e.currentTarget;
          stick.current = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
        }}
      >
        {shown.map((l) => (
          <LogLine line={l} />
        ))}
      </pre>
    </div>
  );
}
