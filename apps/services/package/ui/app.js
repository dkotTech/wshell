"use strict";

const $ = (id) => document.getElementById(id);
// Unit state changes come from systemd, not from the app: poll while the UI is visible.
const POLL_MS = 2000;
let timer = null;

function showError(e) {
  $("error").textContent = e ? String(e.message ?? e) : "";
  $("error").hidden = !e;
}

const running = (u) => u.active === "active" || u.active === "activating" || u.active === "reloading";

async function control(unit, action) {
  showError(null);
  try {
    await shell.call("control", { unit, action });
  } catch (e) {
    showError(e);
  }
  refresh();
}

function render(units) {
  const focused = document.activeElement?.closest("li")?.dataset.unit;
  const items = units.map((u) => {
    const li = document.createElement("li");
    li.dataset.unit = u.name;
    li.tabIndex = 0;
    li.className = u.active;
    const name = Object.assign(document.createElement("span"), { className: "name", textContent: u.name.replace(/\.service$/, "") });
    name.title = u.name;
    if (u.description) name.append(Object.assign(document.createElement("small"), { textContent: u.description }));
    const state = Object.assign(document.createElement("span"), { className: "state", textContent: u.sub });
    const toggle = Object.assign(document.createElement("button"), { textContent: running(u) ? "stop" : "start", tabIndex: -1 });
    toggle.onclick = () => control(u.name, running(u) ? "stop" : "start");
    const restart = Object.assign(document.createElement("button"), { textContent: "restart", tabIndex: -1 });
    restart.onclick = () => control(u.name, "restart");
    li.append(Object.assign(document.createElement("span"), { className: "dot" }), name, state, toggle, restart);
    li.unit = u;
    return li;
  });
  $("list").replaceChildren(...items);
  $("empty").hidden = units.length > 0;
  (items.find((li) => li.dataset.unit === focused) ?? items[0])?.focus();
}

async function refresh() {
  clearTimeout(timer);
  try {
    render(await shell.call("list"));
  } catch (e) {
    showError(e);
  }
  if (document.visibilityState === "visible") timer = setTimeout(refresh, POLL_MS);
}

document.addEventListener("keydown", (e) => {
  const li = document.activeElement?.closest("li");
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    const items = [...$("list").children];
    const next = items[items.indexOf(li) + (e.key === "ArrowDown" ? 1 : -1)] ?? (li ? null : items[0]);
    if (next) {
      next.focus();
      next.scrollIntoView({ block: "nearest" });
    }
    e.preventDefault();
  } else if (li && e.key === "Enter") {
    control(li.unit.name, running(li.unit) ? "stop" : "start");
  } else if (li && (e.key === "r" || e.key === "R")) {
    control(li.unit.name, "restart");
  }
});

document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible") refresh();
  else clearTimeout(timer);
});

shell.onState((ok) => {
  $("conn").textContent = ok ? "connected" : "disconnected";
  $("conn").classList.toggle("ok", ok);
});

refresh();
