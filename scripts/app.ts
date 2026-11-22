// The browser's half of dither: the upload queue, selection, drag to
// reorder, and the dialogs. Everything the server can draw, it draws and
// Datastar patches in; this holds only what lives in the browser: files on
// their way, what is selected, a drag in progress.
//
// Uploads go in chunks (POST a batch, PUT each chunk with Content-Range, POST
// finish), three files at a time. A chunk that fails on the network is sent
// again from wherever the server says it got to, so a phone dropping signal
// halfway through a 40 MB panorama loses seconds, not the file. Progress is
// drawn from bytes acknowledged plus bytes in flight, eased toward at the
// display's frame rate so the bars glide rather than jump between events.

const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];
const body = document.body;

const MAX = Number(body.dataset.maxUpload) || 64_000_000;
const CHUNK = Number(body.dataset.chunk) || 4 * 1024 * 1024;
const PARALLEL = 3;
const RETRIES = 6;
const TYPES = new Set(["image/jpeg", "image/png", "image/gif", "image/webp"]);

const human = (b) => {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let u = 0;
  while (b >= 1000 && u < units.length - 1) {
    b /= 1000;
    u++;
  }
  return u === 0 ? `${Math.round(b)} B` : `${b >= 100 ? b.toFixed(0) : b.toFixed(1)} ${units[u]}`;
};

