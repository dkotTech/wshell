// Small parts of an app row: status, busy state, priority, memory, action buttons.
import type { AppStatus } from '../api/types';
import { PRIORITY_HINT, STATE_HINT, activeBusy, fmtAge, fmtBytes, isRunning, isTransitional } from '../format';
import { act, nowSecs, pending, select } from '../store';
import { confirmStop, confirmUninstall, openUi } from './dialogs';
import { Icon, type IconName } from './Icons';

export function Status({ app }: { app: AppStatus }) {
  return (
    <span class={`status s-${app.state}`} title={STATE_HINT[app.state]}>
      {app.state}
    </span>
  );
}

export function BusyMark({ app }: { app: AppStatus }) {
  const list = activeBusy(app);
  if (!list.length) return null;
  const title =
    'Busy: the app asks not to be stopped:\n' + list.map((b) => `• ${b.reason} (${fmtAge(nowSecs() - b.since)})`).join('\n');
  return (
    <span class="busy" title={title}>
      🔒
    </span>
  );
}

export function PriorityBadge({ app }: { app: AppStatus }) {
  const overridden = app.priority !== app.hints.priority;
  const title = PRIORITY_HINT[app.priority] + (overridden ? ` (app requests ${app.hints.priority}, device policy: ${app.priority})` : '');
  return (
    <span class={`prio p-${app.priority}`} title={title}>
      {app.priority}
      {overridden && '*'}
    </span>
  );
}

export function MemoryBar({ app }: { app: AppStatus }) {
  const m = app.metrics;
  if (!m) return <span class="muted">–</span>;
  const limit = m.memory_max_bytes ?? app.memory_limit_mb * 1048576;
  const pct = Math.min(100, (m.memory_bytes / limit) * 100);
  const level = pct > 90 ? ' crit' : pct > 70 ? ' warn' : '';
  const source = m.source === 'cgroup' ? 'cgroup memory.max' : 'WASM limit, RSS from /proc';
  return (
    <div class="mem" title={`${fmtBytes(m.memory_bytes)} of ${fmtBytes(limit)} (${source})`}>
      <div class={`bar${level}`}>
        <span style={{ width: `${pct.toFixed(1)}%` }} />
      </div>
      <span class="txt">
        {fmtBytes(m.memory_bytes)} / {fmtBytes(limit)}
      </span>
    </div>
  );
}

interface ActionDef {
  icon: IconName;
  title: string;
  run: () => void;
  enabled: boolean;
  class?: string;
}

function actions(app: AppStatus, full: boolean): ActionDef[] {
  const running = isRunning(app);
  const moving = isTransitional(app);
  if (app.kind === 'command') return commandActions(app, full);
  const openTitle = !app.has_ui
    ? 'No UI'
    : running
      ? 'Open UI'
      : app.on_demand
        ? 'Open UI (starts the app)'
        : 'Open UI: start the app first';
  const list: ActionDef[] = [
    running
      ? { icon: 'stop', title: 'Stop', run: () => confirmStop(app), enabled: !moving }
      : { icon: 'start', title: 'Start', run: () => act(app, 'start'), enabled: !moving },
    { icon: 'restart', title: 'Restart', run: () => act(app, 'restart'), enabled: running && !moving },
    { icon: 'open', title: openTitle, run: () => openUi(app), enabled: app.has_ui && (running || app.on_demand) },
  ];
  list.push(
    full
      ? { icon: 'trash', title: 'Remove', run: () => confirmUninstall(app), enabled: !running && !moving, class: 'danger' }
      : { icon: 'logs', title: 'Log', run: () => select(app.id, 'logs'), enabled: true },
  );
  return list;
}

/** Program address: the same host the dashboard was opened from, and the program's port. */
export const portUrl = (port: number) => `${location.protocol}//${location.hostname}:${port}/`;

/** A program (kind = "command"): it has no Shell UI; "Open" leads to its port. */
function commandActions(app: AppStatus, full: boolean): ActionDef[] {
  const running = isRunning(app);
  const moving = isTransitional(app);
  const port = app.ports[0];
  return [
    running
      ? { icon: 'stop', title: 'Stop', run: () => confirmStop(app), enabled: !moving }
      : { icon: 'start', title: 'Start', run: () => act(app, 'start'), enabled: !moving },
    { icon: 'restart', title: 'Restart', run: () => act(app, 'restart'), enabled: running && !moving },
    {
      icon: 'open',
      title: port ? `Open port ${port}${running ? '' : ': start the program first'}` : 'No ports',
      run: () => window.open(portUrl(port), '_blank', 'noopener'),
      enabled: running && port != null,
    },
    full
      ? { icon: 'trash', title: 'Remove', run: () => confirmUninstall(app), enabled: !running && !moving, class: 'danger' }
      : { icon: 'logs', title: 'Log', run: () => select(app.id, 'logs'), enabled: true },
  ];
}

/** App kind label: for programs and plugins; ordinary apps have no label. */
export function KindTag({ app }: { app: AppStatus }) {
  if (app.kind === 'plugin') {
    return (
      <span class="tag" title="Plugin: an app with access to Shell itself (shell.* permissions)">
        plugin
      </span>
    );
  }
  if (app.kind !== 'command') return null;
  return (
    <span class="tag" title="Program with main (wasi:cli/command): its own server, no Shell bridge or hooks">
      program
    </span>
  );
}

/** Action buttons: compact icons in the table or labelled buttons in the card. */
export function Actions({ app, full }: { app: AppStatus; full?: boolean }) {
  const busy = pending.value.has(app.id);
  return (
    <>
      {actions(app, !!full).map((a) =>
        full ? (
          <button type="button" class={`btn ${a.class ?? ''}`} title={a.title} disabled={busy || !a.enabled} onClick={a.run}>
            <Icon name={a.icon} />
            {a.title}
          </button>
        ) : (
          <button
            type="button"
            class={`icon-btn${busy ? ' busy' : ''}`}
            title={a.title}
            disabled={busy || !a.enabled}
            onClick={(ev) => {
              ev.stopPropagation();
              a.run();
            }}
          >
            <Icon name={a.icon} />
          </button>
        ),
      )}
    </>
  );
}
