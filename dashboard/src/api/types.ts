// Dashboard response types. Mirror of the `shell_core::control` serde structs (snake_case).

export type AppState = 'stopped' | 'starting' | 'running' | 'suspended' | 'stopping' | 'failed';
export type Priority = 'low' | 'normal' | 'high';
export type IdleMode = 'stop' | 'keep';

export interface PermissionInfo {
  key: string;
  description: string;
  reason: string | null;
  optional: boolean;
  /** Granted to the installed app (in a package on update, `false` means a new permission). */
  granted: boolean;
}

export interface Metrics {
  memory_bytes: number;
  memory_max_bytes: number | null;
  cpu_usage_usec: number;
  pids: number;
  source: 'cgroup' | 'proc';
}

export interface BusyInfo {
  reason: string;
  since: number;
  expired: boolean;
}

export interface LifecycleHints {
  idle: IdleMode;
  priority: Priority;
}

export interface AppStatus {
  id: string;
  name: string;
  version: string;
  state: AppState;
  pid: number | null;
  permissions: PermissionInfo[];
  has_ui: boolean;
  started_at: number | null;
  starts: number;
  failures: number;
  last_exit: string | null;
  memory_limit_mb: number;
  metrics: Metrics | null;
  hints: LifecycleHints;
  priority: Priority;
  idle: IdleMode;
  busy: BusyInfo[];
  last_activity: number | null;
  evict_rank: number | null;
  on_demand: boolean;
  /** `app`: a component with a bridge and hooks, `command`: a program with `main`,
   *  `plugin`: an app with access to Shell itself (`shell.*` permissions). */
  kind: 'app' | 'command' | 'plugin';
  /** `net.listen` ports on which the program accepts connections. */
  ports: number[];
}

export interface ShellInfo {
  version: string;
  profile: string;
  cgroups: boolean;
  started_at: number;
  ui_port: number | null;
  cpu_max_percent: number;
  memory_mb: number;
}

export interface Overview {
  shell: ShellInfo;
  apps: AppStatus[];
  now: number;
}

export interface PackageInfo {
  id: string;
  name: string;
  version: string;
  has_ui: boolean;
  installed_version: string | null;
  permissions: PermissionInfo[];
  warnings: string[];
}

export interface Uploaded {
  upload: string;
  package: PackageInfo;
}