const span = (s) => {
  s = Math.max(0, Math.round(s));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, "0")}s`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** A request the server can tell came from here: cross-site forms cannot set the header. */
function api(method, url, data) {
  const headers = { "x-dither": "1", accept: "application/json" };
  if (data !== undefined) headers["content-type"] = "application/json";
  return fetch(url, {
    method,
    headers,
    credentials: "same-origin",
    body: data === undefined ? undefined : JSON.stringify(data),
  });
}

async function json(res) {
  try {
    return await res.json();
  } catch {
    return {};
  }
}

// ── Notices ──────────────────────────────────────────────────────────────

function notice(text, bad = false, link) {
  const el = $("#notice");
  if (!el) return;
  el.replaceChildren();
  el.className = bad ? "notice bad" : "notice";
  el.setAttribute("role", bad ? "alert" : "status");
  el.append(text);
  if (link) {
    const a = document.createElement("a");
    a.href = link.href;
    a.textContent = link.label;
    el.append(" ", a);
  }
  const close = document.createElement("button");
  close.className = "quiet";
  close.textContent = "dismiss";
  close.onclick = () => (el.hidden = true);
  el.append(close);
  el.hidden = false;
}

// ── Dialogs ──────────────────────────────────────────────────────────────

function show(id) {
  const d = document.getElementById(id);
  if (d && !d.open) d.showModal();
}

function ask(text, label = "delete") {
  const d = $("#confirm");
  $("#confirm-text").textContent = text;
  $("#confirm-ok").textContent = label;
  d.returnValue = "";
  d.showModal();
  return new Promise((resolve) => d.addEventListener("close", () => resolve(d.returnValue === "ok"), { once: true }));
}

document.addEventListener("click", (ev) => {
  const close = ev.target.closest("[data-close]");
  if (close) close.closest("dialog")?.close();
});

// A tap on a dialog's backdrop closes it, as Escape does.
for (const d of $$("dialog")) {
  d.addEventListener("click", (ev) => {
    if (ev.target === d) d.close();
  });
}

$("#viewer")?.addEventListener("close", () => {
  // The next image opened draws into an empty viewer, not over the last one.
  $("#viewer-body").replaceChildren();
});

// ── Copying and selecting text ───────────────────────────────────────────

document.addEventListener("click", async (ev) => {
  const btn = ev.target.closest("[data-copy]");
  if (!btn) return;
  try {
    await navigator.clipboard.writeText(btn.dataset.copy);
    btn.classList.add("done");
    const label = btn.querySelector("span");
    const was = label?.textContent;
    if (label) label.textContent = "copied";
    setTimeout(() => {
      btn.classList.remove("done");
      if (label) label.textContent = was;
    }, 1400);
  } catch {
    notice("Couldn't copy: the browser refused. Select the link and copy it by hand.", true);
  }
});

document.addEventListener("focusin", (ev) => {
  if (ev.target.matches?.("[data-select]")) ev.target.select();
});

// ── The viewer: keys and swipes ──────────────────────────────────────────

document.addEventListener("keydown", (ev) => {
  const viewer = $("#viewer");
  if (!viewer?.open || ev.target.matches("textarea, input")) return;
  const btn = viewer.querySelector(`[data-key="${ev.key}"]`);
  if (btn) {
    ev.preventDefault();
    btn.click();
  }
});

{
  let start = null;
  const viewer = $("#viewer");
  viewer?.addEventListener(
    "touchstart",
    (ev) => {
      if (ev.target.closest(".stage") && ev.touches.length === 1) {
        start = { x: ev.touches[0].clientX, y: ev.touches[0].clientY };
      }
    },
    { passive: true },
  );
  viewer?.addEventListener("touchend", (ev) => {
    if (!start) return;
    const t = ev.changedTouches[0];
    const dx = t.clientX - start.x;
    const dy = t.clientY - start.y;
    start = null;
    if (Math.abs(dx) > 50 && Math.abs(dx) > Math.abs(dy) * 1.5) {
      viewer.querySelector(dx < 0 ? ".nav.next" : ".nav.prev")?.click();
    } else if (dy > 90 && Math.abs(dy) > Math.abs(dx) * 1.5) {
      viewer.close();
    }
  });
}

// ── The album being shown, if any ────────────────────────────────────────

const albumId = () => $(".album-head")?.dataset.album ?? null;

// ── Selection ────────────────────────────────────────────────────────────

const selected = new Set();
let suppressClick = false;

// Writes only what differs: this also runs from a MutationObserver, and a
// write that changed nothing would still be a mutation that runs it again.
function paintSelection() {
  for (const t of $$(".tile")) {
    const on = selected.has(t.dataset.id);
    if (t.classList.contains("selected") !== on) t.classList.toggle("selected", on);
  }
  const n = selected.size;
  const count = $("[data-selected-count]");
  const text = n === 0 ? "none selected" : `${n} selected`;
  if (count && count.textContent !== text) count.textContent = text;
  for (const b of $$("[data-move], [data-delete]")) if (b.disabled !== (n === 0)) b.disabled = n === 0;
  const cover = $("[data-make-cover]");
  if (cover && cover.disabled !== (n !== 1)) cover.disabled = n !== 1;
}

function selecting(on) {
  body.classList.toggle("selecting", on);
  const bar = $("[data-selectbar]");
  if (bar) bar.hidden = !on;
  if (!on) selected.clear();
  paintSelection();
}

function toggle(id) {
  if (selected.has(id)) selected.delete(id);
  else selected.add(id);
  paintSelection();
}

// Clicks on tiles are the viewer's unless selecting, or a drag just ended.
document.addEventListener(
  "click",
  (ev) => {
    const tile = ev.target.closest(".tile");
    if (!tile) return;
    if (suppressClick) {
      suppressClick = false;
      ev.preventDefault();
      ev.stopImmediatePropagation();
      return;
    }
    if (body.classList.contains("selecting") || ev.metaKey || ev.ctrlKey || ev.shiftKey) {
      ev.preventDefault();
      ev.stopImmediatePropagation();
      if (!body.classList.contains("selecting")) selecting(true);
      toggle(tile.dataset.id);
    }
  },
  true,
);

// The grid is redrawn by the server as uploads land; the selection is put
// back on whatever tiles it draws. Datastar morphs the grid in place, so the
// element observed here is the one it keeps.
{
  const grid = $("#grid");
  if (grid) new MutationObserver(paintSelection).observe(grid, { childList: true, subtree: true });
}

document.addEventListener("click", async (ev) => {
  const t = ev.target;
  if (t.closest("[data-select-mode]")) selecting(!body.classList.contains("selecting"));
  else if (t.closest("[data-select-done]")) selecting(false);
  else if (t.closest("[data-move]")) openMover([...selected]);
  else if (t.closest("[data-make-cover]") && selected.size === 1) {
    const [image] = selected;
    const res = await api("POST", `/albums/${albumId()}/cover`, { image });
    if (res.ok) selecting(false);
    else notice("Couldn't set the cover.", true);
  } else if (t.closest("[data-delete]") && selected.size) {
    const n = selected.size;
    if (await ask(`Delete ${n === 1 ? "this image" : `these ${n} images`}? Links to ${n === 1 ? "it" : "them"} stop working. This can't be undone.`)) {
      const res = await api("POST", "/images/delete", { ids: [...selected] });
      if (res.ok) selecting(false);
      else notice("Couldn't delete them.", true);
    }
  } else if (t.closest("[data-move-one]")) {
    openMover([t.closest("[data-move-one]").dataset.moveOne]);
  } else if (t.closest("[data-delete-one]")) {
    const id = t.closest("[data-delete-one]").dataset.deleteOne;
    if (await ask("Delete this image? Links to it stop working. This can't be undone.")) {
      const viewer = $("#viewer");
      const next = viewer.querySelector(".nav.next") ?? viewer.querySelector(".nav.prev");
      const res = await api("POST", "/images/delete", { ids: [id] });
      if (!res.ok) notice("Couldn't delete it.", true);
      else if (next) next.click();
      else viewer.close();
    }
  } else if (t.closest("[data-delete-album]")) {
    const n = $$(".tile").length;
    const what = n === 0 ? "this album" : `this album and its ${n === 1 ? "image" : `${n} images`}`;
    if (await ask(`Delete ${what}? Its link stops working. This can't be undone.`)) {
      const res = await api("POST", `/albums/${t.closest("[data-delete-album]").dataset.deleteAlbum}/delete`);
      if (res.ok) location.href = "/";
      else notice("Couldn't delete the album.", true);
    }
  } else if (t.closest("[data-new-album]")) {
    const res = await api("POST", "/api/albums", {});
    if (res.ok) location.href = `/albums/${(await json(res)).id}`;
    else notice("Couldn't make an album.", true);
  } else if (t.closest("[data-pick]")) {
    $("#pick").click();
  }
});

