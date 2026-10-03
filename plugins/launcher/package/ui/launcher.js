// Launcher plugin UI: the list of apps with a UI. Arrows move the selection, Enter (or a click)
// opens the app in the same page. Coming back here is the Renderer's job (App switcher key).
"use strict";

const list = document.getElementById("apps");
const status = document.getElementById("status");
let apps = [];

function items() {
  return Array.from(list.children);
}

function show() {
  const focused = document.activeElement?.dataset?.id;
  list.replaceChildren(
    ...apps.map((a) => {
      const item = document.createElement("button");
      item.className = "app";
      item.dataset.id = a.id;
      const name = document.createElement("span");
      name.className = "name";
      name.textContent = a.name;
      const state = document.createElement("span");
      state.className = "state";
      state.textContent = a.state;
      item.append(name, state);
      item.addEventListener("click", () => open(a.id));
      return item;
    }),
  );
  document.getElementById("empty").hidden = apps.length > 0;
  (items().find((i) => i.dataset.id === focused) ?? list.firstElementChild)?.focus();
}

async function load() {
  try {
    apps = await shell.call("apps");
    status.textContent = "";
    show();
  } catch (e) {
    status.textContent = String(e.message ?? e);
  }
}

async function open(id) {
  status.textContent = "…";
  try {
    const { url } = await shell.call("open", { id });
    location.assign(url);
  } catch (e) {
    status.textContent = String(e.message ?? e);
  }
}

document.addEventListener("keydown", (e) => {
  const all = items();
  const i = all.indexOf(document.activeElement);
  if (e.key === "ArrowDown") {
    all[Math.min(i + 1, all.length - 1)]?.focus();
  } else if (e.key === "ArrowUp") {
    all[Math.max(i - 1, 0)]?.focus();
  } else if (e.key === "Enter" && i >= 0) {
    open(all[i].dataset.id);
  } else {
    return;
  }
  e.preventDefault();
});

// App states change (started, stopped): refresh while the launcher is on screen.
setInterval(() => document.visibilityState === "visible" && load(), 5000);
document.addEventListener("visibilitychange", () => document.visibilityState === "visible" && load());
load();
