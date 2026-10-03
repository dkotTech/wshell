"use strict";

const $ = (id) => document.getElementById(id);
const out = $("out");
let currentJob = null;

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
  const hosts = await shell.call("history");
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