// ── Moving images to another album ───────────────────────────────────────

let moving = [];

async function openMover(ids) {
  if (!ids.length) return;
  moving = ids;
  const res = await fetch(`/albums/${albumId()}/picker`, { headers: { "x-dither": "1" } });
  if (!res.ok) return notice("Couldn't list your albums.", true);
  $("#mover-body").outerHTML = await res.text();
  show("mover");
}

document.addEventListener("click", async (ev) => {
  if (!ev.target.closest("[data-move-confirm]")) return;
  const dialog = $("#mover");
  const to = dialog.querySelector("input[name=to]:checked")?.value || null;
  const title = dialog.querySelector("input[name=title]")?.value ?? "";
  const res = await api("POST", "/images/move", { ids: moving, to, title });
  if (!res.ok) return notice("Couldn't move them.", true);
  const { moved, to: dest } = await json(res);
  dialog.close();
  $("#viewer")?.close();
  selecting(false);
  notice(`Moved ${moved === 1 ? "1 image" : `${moved} images`}.`, false, { href: `/albums/${dest}`, label: "open that album" });
});

// Typing a new album's name picks that option.
document.addEventListener("input", (ev) => {
  if (ev.target.matches("#mover input[name=title]")) {
    const radio = ev.target.closest(".choice")?.querySelector("input[type=radio]");
    if (radio) radio.checked = true;
  }
});

// ── Drag to reorder ──────────────────────────────────────────────────────
//
// With a mouse, a tile moves as soon as it is dragged. On a touch screen a
// press is a scroll unless it is held: hold a tile to lift it, then drag;
// let go without moving to start selecting from it. The lifted tile follows
// the pointer while the others slide out of its way.

