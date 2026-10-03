"use strict";

const $ = (id) => document.getElementById(id);

function showError(e) {
  $("error").textContent = e ? String(e.message ?? e) : "";
  $("error").hidden = !e;
}

function render(items) {
  $("list").replaceChildren(...items.map(item));
  const left = items.filter((t) => !t.done).length;
  $("left").textContent = items.length ? `${left} of ${items.length} left` : "nothing to do";
  $("clear").hidden = left === items.length;
}

function item(t) {
  const li = document.createElement("li");
  li.classList.toggle("done", t.done);

  const check = Object.assign(document.createElement("input"), { type: "checkbox", checked: t.done });
  check.addEventListener("change", () => call("toggle", { id: t.id }));

  const text = Object.assign(document.createElement("span"), { textContent: t.text, title: "double-click to edit" });
  text.addEventListener("dblclick", () => {
    const next = prompt("Edit todo", t.text);
    if (next !== null && next.trim() && next !== t.text) call("edit", { id: t.id, text: next });
  });

  const del = Object.assign(document.createElement("button"), { className: "link", textContent: "✕", title: "delete" });
  del.addEventListener("click", () => call("remove", { id: t.id }));

  li.append(check, text, del);
  return li;
}

// Every method returns the whole list: render it as is.
async function call(method, params = {}) {
  try {
    render(await shell.call(method, params));
    showError(null);
  } catch (e) {
    showError(e);
  }
}

$("form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  await call("add", { text: $("text").value });
  $("text").value = "";
});
$("clear").addEventListener("click", () => call("clear_done"));

// Changes from other tabs.
shell.on("changed", () => call("list"));
shell.onState((ok) => {
  $("conn").textContent = ok ? "connected" : "disconnected";
  $("conn").classList.toggle("ok", ok);
});

call("list");
