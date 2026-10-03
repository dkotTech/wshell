import { useRef, useState } from 'preact/hooks';
import { cancelUpload, installUpload, openUrl, uninstall, uploadPackage } from '../api/client';
import type { AppStatus, PackageInfo } from '../api/types';
import { activeBusy, fmtBytes } from '../format';
import { act, closeDrawer, closeModal, modal, refresh, select, toast } from '../store';

function Confirm({ title, text, ok, onOk }: { title: string; text: string; ok: string; onOk: () => void }) {
  return (
    <>
      <h2>{title}</h2>
      <div class="body">{text}</div>
      <div class="foot">
        <button type="button" class="btn" onClick={closeModal}>
          Cancel
        </button>
        <button
          type="button"
          class="btn danger solid"
          autoFocus
          onClick={() => {
            closeModal();
            onOk();
          }}
        >
          {ok}
        </button>
      </div>
    </>
  );
}

export function confirmStop(app: AppStatus): void {
  const reasons = activeBusy(app).map((b) => b.reason);
  if (!reasons.length) {
    modal.value = {
      content: (
        <Confirm
          title={`Stop ${app.name}?`}
          text="The app will receive on-stop and be stopped. Open tabs of its UI will lose the connection to the backend."
          ok="Stop"
          onOk={() => act(app, 'stop')}
        />
      ),
    };
    return;
  }
  // The app asks not to be stopped: gracefully means with stop-requested and waiting.
  const run = (force: boolean) => {
    closeModal();
    act(app, 'stop', force);
  };
  modal.value = {
    content: (
      <>
        <h2>{app.name} is busy right now</h2>
        <div class="body">
          <p>The app reported that it is unsafe to stop it:</p>
          <ul>
            {reasons.map((r) => (
              <li>{r}</li>
            ))}
          </ul>
          <p class="muted">Gracefully: the app gets stop-requested and time to finish its work. Now: on-stop without waiting.</p>
        </div>
        <div class="foot">
          <button type="button" class="btn" onClick={closeModal}>
            Cancel
          </button>
          <button type="button" class="btn danger solid" onClick={() => run(true)}>
            Stop now
          </button>
          <button type="button" class="btn primary" autoFocus onClick={() => run(false)}>
            Stop gracefully
          </button>
        </div>
      </>
    ),
  };
}

export function confirmUninstall(app: AppStatus): void {
  modal.value = {
    content: (
      <Confirm
        title={`Remove ${app.name}?`}
        text={`The package, granted permissions, log and app data (${app.id}) will be removed. This cannot be undone.`}
        ok="Remove"
        onOk={async () => {
          try {
            await uninstall(app.id);
            toast(`${app.name}: removed`);
            closeDrawer();
          } catch (e) {
            toast(`${app.name}: ${(e as Error).message}`, true);
          }
          await refresh();
        }}
      />
    ),
  };
}

export async function openUi(app: AppStatus): Promise<void> {
  // Open the window synchronously on click, otherwise the browser will block it.
  const win = window.open('about:blank', '_blank');
  try {
    const { url } = await openUrl(app.id);
    if (win) {
      win.opener = null;
      win.location.href = url;
    } else {
      window.location.href = url;
    }
  } catch (e) {
    win?.close();
    toast(`${app.name}: ${(e as Error).message}`, true);
  }
}

// --- Package installation ---

export function openInstall(): void {
  modal.value = { content: <Upload /> };
}