{
  const grid = () => $("#grid");
  let drag = null;
  let scrollFrame = 0;

  const flip = (tiles, change) => {
    const before = new Map(tiles.map((t) => [t, t.getBoundingClientRect()]));
    change();
    for (const t of tiles) {
      if (t === drag?.tile) continue;
      const a = before.get(t);
      const b = t.getBoundingClientRect();
      const dx = a.left - b.left;
      const dy = a.top - b.top;
      if (dx || dy) {
        t.animate([{ transform: `translate(${dx}px, ${dy}px)` }, { transform: "none" }], {
          duration: 180,
          easing: "cubic-bezier(.2,.8,.2,1)",
        });
      }
    }
  };

  const follow = () => {
    if (!drag?.live) return;
    const { tile } = drag;
    tile.style.transform = "none";
    const slot = tile.getBoundingClientRect();
    tile.style.transform = `translate(${drag.px - drag.ox - slot.left}px, ${drag.py - drag.oy - slot.top}px)`;
  };

  const autoscroll = () => {
    cancelAnimationFrame(scrollFrame);
    if (!drag?.live) return;
    const edge = 70;
    const v = drag.py < edge ? -(edge - drag.py) / 3 : drag.py > innerHeight - edge ? (drag.py - innerHeight + edge) / 3 : 0;
    if (v) {
      scrollBy(0, v);
      reslot();
      follow();
    }
    scrollFrame = requestAnimationFrame(autoscroll);
  };

  const reslot = () => {
    const { tile } = drag;
    tile.style.visibility = "hidden";
    const under = document.elementFromPoint(drag.px, drag.py)?.closest(".tile");
    tile.style.visibility = "";
    if (!under || under === tile || under.parentElement !== tile.parentElement) return;
    const tiles = $$(".tile", grid());
    const after = tiles.indexOf(under) > tiles.indexOf(tile);
    flip(tiles, () => (after ? under.after(tile) : under.before(tile)));
    // Moving an element releases its pointer capture; take it back so the
    // release still arrives if the pointer leaves the window.
    try {
      tile.setPointerCapture(drag.pointer);
    } catch {}
  };

  const lift = () => {
    drag.armed = true;
    drag.tile.classList.add("lifted");
    navigator.vibrate?.(12);
  };

  const start = () => {
    drag.live = true;
    try {
      drag.tile.setPointerCapture(drag.pointer);
    } catch {}
    drag.order = $$(".tile", grid()).map((t) => t.dataset.id).join();
    const r = drag.tile.getBoundingClientRect();
    drag.ox = drag.x - r.left;
    drag.oy = drag.y - r.top;
    drag.tile.classList.add("dragging");
    // Patches from the live stream wait until the drop.
    grid().setAttribute("data-ignore-morph", "");
    autoscroll();
  };

  const end = async (commit) => {
    const d = drag;
    drag = null;
    clearTimeout(d.timer);
    cancelAnimationFrame(scrollFrame);
    d.tile.classList.remove("lifted", "dragging");
    if (d.live) {
      d.tile.animate([{ transform: d.tile.style.transform || "none" }, { transform: "none" }], {
        duration: 140,
        easing: "cubic-bezier(.2,.8,.2,1)",
      });
      d.tile.style.transform = "";
      suppressClick = true;
      setTimeout(() => (suppressClick = false), 0);
      const ids = $$(".tile", grid()).map((t) => t.dataset.id);
      grid().removeAttribute("data-ignore-morph");
      if (commit && ids.join() !== d.order) {
        const res = await api("POST", `/albums/${albumId()}/order`, { ids });
        if (!res.ok) notice("Couldn't save the new order; reload to see the album as it is.", true);
      }
    } else if (d.armed && d.touch && commit) {
      // Held and let go: select from here.
      suppressClick = true;
      setTimeout(() => (suppressClick = false), 400);
      if (!body.classList.contains("selecting")) selecting(true);
      toggle(d.tile.dataset.id);
    }
  };

  document.addEventListener("pointerdown", (ev) => {
    const tile = ev.target.closest(".tile");
    if (!tile || ev.button !== 0 || !albumId() || body.classList.contains("selecting")) return;
    const touch = ev.pointerType !== "mouse";
    drag = {
      tile,
      pointer: ev.pointerId,
      x: ev.clientX,
      y: ev.clientY,
      px: ev.clientX,
      py: ev.clientY,
      touch,
      armed: !touch,
      live: false,
    };
    if (touch) drag.timer = setTimeout(lift, 380);
  });

  document.addEventListener("pointermove", (ev) => {
    if (!drag) return;
    drag.px = ev.clientX;
    drag.py = ev.clientY;
    const moved = Math.hypot(drag.px - drag.x, drag.py - drag.y);
    if (!drag.armed) {
      // A touch that moves before it is held is a scroll.
      if (moved > 8) {
        clearTimeout(drag.timer);
        drag = null;
      }
      return;
    }
    if (!drag.live && moved > 5) start();
    if (drag.live) {
      reslot();
      follow();
    }
  });

  document.addEventListener("pointerup", () => drag && end(true));
  document.addEventListener("pointercancel", () => drag && end(drag.live));
  // A held tile must not scroll the page under the finger.
  document.addEventListener(
    "touchmove",
    (ev) => {
      if (drag?.armed) ev.preventDefault();
    },
    { passive: false },
  );
  document.addEventListener("contextmenu", (ev) => {
    if (ev.target.closest(".tile")) ev.preventDefault();
  });
}

