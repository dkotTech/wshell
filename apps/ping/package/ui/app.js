"use strict";

const $ = (id) => document.getElementById(id);
const out = $("out");
let currentJob = null;
let hosts = [];

function print(text, cls) {
  const span = document.createElement("span");
  if (cls) span.className = cls;
  span.textContent = text;
  out.append(span);
  out.scrollTop = out.scrollHeight;
}

function showError(e) {
  $("error").textContent = e ? String(e.message ?? e) : "";
  $("error").hidden = !e;
}

function setBusy(job) {
  currentJob = job;
  $("ping").disabled = $("trace").disabled = job !== null;
  $("cancel").disabled = job === null;
}

async function refreshHistory() {
  hosts = await shell.call("history");
  $("history").replaceChildren(...hosts.map((h) => Object.assign(document.createElement("option"), { value: h })));
}

async function run(method, params) {
  showError(null);
  out.replaceChildren();
  try {
    const { job } = await shell.call(method, params);
    setBusy(job);
    print(`$ ${method} ${params.host}\n`, "meta");
    refreshHistory().catch(() => {});
  } catch (e) {
    showError(e);
  }
}

shell.on("output", ({ job, stream, text }) => {
  if (job === currentJob) print(text, stream === "stderr" ? "stderr" : null);
});

shell.on("exit", ({ job, exitCode, timedOut }) => {
  if (job !== currentJob) return;
  const why = timedOut ? "timeout" : exitCode === null ? "stopped" : `code ${exitCode}`;
  print(`\n[finished: ${why}]\n`, "meta");
  setBusy(null);
});

shell.onState((ok) => {
  $("conn").textContent = ok ? "connected" : "disconnected";
  $("conn").classList.toggle("ok", ok);
});

$("form").addEventListener("submit", (ev) => {
  ev.preventDefault();
  run("ping", { host: $("host").value.trim(), count: Number($("count").value) });
});
$("trace").addEventListener("click", () => run("tracepath", { host: $("host").value.trim() }));
$("cancel").addEventListener("click", () => currentJob !== null && shell.call("cancel", { job: currentJob }));

// Device keys (arrows, Enter, Back = Escape): Left/Right walk the controls, Up/Down pick a host
// from the history in the host field and scroll the output elsewhere, Escape stops the job.
function controls() {
  return [$("host"), $("count"), $("ping"), $("trace"), $("cancel")].filter((c) => !c.disabled);
}

document.addEventListener("keydown", (e) => {
  const el = document.activeElement;
  const all = controls();
  const i = all.indexOf(el);
  const caretFree = el !== $("host") || (e.key === "ArrowLeft" ? el.selectionStart === 0 : el.selectionEnd === el.value.length);
  if ((e.key === "ArrowLeft" || e.key === "ArrowRight") && caretFree) {
    all[(i + (e.key === "ArrowLeft" ? -1 : 1) + all.length) % all.length].focus();
  } else if ((e.key === "ArrowUp" || e.key === "ArrowDown") && el === $("host") && hosts.length) {
    const at = hosts.indexOf(el.value);
    const next = at < 0 ? 0 : (at + (e.key === "ArrowDown" ? 1 : -1) + hosts.length) % hosts.length;
    el.value = hosts[next];
  } else if ((e.key === "ArrowUp" || e.key === "ArrowDown") && el !== $("count")) {
    out.scrollBy(0, (e.key === "ArrowDown" ? 1 : -1) * out.clientHeight * 0.8);
  } else if (e.key === "Escape" && currentJob !== null) {
    shell.call("cancel", { job: currentJob });
  } else {
    return;
  }
  e.preventDefault();
});

$("host").focus();

(async () => {
  try {
    const caps = await shell.call("capabilities");
    if (!caps.tracepath) {
      $("trace").disabled = true;
      $("trace").title = "exec:tracepath permission not granted";
    }
    if (caps.history) await refreshHistory();
  } catch (e) {
    showError(e);
  }
})();
