# Todo list in Python: a shell:app component built by componentize-py.
# State lives in the app's private storage (the storage.private permission),
# so the list survives restarts, idle stops and updates.
import json
import time

from componentize_py_types import Err
from wit_world import exports
from wit_world.imports import events, log, storage

KEY = "todos"

store = None


def load():
    raw = store.get(KEY)
    return json.loads(raw) if raw else {"next_id": 1, "items": []}


def save(state):
    store.set(KEY, json.dumps(state).encode())
    # Other open tabs refresh on this event.
    events.emit("changed", "{}")


def find(state, todo_id):
    for item in state["items"]:
        if item["id"] == todo_id:
            return item
    raise ValueError(f"no todo with id {todo_id}")


def add(state, p):
    text = str(p.get("text", "")).strip()
    if not text:
        raise ValueError("empty todo")
    state["items"].append({"id": state["next_id"], "text": text[:500], "done": False, "created": int(time.time())})
    state["next_id"] += 1


def toggle(state, p):
    item = find(state, p["id"])
    item["done"] = not item["done"]


def edit(state, p):
    text = str(p.get("text", "")).strip()
    if not text:
        raise ValueError("empty todo")
    find(state, p["id"])["text"] = text[:500]


def remove(state, p):
    state["items"] = [i for i in state["items"] if i["id"] != p["id"]]


def clear_done(state, p):
    state["items"] = [i for i in state["items"] if not i["done"]]


# Methods that change the state; `list` only reads it.
MUTATIONS = {"add": add, "toggle": toggle, "edit": edit, "remove": remove, "clear_done": clear_done}


class Lifecycle(exports.Lifecycle):
    def on_start(self):
        global store
        store = storage.open()
        if store is None:
            raise Err("storage.private permission not granted")
        log.log(log.Level.INFO, f"todo-py: {len(load()['items'])} todos loaded")

    def on_stop(self):
        pass

    def on_event(self, event):
        pass


class Bridge(exports.Bridge):
    def handle(self, method, payload):
        try:
            p = json.loads(payload) if payload else {}
            state = load()
            if method in MUTATIONS:
                MUTATIONS[method](state, p)
                save(state)
            elif method != "list":
                raise ValueError(f"unknown method: {method}")
            return json.dumps(state["items"], ensure_ascii=False)
        except (ValueError, KeyError, TypeError) as e:
            raise Err(str(e))