// ── Uploads ──────────────────────────────────────────────────────────────

class Failure extends Error {
  constructor(message, retry) {
    super(message);
    this.retry = retry;
  }
}

const queue = {
  items: [],
  active: 0,
  album: null,
  albumIsNew: false,
  samples: [],
};

const pending = () => queue.items.filter((i) => i.state === "queued" || i.state === "sending" || i.state === "processing");

addEventListener("beforeunload", (ev) => {
  if (pending().length) {
    ev.preventDefault();
    ev.returnValue = "";
  }
});

/** Where this batch goes: the album on show, the one a batch still going
 * went to, or a new one made for it. */
async function target(running) {
  if (albumId()) return albumId();
  if (queue.album && running) return queue.album;
  const res = await api("POST", "/api/albums", {});
  if (!res.ok) throw new Failure("couldn't make an album for these", false);
  queue.album = (await json(res)).id;
  queue.albumIsNew = true;
  return queue.album;
}

async function add(files) {
  files = [...files];
  if (!files.length) return;
  const items = files.map((file) => {
    const item = { file, name: file.name || "pasted image", size: file.size, acked: 0, flight: 0, state: "queued", error: "" };
    if (/\.(heic|heif)$/i.test(item.name) || /hei[cf]/.test(file.type)) {
      item.state = "failed";
      item.error = "HEIC isn't supported yet: export it as JPEG";
    } else if (file.type && !TYPES.has(file.type)) {
      item.state = "failed";
      item.error = "not a JPEG, PNG, GIF or WebP";
    } else if (file.size > MAX) {
      item.state = "failed";
      item.error = `${human(file.size)} is over the ${human(MAX)} limit`;
    } else if (file.size === 0) {
      item.state = "failed";
      item.error = "the file is empty";
    }
    return item;
  });
  const running = pending().length > 0;
  if (!running) {
    // A new batch: what the last one left in the tray goes.
    for (const i of queue.items) if (i.preview) URL.revokeObjectURL(i.preview);
    queue.items = [];
    queue.samples = [];
    queue.albumIsNew = false;
  }
  queue.items.push(...items);
  const go = items.filter((i) => i.state === "queued");
  tray.open();
  if (!go.length) return tray.draw();
  try {
    const album = await target(running);
    const res = await api("POST", `/api/albums/${album}/uploads`, {
      files: go.map((i) => ({ name: i.name, size: i.size })),
    });
    const answer = await json(res);
    if (!res.ok) throw new Failure(answer.error || `the server refused the batch (${res.status})`, false);
    answer.uploads.forEach((u, n) => {
      go[n].id = u.id;
      go[n].album = album;
    });
  } catch (e) {
    for (const i of go) {
      i.state = "failed";
      i.error = e.message || "couldn't start the upload";
    }
  }
  tray.draw();
  pump();
}

