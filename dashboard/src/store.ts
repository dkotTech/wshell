import { computed, signal } from '@preact/signals';
import type { ComponentChildren } from 'preact';
import { ApiError, appAction, getOverview, type Action } from './api/client';
import type { AppState, AppStatus, ShellInfo } from './api/types';

const POLL_MS = 2000;

// --- Server data ---

export const shell = signal<ShellInfo | null>(null);
export const apps = signal<AppStatus[]>([]);
/** Server clock offset relative to the browser, seconds. */
const clockSkew = signal(0);
export const connection = signal<'online' | 'paused' | 'offline' | 'forbidden'>('online');
export const connectionError = signal('');

/** Current server time, Unix seconds. */
export const nowSecs = () => Math.floor(Date.now() / 1000) + clockSkew.value;

/** App CPU%, from the `cpu_usage_usec` difference between two samples. */
export const cpu = signal<Map<string, number | null>>(new Map());
const cpuSamples = new Map<string, { usec: number; at: number; pid: number }>();

function updateCpu(list: AppStatus[]): void {
  const at = performance.now();
  const next = new Map<string, number | null>();
  for (const a of list) {
    const m = a.metrics;
    if (!m || a.pid == null) {
      cpuSamples.delete(a.id);
      continue;
    }
    const prev = cpuSamples.get(a.id);
    let pct: number | null = cpu.value.get(a.id) ?? null;
    if (prev && prev.pid === a.pid && m.cpu_usage_usec >= prev.usec && at > prev.at) {
      pct = ((m.cpu_usage_usec - prev.usec) / ((at - prev.at) * 1000)) * 100;
    }
    cpuSamples.set(a.id, { usec: m.cpu_usage_usec, at, pid: a.pid });
    next.set(a.id, pct);
  }
  cpu.value = next;
}

export async function refresh(): Promise<void> {
  try {
    const data = await getOverview();
    clockSkew.value = data.now - Math.floor(Date.now() / 1000);
    updateCpu(data.apps);
    shell.value = data.shell;
    apps.value = data.apps;
    connection.value = 'online';
  } catch (e) {
    connection.value = e instanceof ApiError && e.status === 403 ? 'forbidden' : 'offline';
    connectionError.value = e instanceof Error ? e.message : String(e);
  }
}

/** Poll every 2 s while the tab is visible: a hidden dashboard does not wake Shell. */
let pollTimer: number | undefined;
function schedule(): void {
  clearTimeout(pollTimer);
  if (document.visibilityState !== 'visible') {
    connection.value = 'paused';
    return;
  }
  pollTimer = window.setTimeout(async () => {
    await refresh();
    schedule();
  }, POLL_MS);
}

export function startPolling(): void {
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') refresh().then(schedule);
    else schedule();
  });
  refresh().then(schedule);
}

// --- Filters and selection ---

export const filter = signal<'all' | AppState>('all');
export const search = signal('');
export const selectedId = signal<string | null>(null);
export const tab = signal<'overview' | 'logs'>('overview');

export const selected = computed(() => apps.value.find((a) => a.id === selectedId.value) ?? null);

export const counts = computed(() => {
  const c: Record<AppState, number> = { running: 0, suspended: 0, starting: 0, stopping: 0, failed: 0, stopped: 0 };
  for (const a of apps.value) c[a.state]++;
  return c;
});

export const visibleApps = computed(() => {
  const q = search.value.trim().toLowerCase();
  return apps.value.filter(
    (a) => (filter.value === 'all' || a.state === filter.value) && (!q || a.id.toLowerCase().includes(q) || a.name.toLowerCase().includes(q)),
  );
});

export function select(id: string, t?: 'overview' | 'logs'): void {
  if (selectedId.value !== id) tab.value = 'overview';
  if (t) tab.value = t;
  selectedId.value = id;
}

export function closeDrawer(): void {
  selectedId.value = null;
}

// --- Notifications ---

export interface Toast {
  id: number;
  text: string;
  error: boolean;
}
export const toasts = signal<Toast[]>([]);
let toastId = 0;

export function toast(text: string, error = false): void {
  const t = { id: ++toastId, text, error };
  toasts.value = [...toasts.value, t];
  setTimeout(() => (toasts.value = toasts.value.filter((x) => x.id !== t.id)), error ? 7000 : 3500);
}

// --- Modal dialog (one per page) ---

export interface ModalState {
  content: ComponentChildren;
  onClose?: () => void;
}
export const modal = signal<ModalState | null>(null);

export function closeModal(): void {
  const m = modal.value;
  modal.value = null;
  m?.onClose?.();
}

// --- App actions ---

/** Ids of apps with an action in progress: their buttons are disabled. */
export const pending = signal<Set<string>>(new Set());

const DONE: Record<Action, string> = { start: 'started', stop: 'stopped', restart: 'restarted' };

export async function act(a: AppStatus, action: Action, force = false): Promise<void> {
  pending.value = new Set(pending.value).add(a.id);
  try {
    await appAction(a.id, action, force);
    toast(`${a.name}: ${DONE[action]}`);
  } catch (e) {
    toast(`${a.name}: ${(e as Error).message}`, true);
  } finally {
    const next = new Set(pending.value);
    next.delete(a.id);
    pending.value = next;
    await refresh();
  }
}