function Upload() {
  const input = useRef<HTMLInputElement>(null);
  const [over, setOver] = useState(false);
  const [busy, setBusy] = useState<File | null>(null);

  const upload = async (file: File) => {
    setBusy(file);
    try {
      const { upload, package: pkg } = await uploadPackage(file);
      showPackage(upload, pkg);
    } catch (e) {
      closeModal();
      toast(`Package rejected: ${(e as Error).message}`, true);
    }
  };

  if (busy) {
    return (
      <>
        <h2>Checking package…</h2>
        <p class="muted">
          {busy.name}, {fmtBytes(busy.size)}
        </p>
      </>
    );
  }
  return (
    <>
      <h2>Install package</h2>
      <div class="body">
        <div
          class={`dropzone${over ? ' over' : ''}`}
          tabIndex={0}
          onClick={() => input.current?.click()}
          onKeyDown={(e) => (e.key === 'Enter' || e.key === ' ') && input.current?.click()}
          onDragOver={(e) => {
            e.preventDefault();
            setOver(true);
          }}
          onDragLeave={() => setOver(false)}
          onDrop={(e) => {
            e.preventDefault();
            setOver(false);
            const f = e.dataTransfer?.files[0];
            if (f) upload(f);
          }}
        >
          <p>Drop a .pkg here or click to choose a file</p>
          <p class="muted">The package is checked before installation: manifest, component imports, permissions.</p>
        </div>
        <input
          ref={input}
          type="file"
          accept=".pkg,.tar,application/x-tar"
          hidden
          onChange={(e) => {
            const f = e.currentTarget.files?.[0];
            if (f) upload(f);
          }}
        />
      </div>
      <div class="foot">
        <button type="button" class="btn" onClick={closeModal}>
          Cancel
        </button>
      </div>
    </>
  );
}

function showPackage(upload: string, pkg: PackageInfo): void {
  let installed = false;
  modal.value = {
    content: <PackageDialog upload={upload} pkg={pkg} onInstalled={() => (installed = true)} />,
    // Closed without installing: the uploaded package is no longer needed.
    onClose: () => {
      if (!installed) cancelUpload(upload).catch(() => {});
    },
  };
}

function PackageDialog({ upload, pkg, onInstalled }: { upload: string; pkg: PackageInfo; onInstalled: () => void }) {
  const upgrade = pkg.installed_version != null;
  // On update, previously granted optional permissions stay checked.
  const [grant, setGrant] = useState(() => new Set(pkg.permissions.filter((p) => p.optional && p.granted).map((p) => p.key)));
  const [busy, setBusy] = useState(false);

  const install = async () => {
    setBusy(true);
    try {
      const r = await installUpload(upload, [...grant]);
      onInstalled();
      closeModal();
      toast(`Installed: ${r.id} ${r.version}`);
      await refresh();
      select(r.id);
    } catch (e) {
      setBusy(false);
      toast((e as Error).message, true);
    }
  };

  const toggle = (key: string, on: boolean) => {
    const next = new Set(grant);
    if (on) next.add(key);
    else next.delete(key);
    setGrant(next);
  };

  return (
    <>
      <h2>{upgrade ? `Update ${pkg.name}: ${pkg.installed_version} → ${pkg.version}` : `Install ${pkg.name} ${pkg.version}`}</h2>
      <div class="mono muted">{pkg.id}</div>
      <div class="body">
        {pkg.warnings.map((w) => (
          <div class="warning">⚠ {w}</div>
        ))}
        <h3>{pkg.permissions.length ? 'Permissions' : 'No permissions required'}</h3>
        {pkg.permissions.map((p) => (
          <div class="perm">
            <label>
              <span class="mark">
                <input
                  type="checkbox"
                  checked={!p.optional || grant.has(p.key)}
                  disabled={!p.optional}
                  onChange={(e) => toggle(p.key, e.currentTarget.checked)}
                />
              </span>
              <span class="key">
                {p.key}
                {p.optional && <span class="tag">optional</span>}
                {upgrade && !p.granted && <span class="tag new">new</span>}
              </span>
            </label>
            <span class="desc">{p.description}</span>
            {p.reason && <span class="reason">why: {p.reason}</span>}
          </div>
        ))}
        {!pkg.has_ui && <p class="muted">The app has no UI (background service).</p>}
      </div>
      <div class="foot">
        <button type="button" class="btn" onClick={closeModal}>
          Cancel
        </button>
        <button type="button" class="btn primary" disabled={busy} onClick={install}>
          {upgrade ? 'Update' : 'Install'}
        </button>
      </div>
    </>
  );
}
