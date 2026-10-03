// shell.js: the bridge between an app's UI and its WASM backend.
//
//   await shell.call("method", params)  → result of the backend's bridge.handle
//   const off = shell.on("event", (payload) => { ... })
//   shell.onState((connected) => { ... })
(() => {
  "use strict";

  const handlers = new Map();
  const stateHandlers = new Set();
  let ws = null;
  let retry = 0;
  let timer = null;

  async function call(method, params) {
    const resp = await fetch("/_shell/call", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ method, params: params ?? null }),
      credentials: "same-origin",
    });
    let body;
    try {
      body = await resp.json();
    } catch {
      body = { error: resp.statusText };
    }
    if (!resp.ok || "error" in body) {
      const err = new Error(body.error || `HTTP ${resp.status}`);
      err.status = resp.status;
      throw err;
    }
    // The call may have woken a stopped app: connect to events right away.
    if (!ws) {
      clearTimeout(timer);
      timer = null;
      retry = 0;
      connect();
    }
    return body.result;
  }

  function on(event, handler) {
    if (!handlers.has(event)) handlers.set(event, new Set());
    handlers.get(event).add(handler);
    connect();
    return () => handlers.get(event)?.delete(handler);
  }

  function onState(handler) {
    stateHandlers.add(handler);
    handler(ws?.readyState === WebSocket.OPEN);
    return () => stateHandlers.delete(handler);
  }

  function setState(connected) {
    for (const h of stateHandlers) h(connected);
  }

  function sendVisibility() {
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: "visibility", visible: document.visibilityState === "visible" }));
    }
  }

  // wake: the user opened the page or returned to the tab, so Shell may
  // start a stopped app. Background reconnects do not wake it.
  function connect(wake = false) {
    if (ws) return;
    if (timer) {
      if (!wake) return;
      clearTimeout(timer);
      timer = null;
    }
    ws = new WebSocket(`ws://${location.host}/_shell/events${wake ? "?wake=1" : ""}`);
    ws.onopen = () => {
      retry = 0;
      sendVisibility();
      setState(true);
    };
    ws.onmessage = (msg) => {
      let data;
      try {
        data = JSON.parse(msg.data);
      } catch {
        return;
      }
      for (const h of handlers.get(data.event) ?? []) {
        try {
          h(data.payload);
        } catch (e) {
          console.error(e);
        }
      }
    };
    ws.onclose = () => {
      ws = null;
      setState(false);
      // Reconnect with backoff, only while the page is visible: a hidden UI does not wake Shell.
      if (document.visibilityState !== "visible") return;
      const delay = Math.min(30000, 500 * 2 ** retry++);
      timer = setTimeout(() => {
        timer = null;
        connect();
      }, delay);
    };
  }

  document.addEventListener("visibilitychange", () => {
    if (ws) sendVisibility();
    else if (document.visibilityState === "visible") connect(true);
  });

  // The connection is needed even without subscriptions: Shell learns about UI visibility through it.
  connect(true);
  window.shell = Object.freeze({ call, on, onState });
})();
