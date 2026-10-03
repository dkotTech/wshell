import type { AppState, AppStatus, BusyInfo, Priority } from './api/types';

/** Age in kubectl style: 45s, 12m, 3h5m, 2d4h. */
export function fmtAge(secs: number | null | undefined): string {
  if (secs == null) return '–';
  if (secs < 120) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h${Math.floor((secs % 3600) / 60)}m`;
  return `${Math.floor(secs / 86400)}d${Math.floor((secs % 86400) / 3600)}h`;
}

export function fmtBytes(b: number | null | undefined): string {
  if (b == null) return '–';
  if (b < 1024) return `${b} B`;
  if (b < 1048576) return `${(b / 1024).toFixed(0)} Ki`;
  if (b < 1073741824) return `${(b / 1048576).toFixed(1)} Mi`;
  return `${(b / 1073741824).toFixed(2)} Gi`;
}

export function fmtCpu(pct: number | null | undefined): string {
  if (pct == null) return '–';
  if (pct < 0.1) return '0%';
  return pct < 10 ? `${pct.toFixed(1)}%` : `${pct.toFixed(0)}%`;
}

export const STATES: AppState[] = ['running', 'suspended', 'starting', 'stopping', 'failed', 'stopped'];

export const STATE_HINT: Record<AppState, string> = {
  running: 'Running, UI open and visible',
  suspended: 'Running, UI hidden or closed: events are not sent to the UI',
  starting: 'Starting',
  stopping: 'Stopping',
  failed: 'Terminated abnormally or failed to start',
  stopped: 'Installed, not running',
};

export const PRIORITY_HINT: Record<Priority, string> = {
  low: 'Low: stopped first under memory pressure',
  normal: 'Normal priority',
  high: 'High: stopped last under memory pressure',
};

export const isRunning = (a: AppStatus) => a.state === 'running' || a.state === 'suspended';
export const isTransitional = (a: AppStatus) => a.state === 'starting' || a.state === 'stopping';
export const activeBusy = (a: AppStatus): BusyInfo[] => a.busy.filter((b) => !b.expired);
