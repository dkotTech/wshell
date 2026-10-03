import { useEffect } from 'preact/hooks';
import { STATES, fmtAge, fmtBytes, fmtCpu, isRunning } from '../format';
import {
  apps,
  closeModal,
  connection,
  connectionError,
  counts,
  cpu,
  filter,
  modal,
  nowSecs,
  search,
  select,
  selectedId,
  shell,
  toasts,
  visibleApps,
} from '../store';
import { Actions, BusyMark, KindTag, MemoryBar, PriorityBadge, Status } from './AppBits';
import { openInstall } from './dialogs';
import { Icon, Logo } from './Icons';

const LIVE = {
  online: ['live', '● online', 'Auto-refresh every 2 s while the tab is visible'],
  paused: ['live paused', '● paused', 'Tab hidden: polling stopped'],
  offline: ['live error', '● offline', ''],
  forbidden: ['live error', '● no access', 'Open the dashboard via the link from `shellctl dashboard`'],
} as const;

export function TopBar() {
  const s = shell.value;
  const [cls, text, hint] = LIVE[connection.value];
  return (
    <header class="topbar">
      <div class="brand">
        <Logo />
        <span>wshell</span>
        {s && <span class="muted">v{s.version}</span>}
      </div>
      {s && (
        <div class="shell-meta">
          <span class="chip" title="Resource profile">
            profile <b>{s.profile}</b>
          </span>
          <span
            class={`chip${s.cgroups ? '' : ' warn'}`}
            title={s.cgroups ? 'OS limits via cgroups v2' : 'cgroup not delegated: OS limits are not applied'}
          >
            {s.cgroups ? 'cgroups v2' : 'no cgroups'}
          </span>
          <span class="chip" title="Per-app limits">
            CPU {s.cpu_max_percent || '∞'}% · RAM {s.memory_mb} Mi
          </span>
          <span class="chip" title="shelld uptime">
            uptime <b>{fmtAge(nowSecs() - s.started_at)}</b>
          </span>
        </div>
      )}
      <div class="spacer" />
      <span class={cls} title={hint || connectionError.value}>
        {text}
      </span>
      <button type="button" class="btn primary" onClick={openInstall}>
        <Icon name="download" size={16} />
        Install package
      </button>
    </header>
  );
}

export function Summary() {
  const c = counts.value;
  const active = apps.value.filter((a) => a.metrics);
  const mem = active.reduce((sum, a) => sum + (a.metrics?.memory_bytes ?? 0), 0);
  const cpuTotal = active.reduce((sum, a) => sum + (cpu.value.get(a.id) ?? 0), 0);
  const failures = apps.value.reduce((sum, a) => sum + a.failures, 0);
  const card = (label: string, value: string | number, sub: string, cls?: string) => (
    <div class="card">
      <div class="label">
        {cls && <span class={`status ${cls}`} />}
        {label}
      </div>
      <div class="value">{value}</div>
      <div class="sub">{sub}</div>
    </div>
  );
  return (
    <section class="summary" aria-label="Summary">
      {card('Apps', apps.value.length, `${c.running + c.suspended} running`)}
      {card('Running', c.running, 'UI visible', 's-running')}
      {card('Suspended', c.suspended, 'UI hidden', 's-suspended')}
      {card('Failed', c.failed, failures ? `total failures: ${failures}` : 'no failures', 's-failed')}
      {card('CPU', fmtCpu(cpuTotal), 'total across apps')}
      {card('Memory', fmtBytes(mem), 'total across apps')}
    </section>
  );
}

export function Toolbar() {
  const c = counts.value;
  const items: [typeof filter.value, number][] = [
    ['all', apps.value.length],
    ...STATES.filter((s) => c[s] > 0 || s === filter.value).map((s): [typeof filter.value, number] => [s, c[s]]),
  ];
  return (
    <section class="toolbar">
      <input
        type="search"
        placeholder="Filter by name or id…"
        autocomplete="off"
        value={search.value}
        onInput={(e) => (search.value = e.currentTarget.value)}
      />
      <div class="chips" role="tablist">
        {items.map(([key, n]) => (
          <button type="button" class={filter.value === key ? 'active' : ''} onClick={() => (filter.value = key)}>
            {key === 'all' ? 'All' : key}
            <span class="count">{n}</span>
          </button>
        ))}
      </div>
    </section>
  );
}

export function AppTable() {
  const list = visibleApps.value;
  return (
    <div class="table-wrap">
      <table>
        <thead>
          <tr>
            <th>App</th>
            <th>Status</th>
            <th title="Priority under memory pressure">Priority</th>
            <th class="num" title="Starts since shelld started">
              Starts
            </th>
            <th class="num" title="Abnormal terminations">
              Failures
            </th>
            <th class="num">Age</th>
            <th class="num">CPU</th>
            <th>Memory</th>
            <th class="num">PID</th>
            <th class="actions-col">
              <span class="sr-only">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {list.map((a) => (
            <tr key={a.id} class={selectedId.value === a.id ? 'selected' : ''} onClick={() => select(a.id)}>
              <td>
                <div class="app-name">
                  {a.name} <KindTag app={a} />
                </div>
                <div class="app-id">
                  {a.id} · v{a.version}
                </div>
              </td>
              <td>
                <Status app={a} />
                <BusyMark app={a} />
              </td>
              <td>
                <PriorityBadge app={a} />
              </td>
              <td class="num">{a.starts}</td>
              <td class={`num${a.failures ? ' fails' : ''}`} title={a.last_exit ?? ''}>
                {a.failures}
              </td>
              <td class="num">{a.started_at ? fmtAge(nowSecs() - a.started_at) : '–'}</td>
              <td class="num">{isRunning(a) ? fmtCpu(cpu.value.get(a.id)) : '–'}</td>
              <td>
                <MemoryBar app={a} />
              </td>
              <td class="num mono">{a.pid ?? '–'}</td>
              <td>
                <div class="row-actions">
                  <Actions app={a} />
                </div>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      {!apps.value.length ? (
        <div class="empty">
          <p>No installed apps.</p>
          <button type="button" class="btn primary" onClick={openInstall}>
            Install package
          </button>
        </div>
      ) : (
        !list.length && (
          <div class="empty">
            <p>Nothing matches the filter.</p>
          </div>
        )
      )}
    </div>
  );
}

export function ModalHost() {
  const m = modal.value;
  useEffect(() => {
    if (!m) return;
    const onKey = (e: KeyboardEvent) => e.key === 'Escape' && closeModal();
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [m]);
  if (!m) return null;
  return (
    <div class="modal-wrap" onMouseDown={(e) => e.target === e.currentTarget && closeModal()}>
      <div class="modal" role="dialog" aria-modal="true">
        {m.content}
      </div>
    </div>
  );
}

export function Toasts() {
  return (
    <div class="toasts" aria-live="polite">
      {toasts.value.map((t) => (
        <div key={t.id} class={`toast${t.error ? ' error' : ''}`}>
          {t.text}
        </div>
      ))}
    </div>
  );
}