function pump() {
  while (queue.active < PARALLEL) {
    const next = queue.items.find((i) => i.state === "queued" && i.id);
    if (!next) break;
    send(next);
  }
  tray.draw();
}

function put(item, start, end) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    item.xhr = xhr;
    xhr.open("PUT", `/api/uploads/${item.id}`);
    xhr.setRequestHeader("x-dither", "1");
    xhr.setRequestHeader("content-range", `bytes ${start}-${end - 1}/${item.size}`);
    xhr.timeout = 120_000;
    xhr.upload.onprogress = (ev) => {
      item.flight = ev.loaded;
      tray.wake();
    };
    xhr.onload = () => {
      item.flight = 0;
      let answer = {};
      try {
        answer = JSON.parse(xhr.responseText);
      } catch {}
      if (xhr.status === 200 || xhr.status === 409) resolve(answer.received ?? start);
      else if (xhr.status === 401) reject(new Failure("signed out: sign in again and add them again", false));
      else if (xhr.status >= 500 && xhr.status !== 507) reject(new Failure(`the server failed (${xhr.status})`, true));
      else reject(new Failure(answer.error || `refused (${xhr.status})`, false));
    };
    xhr.onerror = () => {
      item.flight = 0;
      reject(new Failure("the connection dropped", true));
    };
    xhr.ontimeout = xhr.onerror;
    xhr.onabort = () => {
      item.flight = 0;
      reject(new Failure("cancelled", false));
    };
    xhr.send(item.file.slice(start, end));
  });
}

async function send(item) {
  queue.active++;
  item.state = "sending";
  let tries = 0;
  try {
    for (;;) {
      try {
        while (item.acked < item.size) {
          const end = Math.min(item.acked + CHUNK, item.size);
          item.acked = await put(item, item.acked, end);
          tries = 0;
        }
        item.state = "processing";
        tray.draw();
        const res = await api("POST", `/api/uploads/${item.id}/finish`);
        const answer = await json(res);
        if (res.ok) {
          item.state = "done";
          item.image = answer.image;
          break;
        }
        if (res.status === 409) {
          item.acked = answer.received ?? 0;
          item.state = "sending";
          continue;
        }
        throw new Failure(answer.error || `processing failed (${res.status})`, res.status >= 500 && res.status !== 507);
      } catch (e) {
        if (!(e instanceof Failure) || !e.retry || tries >= RETRIES) throw e;
        tries++;
        item.state = "sending";
        item.error = `${e.message}; retrying`;
        tray.draw();
        await sleep(1000 * 2 ** (tries - 1));
        // Resume from what the server has, whatever the last answer said.
        const res = await api("GET", `/api/uploads/${item.id}`).catch(() => null);
        if (res?.ok) item.acked = (await json(res)).received ?? item.acked;
        item.error = "";
      }
    }
  } catch (e) {
    item.state = item.state === "cancelled" ? "cancelled" : "failed";
    item.error = e.message || "the upload failed";
  } finally {
    item.flight = 0;
    item.xhr = null;
    queue.active--;
    pump();
  }
}

function retry(item) {
  if (!item.id) return;
  item.state = "queued";
  item.error = "";
  pump();
}

function cancelAll() {
  for (const i of pending()) {
    if (i.state === "processing") continue;
    i.state = "cancelled";
    i.xhr?.abort();
    if (i.id) api("DELETE", `/api/uploads/${i.id}`).catch(() => {});
  }
  tray.draw();
}

// ── The tray: what is uploading, drawn at the display's frame rate ──────

