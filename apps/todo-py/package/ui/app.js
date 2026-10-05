"use strict";

const $ = (id) => document.getElementById(id);

function showError(e) {
  $("error").textContent = e ? String(e.message ?? e) : "";
  $("error").hidden = !e;
}

function render(items) {
  // Re-rendering replaces the rows: keep the focus on the same todo (or its ✕).
  const el = document.activeElement;
  const keep = el?.closest("li")?.dataset.id;
  const onDel = el?.classList.contains("del");
  $("list").replaceChildren(...items.map(item));
  const left = items.filter((t) => !t.done).length;
  $("left").textContent = items.length ? `${left} of ${items.length} left` : "nothing to do";
  $("clear").hidden = left === items.length;
  if (keep !== undefined) {
    const li = $("list").querySelector(`li[data-id="${CSS.escape(keep)}"]`) ?? $("list").lastElementChild;
    (onDel ? li?.querySelector(".del") : li)?.focus() ?? $("text").focus();
  }
}

function item(t) {
  const li = document.createElement("li");
  li.dataset.id = t.id;
  li.tabIndex = 0;
  li.classList.toggle("done", t.done);

  const check = Object.assign(document.createElement("input"), { type: "checkbox", checked: t.done, tabIndex: -1 });
  check.addEventListener("change", () => call("toggle", { id: t.id }));

  const text = Object.assign(document.createElement("span"), { textContent: t.text, title: "double-click (F2) to edit" });
  text.addEventListener("dblclick", () => edit(li, t));

  const del = Object.assign(document.createElement("button"), { className: "link del", textContent: "✕", title: "delete" });
  del.addEventListener("click", () => call("remove", { id: t.id }));

  li.append(check, text, del);
  return li;
}

// Inline editing: Enter saves, Escape cancels.
function edit(li, t) {
  const span = li.querySelector("span");
  const input = Object.assign(document.createElement("input"), { className: "edit", value: t.text, maxLength: 500 });
  let done = false;
  const finish = (save) => {
    if (done) return;
    done = true;
    const next = input.value.trim();
    input.replaceWith(span);
    li.focus();
    if (save && next && next !== t.text) call("edit", { id: t.id, text: next });
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" || e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      finish(e.key === "Enter");
    }
  });
  input.addEventListener("blur", () => finish(true));
  span.replaceWith(input);
  input.focus();
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

// Device keys (arrows, Enter, Back = Escape): Up/Down walk the field, the todos and
// "clear completed"; on a todo Enter toggles it, Right goes to its ✕, F2 edits it.
function stops() {
  return [$("text"), ...$("list").children, $("clear")].filter((el) => !el.hidden);
}

document.addEventListener("keydown", (e) => {
  const el = document.activeElement;
  if (el?.classList.contains("edit")) return;
  const li = el?.closest("li");
  const all = stops();
  const i = all.indexOf(li ?? el);
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    all[Math.max(0, Math.min(all.length - 1, i + (e.key === "ArrowDown" ? 1 : -1)))]?.focus();
  } else if (li && el === li && e.key === "Enter") {
    call("toggle", { id: Number(li.dataset.id) }); // data-id is a string, the backend wants the number
  } else if (li && e.key === "ArrowRight") {
    li.querySelector(".del").focus();
  } else if (li && (e.key === "ArrowLeft" || e.key === "Escape") && el !== li) {
    li.focus();
  } else if (li && e.key === "F2") {
    li.querySelector("span").dispatchEvent(new MouseEvent("dblclick"));
  } else if (li && e.key === "Delete") {
    call("remove", { id: Number(li.dataset.id) });
  } else {
    return;
  }
  e.preventDefault();
});

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

$("text").focus();
call("list");
