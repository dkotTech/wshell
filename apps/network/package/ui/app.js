"use strict";

const $ = (id) => document.getElementById(id);
const el = (tag, props) => Object.assign(document.createElement(tag), props);
// The network changes on its own (cables, DHCP, radio): poll while the UI is visible.
const POLL_MS = 2000;
// Not shown: nothing to see or do on them here.
const HIDDEN_KINDS = ["loopback", "wifi-p2p"];

let timer = null;
let current = null; // the open device's name; null for the list
let devices = [];
let armed = null; // a disconnect waiting for the second press
let passwordFor = null; // the network the password form is for

function showError(e) {
  $("error").textContent = e ? String(e.message ?? e) : "";
  $("error").hidden = !e;
}

const linked = (d) => d.state === "connected" || d.state === "connecting";

async function action(method, params) {
  showError(null);
  try {
    await shell.call(method, params);
  } catch (e) {
    showError(e);
  }
  refresh();
}

// --- the device list ---

function renderDevices() {
  const shown = devices.filter((d) => !HIDDEN_KINDS.includes(d.kind) && d.state !== "unmanaged");
  const items = shown.map((d) => {
    const li = el("li", { tabIndex: 0, className: d.state });
    li.dataset.key = d.name;
    li.onclick = () => (location.hash = encodeURIComponent(d.name));
    const name = el("span", { className: "name", textContent: d.name });
    name.append(el("small", { textContent: d.connection ? `${d.kind} · ${d.connection}` : d.kind }));
    const addr = el("span", { className: "state", textContent: d.ipv4[0]?.replace(/\/\d+$/, "") ?? d.state });
    li.append(el("span", { className: "dot" }), name, addr);
    return li;
  });
  keepFocus($("device-list"), items);
  $("no-devices").hidden = shown.length > 0;
}

// --- one device ---

function openDevice(name) {
  current = name;
  armed = null;
  closePassword();
  $("devices").hidden = true;
  $("device").hidden = false;
  $("ap-list").replaceChildren();
  const d = devices.find((d) => d.name === name);
  if (d?.kind === "wifi") shell.call("scan", { device: name }).catch(() => {}); // NetworkManager limits how often
  refresh().then(() => $("toggle").focus());
}

// The open device is in the URL (#wlp0s20f3): the browser's back button works too.
function route() {
  const name = decodeURIComponent(location.hash.slice(1));
  if (name) openDevice(name);
  else back();
}
window.addEventListener("hashchange", route);

function back() {
  if (location.hash) history.replaceState(null, "", location.pathname + location.search);
  current = null;
  $("devices").hidden = false;
  $("device").hidden = true;
  showError(null);
  refresh();
}

function renderDevice(d, aps) {
  const props = [
    ["state", d.state],
    ["profile", d.connection],
    ["type", d.kind],
    ["MAC", d.hw_address, "extra"],
    ["IPv4", d.ipv4.join(", ")],
    ["IPv6", d.ipv6.join(", "), "extra"],
    ["gateway", d.gateway],
    ["DNS", d.dns.join(", ")],
  ].filter(([, v]) => v);
  $("props").replaceChildren(
    ...props.flatMap(([k, v, className = ""]) => [el("dt", { textContent: k, className }), el("dd", { textContent: v, className })]),
  );

  const toggle = $("toggle");
  toggle.textContent = !linked(d) ? "connect" : armed === d.name ? "press again to disconnect" : "disconnect";
  toggle.classList.toggle("armed", armed === d.name);
  toggle.onclick = () => {
    if (!linked(d)) return action("connect", { device: d.name });
    if (armed !== d.name) {
      // Disconnecting the link the UI is opened over cuts it off: ask twice.
      armed = d.name;
      setTimeout(() => (armed = null), 3000);
      return renderDevice(d, aps);
    }
    armed = null;
    action("disconnect", { device: d.name });
  };

  $("wifi").hidden = d.kind !== "wifi";
  if (d.kind !== "wifi") return;
  const items = aps.map((ap) => {
    const li = el("li", { tabIndex: 0, className: [ap.active && "connected", ap.secured && "secured"].filter(Boolean).join(" ") });
    li.dataset.key = ap.ssid;
    li.onclick = () => choose(d.name, ap);
    const name = el("span", { className: "name", textContent: ap.ssid });
    const flags = [ap.secured ? "secured" : "open", ap.saved && "saved"].filter(Boolean).join(" · ");
    name.append(el("small", { textContent: flags }));
    const strength = el("span", { className: "state", textContent: `${ap.strength}%` });
    li.append(el("span", { className: "dot" }), name, strength);
    return li;
  });
  keepFocus($("ap-list"), items);
  $("no-aps").hidden = aps.length > 0;
}

function choose(device, ap) {
  if (ap.active) return;
  if (ap.secured && !ap.saved) {
    passwordFor = { device, ssid: ap.ssid };
    $("password-ssid").textContent = ap.ssid;
    $("password").value = "";
    $("password-form").hidden = false;
    $("password").focus();
    return;
  }
  action("connect-wifi", { device, ssid: ap.ssid });
}

function closePassword() {
  passwordFor = null;
  $("password-form").hidden = true;
  $("password").value = "";
}

$("password-form").onsubmit = (e) => {
  e.preventDefault();
  const { device, ssid } = passwordFor;
  const password = $("password").value;
  closePassword();
  action("connect-wifi", { device, ssid, password });
};
$("password-cancel").onclick = closePassword;

// --- common ---

/** Replaces the list and keeps the focus on the same entry (by its key) if it was in it. */
function keepFocus(list, items) {
  const active = document.activeElement;
  const inList = list.contains(active);
  const key = inList ? active.closest("li")?.dataset.key : null;
  list.replaceChildren(...items);
  if (inList) (items.find((li) => li.dataset.key === key) ?? items[0])?.focus();
  else if (!current && active === document.body) items[0]?.focus();
}

async function refresh() {
  clearTimeout(timer);
  try {
    devices = await shell.call("devices");
    const d = current && devices.find((d) => d.name === current);
    if (current && !d) back();
    else if (d) renderDevice(d, d.kind === "wifi" ? await shell.call("access-points", { device: d.name }) : []);
    else renderDevices();
    $("title").textContent = current ? `← ${current}` : "Network";
    $("hint").textContent = current
      ? `Esc back · Enter ${d?.kind === "wifi" ? "connect · S scan" : "toggle"}`
      : "↑↓ device · Enter open";
  } catch (e) {
    showError(e);
  }
  if (document.visibilityState === "visible") timer = setTimeout(refresh, POLL_MS);
}

$("title").onclick = () => current && back();

document.addEventListener("keydown", (e) => {
  if (e.target instanceof HTMLInputElement) {
    if (e.key === "Escape") closePassword();
    return;
  }
  const focusable = current ? [$("toggle"), ...$("ap-list").children] : [...$("device-list").children];
  const at = focusable.indexOf(document.activeElement);
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    const next = focusable[at < 0 ? 0 : at + (e.key === "ArrowDown" ? 1 : -1)];
    next?.focus();
    next?.scrollIntoView({ block: "nearest" });
    e.preventDefault();
  } else if (e.key === "Enter" && document.activeElement?.tagName === "LI") {
    document.activeElement.click();
  } else if (current && (e.key === "Escape" || e.key === "Backspace")) {
    back();
  } else if (current && (e.key === "s" || e.key === "S")) {
    action("scan", { device: current });
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

route();