const tray = (() => {
  const root = $("#tray");
  let built = false;
  let el = {};
  let shown = 0;
  let last = 0;
  let frame = 0;
  let textAt = 0;
  let expanded = matchMedia("(min-width: 721px)").matches;

  const make = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text) e.textContent = text;
    return e;
  };

  const build = () => {
    built = true;
    el.title = make("span", "tray-title");
    el.rate = make("span", "tray-rate");
    el.toggle = make("button", "quiet", "details");
    el.cancel = make("button", "quiet", "cancel");
    el.close = make("button", "quiet", "close");
    el.meter = make("progress", "meter");
    el.meter.max = 1;
    el.meter.value = 0;
    el.foot = make("div", "tray-foot");
    el.list = make("ol", "tray-list");
    const line = make("div", "tray-line");
    line.append(el.title, make("span", "flex-1"), el.rate, el.toggle, el.cancel, el.close);
    root.append(line, el.meter, el.foot, el.list);
    el.toggle.onclick = () => {
      expanded = !expanded;
      draw();
    };
    el.cancel.onclick = cancelAll;
    el.close.onclick = () => {
      root.hidden = true;
      for (const i of queue.items) if (i.preview) URL.revokeObjectURL(i.preview);
      queue.items = queue.items.filter((i) => pending().includes(i));
    };
  };

  const row = (item) => {
    if (!item.row) {
      const li = make("li", "tray-item");
      const thumb = make("span", "tray-thumb");
      if (TYPES.has(item.file.type)) {
        const img = make("img");
        item.preview = URL.createObjectURL(item.file);
        img.src = item.preview;
        img.alt = "";
        img.loading = "lazy";
        img.decoding = "async";
        thumb.append(img);
      }
      const name = make("span", "tray-name", item.name);
      const state = make("span", "tray-state");
      const meter = make("progress", "meter");
      meter.max = 1;
      const again = make("button", "quiet", "retry");
      again.onclick = () => retry(item);
      const text = make("div", "tray-text");
      text.append(name, state);
      const main = make("div", "tray-main");
      main.append(text, meter);
      li.append(thumb, main, again);
      item.row = { li, state, meter, again };
    }
    return item.row;
  };

  const words = {
    queued: "waiting",
    sending: "",
    processing: "processing",
    done: "done",
    failed: "",
    cancelled: "cancelled",
  };

  // Text changes four times a second at most; bars move every frame.
  const drawText = (now) => {
    const items = queue.items.filter((i) => i.state !== "cancelled");
    const live = pending();
    const done = items.filter((i) => i.state === "done").length;
    const failed = items.filter((i) => i.state === "failed").length;
    const total = items.filter((i) => i.state !== "failed").reduce((s, i) => s + i.size, 0);
    const sent = items.filter((i) => i.state !== "failed").reduce((s, i) => s + Math.min(i.size, i.acked + i.flight), 0);
    const processing = live.filter((i) => i.state === "processing").length;

    // Rate over the last few seconds, from samples of bytes sent.
    queue.samples.push([now, sent]);
    while (queue.samples.length > 2 && now - queue.samples[0][0] > 4000) queue.samples.shift();
    const [t0, b0] = queue.samples[0];
    const rate = now - t0 > 400 ? ((sent - b0) * 1000) / (now - t0) : 0;

    if (live.length) {
      el.title.textContent = processing && sent >= total ? `processing ${processing}…` : `${done} of ${items.length - failed} uploaded`;
      el.rate.textContent = rate > 0 && sent < total ? `${human(rate)}/s · ${span((total - sent) / rate)} left` : "";
    } else {
      el.title.textContent = failed ? `${done} uploaded, ${failed} failed` : `${done === 1 ? "1 image" : `${done} images`} uploaded`;
      el.rate.textContent = "";
    }
    el.foot.replaceChildren(`${human(Math.min(sent, total))} of ${human(total)}`);
    if (!live.length && queue.albumIsNew && queue.album && done) {
      const a = make("a", "", "open the album");
      a.href = `/albums/${queue.album}`;
      el.foot.append(" · ", a);
    }
    el.cancel.hidden = !live.some((i) => i.state !== "processing");
    el.close.hidden = live.length > 0;
    el.toggle.textContent = expanded ? "hide" : "details";
    el.list.hidden = !expanded;

    if (expanded) {
      const rows = items.map((i) => {
        const r = row(i);
        r.li.dataset.state = i.state;
        r.state.textContent = i.error || words[i.state] || human(i.size);
        r.again.hidden = !(i.state === "failed" && i.id);
        return r.li;
      });
      if (rows.length !== el.list.children.length || rows.some((r, n) => el.list.children[n] !== r)) {
        el.list.replaceChildren(...rows);
      }
    }
    return { sent, total, live: live.length };
  };

  let target = { sent: 0, total: 0, live: 0 };

  const draw = () => {
    if (!built) build();
    textAt = 0;
    wake();
  };

  const tick = (now) => {
    frame = 0;
    if (now - textAt > 250) {
      target = drawText(now);
      textAt = now;
    }
    const goal = target.total ? Math.min(1, target.sent / target.total) : 0;
    const dt = last ? Math.min(100, now - last) : 16;
    last = now;
    // Ease toward the measurement: fast enough to keep up, slow enough that
    // a chunk's progress events read as one motion.
    shown += (goal - shown) * (1 - Math.exp(-dt / 140));
    if (Math.abs(goal - shown) < 0.0005) shown = goal;
    el.meter.value = shown;
    if (!target.live && shown >= goal) el.meter.value = goal;
    for (const i of queue.items) {
      if (!i.row || !expanded) continue;
      const m = i.row.meter;
      if (i.state === "processing" || i.state === "queued") {
        m.removeAttribute("value");
      } else {
        const g = i.size ? Math.min(1, (i.acked + i.flight) / i.size) : 0;
        const v = Number(m.getAttribute("value") ?? 0);
        const next = i.state === "done" ? 1 : v + (g - v) * (1 - Math.exp(-dt / 140));
        m.value = next;
      }
    }
    if (target.live || Math.abs(goal - shown) > 0.0005) wake();
    else last = 0;
  };

  const wake = () => {
    if (!frame && !document.hidden) frame = requestAnimationFrame(tick);
  };

  document.addEventListener("visibilitychange", wake);

  return {
    open() {
      if (!built) build();
      root.hidden = false;
      wake();
    },
    draw,
    wake,
  };
})();

