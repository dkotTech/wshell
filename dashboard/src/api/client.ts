import type { Overview, Uploaded } from './types';

export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
  ) {
    super(message);
  }
}

/** Dashboard API request. Server errors come as `{"error": "..."}`. */
async function api<T>(method: string, path: string, body?: Blob | object): Promise<T> {
  const init: RequestInit = { method, credentials: 'same-origin', headers: {} };
  if (body instanceof Blob) {
    init.body = body;
    init.headers = { 'content-type': 'application/octet-stream' };
  } else if (body !== undefined) {
    init.body = JSON.stringify(body);
    init.headers = { 'content-type': 'application/json' };
  }
  const resp = await fetch(`/api${path}`, init);
  let data: unknown = null;
  try {
    data = await resp.json();
  } catch {
    /* empty response */
  }
  if (!resp.ok) {
    const message = (data as { error?: string } | null)?.error ?? `HTTP ${resp.status}`;
    throw new ApiError(resp.status, message);
  }
  return data as T;
}

const app = (id: string) => `/apps/${encodeURIComponent(id)}`;

export type Action = 'start' | 'stop' | 'restart';

export const getOverview = () => api<Overview>('GET', '/overview');
export const appAction = (id: string, action: Action, force = false) =>
  api<{ ok: true }>('POST', `${app(id)}/${action}${force ? '?force=1' : ''}`);
export const openUrl = (id: string) => api<{ url: string }>('POST', `${app(id)}/open`);
export const uninstall = (id: string) => api<{ ok: true }>('DELETE', app(id));
/** Log stream (SSE): the last `tail` lines, then new ones. */
export const logStreamUrl = (id: string, tail: number) => `/api${app(id)}/logs/stream?tail=${tail}`;
export const uploadPackage = (file: File) => api<Uploaded>('POST', '/packages', file);
export const installUpload = (upload: string, grantOptional: string[]) =>
  api<{ id: string; version: string }>('POST', `/packages/${upload}/install`, { grant_optional: grantOptional });
export const cancelUpload = (upload: string) => api<{ ok: true }>('DELETE', `/packages/${upload}`);