// ── Ways in: the picker, dropping, pasting ───────────────────────────────

$("#pick")?.addEventListener("change", (ev) => {
  add(ev.target.files);
  ev.target.value = "";
});

{
  let depth = 0;
  const drop = $("#drop");
  const hasFiles = (ev) => [...(ev.dataTransfer?.types ?? [])].includes("Files");
  addEventListener("dragenter", (ev) => {
    if (!hasFiles(ev) || !drop) return;
    depth++;
    drop.hidden = false;
    body.classList.add("dropping");
  });
  addEventListener("dragleave", () => {
    if (!drop) return;
    depth = Math.max(0, depth - 1);
    if (!depth) {
      drop.hidden = true;
      body.classList.remove("dropping");
    }
  });
  addEventListener("dragover", (ev) => {
    if (hasFiles(ev)) ev.preventDefault();
  });
  addEventListener("drop", (ev) => {
    if (!hasFiles(ev)) return;
    ev.preventDefault();
    depth = 0;
    if (drop) drop.hidden = true;
    body.classList.remove("dropping");
    if ($("[data-dropzone]")) add(ev.dataTransfer.files);
  });
}

addEventListener("paste", (ev) => {
  const files = [...(ev.clipboardData?.files ?? [])].filter((f) => f.type.startsWith("image/"));
  if (!files.length || !$("[data-dropzone]")) return;
  ev.preventDefault();
  add(files);
});

// Datastar expressions reach these.
window.dither = { show, selecting, add };
