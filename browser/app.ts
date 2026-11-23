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
//
// No words live here: the layout renders this script's copy through the
// catalogue into <template id="copy">, keyed by name, so every sentence on
// screen is one a translator can reach.

const $ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = document) =>
  root.querySelector<T>(sel);
const $$ = <T extends Element = HTMLElement>(sel: string, root: ParentNode = document) => [
  ...root.querySelectorAll<T>(sel),
];
const body = document.body;
const lang = document.documentElement.lang || "en-GB";

const MAX = Number(body.dataset["maxUpload"]) || 64_000_000;
/** The limit for a video, when the server takes them. */
const MAX_VIDEO = Number(body.dataset["maxVideo"]) || 0;
const CHUNK = Number(body.dataset["chunk"]) || 4 * 1024 * 1024;
const PARALLEL = 3;
const RETRIES = 6;
const TYPES = new Set(["image/jpeg", "image/png", "image/gif", "image/webp"]);
/** The video types the server takes (`api::VIDEO_TYPES`). */
const VIDEO_TYPES = new Set([
  "video/mp4",
  "video/quicktime",
  "video/x-m4v",
  "video/3gpp",
  "video/3gpp2",
  "video/webm",
  "video/x-matroska",
]);
/** Whether a file is a video the server takes, by what the browser says or, when it says nothing, by its name: as the server judges at the start, and its bytes decide at the end. */
const isVideo = (file: File) =>
  VIDEO_TYPES.has(file.type) || (!file.type && /\.(mp4|mov|m4v|3gp|webm|mkv)$/i.test(file.name));

// ── Copy ─────────────────────────────────────────────────────────────────

const plural = new Intl.PluralRules(lang);

// A line with a count comes once for each plural form the language has,
// rendered at a count in that form (`data-n`), so it is filed under the
// category its count falls in here.
const copy = new Map<string, string>();
const counted = new Map<string, Map<Intl.LDMLPluralRule, string>>();
for (const el of $<HTMLTemplateElement>("#copy")?.content.querySelectorAll<HTMLElement>("[data-k]") ?? []) {
  const key = el.dataset["k"] ?? "";
  const n = el.dataset["n"];
  if (n === undefined) {
    copy.set(key, el.textContent ?? "");
  } else {
    const forms = counted.get(key) ?? new Map<Intl.LDMLPluralRule, string>();
    forms.set(plural.select(Number(n)), el.textContent ?? "");
    counted.set(key, forms);
  }
}

const fill = (text: string, values: Record<string, string | number>) =>
  text.replace(/%\{(\w+)\}/g, (whole, name: string) => (Object.hasOwn(values, name) ? String(values[name]) : whole));

/** A line of copy with its `%{name}` placeholders filled. A missing key reads as itself, which is obvious on screen. */
function say(key: string, values: Record<string, string | number> = {}) {
  return fill(copy.get(key) ?? key, values);
}

/** A counted line in the form `n` takes, with `%{n}` filled. */
function sayN(key: string, n: number, values: Record<string, string | number> = {}) {
  const forms = counted.get(key);
  return fill(forms?.get(plural.select(n)) ?? forms?.get("other") ?? key, { ...values, n });
}

/** Bytes as the server words them (`web::human`), so a size reads the same wherever it is shown. */
function human(bytes: number) {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let u = 0;
  while (bytes >= 1000 && u < units.length - 1) {
    bytes /= 1000;
    u++;
  }
  return `${u === 0 || bytes >= 100 ? bytes.toFixed(0) : bytes.toFixed(1)} ${units[u]}`;
}

function span(seconds: number) {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, "0")}s`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, "0")}m`;
}

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/** A request the server can tell came from here: a cross-site form cannot set the header. */
function api(method: string, url: string, data?: unknown) {
  const headers: Record<string, string> = { "x-dither": "1", accept: "application/json" };
  const init: RequestInit = { method, headers, credentials: "same-origin" };
  if (data !== undefined) {
    headers["content-type"] = "application/json";
    init.body = JSON.stringify(data);
  }
  return fetch(url, init);
}

/** A JSON answer's fields, or none when it carried no JSON. */
async function json<T>(res: Response) {
  return (await res.json().catch(() => ({}))) as Partial<T>;
}

// ── Notices ──────────────────────────────────────────────────────────────

function notice(text: string, bad = false, link?: { href: string; label: string }) {
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
  close.textContent = say("dismiss");
  close.onclick = () => (el.hidden = true);
  el.append(close);
  el.hidden = false;
}

// ── Dialogs ──────────────────────────────────────────────────────────────

function show(id: string) {
  const d = document.getElementById(id);
  if (d instanceof HTMLDialogElement && !d.open) d.showModal();
}

function ask(text: string, label: string) {
  const d = $<HTMLDialogElement>("#confirm");
  const textEl = $("#confirm-text");
  const ok = $("#confirm-ok");
  if (!d || !textEl || !ok) return Promise.resolve(false);
  textEl.textContent = text;
  ok.textContent = label;
  d.returnValue = "";
  d.showModal();
  return new Promise<boolean>((resolve) =>
    d.addEventListener("close", () => resolve(d.returnValue === "ok"), { once: true }),
  );
}

document.addEventListener("click", (ev) => {
  const close = (ev.target as Element).closest("[data-close]");
  if (close) close.closest("dialog")?.close();
});

// A tap on a dialog's backdrop closes it, as Escape does.
for (const d of $$<HTMLDialogElement>("dialog")) {
  d.addEventListener("click", (ev) => {
    if (ev.target === d) d.close();
  });
}

$<HTMLDialogElement>("#viewer")?.addEventListener("close", () => {
  // The next image opened draws into an empty viewer, not over the last one.
  $("#viewer-body")?.replaceChildren();
});

// ── Copying and selecting text ───────────────────────────────────────────

document.addEventListener("click", async (ev) => {
  const btn = (ev.target as Element).closest<HTMLElement>("[data-copy]");
  if (!btn) return;
  try {
    await navigator.clipboard.writeText(btn.dataset["copy"] ?? "");
    btn.classList.add("done");
    const label = btn.querySelector("span");
    const was = label?.textContent ?? "";
    if (label) label.textContent = say("copied");
    setTimeout(() => {
      btn.classList.remove("done");
      if (label) label.textContent = was;
    }, 1400);
  } catch {
    notice(say("copy refused"), true);
  }
});

document.addEventListener("focusin", (ev) => {
  const t = ev.target;
  if (t instanceof HTMLInputElement && t.matches("[data-select]")) t.select();
});

// ── The viewer: keys and swipes ──────────────────────────────────────────

document.addEventListener("keydown", (ev) => {
  const viewer = $<HTMLDialogElement>("#viewer");
  if (!viewer?.open || (ev.target as Element).matches("textarea, input")) return;
  const btn = viewer.querySelector<HTMLElement>(`[data-key="${ev.key}"]`);
  if (btn) {
    ev.preventDefault();
    btn.click();
  }
});

{
  let start: { x: number; y: number } | null = null;
  const viewer = $<HTMLDialogElement>("#viewer");
  viewer?.addEventListener(
    "touchstart",
    (ev) => {
      const touch = ev.touches[0];
      // A swipe on the player's controls is a scrub, not a move to the next image.
      const target = ev.target as Element;
      if (target.closest(".stage") && !target.closest("[data-player-controls]") && ev.touches.length === 1 && touch) {
        start = { x: touch.clientX, y: touch.clientY };
      }
    },
    { passive: true },
  );
  viewer?.addEventListener("touchend", (ev) => {
    const t = ev.changedTouches[0];
    if (!start || !t) return;
    const dx = t.clientX - start.x;
    const dy = t.clientY - start.y;
    start = null;
    if (Math.abs(dx) > 50 && Math.abs(dx) > Math.abs(dy) * 1.5) {
      viewer.querySelector<HTMLElement>(dx < 0 ? ".nav.next" : ".nav.prev")?.click();
    } else if (dy > 90 && Math.abs(dy) > Math.abs(dx) * 1.5) {
      viewer.close();
    }
  });
}

// ── The album being shown, if any ────────────────────────────────────────

const albumId = () => $(".album-head")?.dataset["album"] ?? null;

// ── Selection ────────────────────────────────────────────────────────────

const selected = new Set<string>();
let suppressClick = false;

// Writes only what differs: this also runs from a MutationObserver, and a
// write that changed nothing would still be a mutation that runs it again.
function paintSelection() {
  for (const t of $$(".tile")) {
    const on = selected.has(t.dataset["id"] ?? "");
    if (t.classList.contains("selected") !== on) t.classList.toggle("selected", on);
  }
  const n = selected.size;
  const count = $("[data-selected-count]");
  const text = n === 0 ? say("none selected") : sayN("n selected", n);
  if (count && count.textContent !== text) count.textContent = text;
  for (const b of $$<HTMLButtonElement>("[data-move], [data-delete]")) if (b.disabled !== (n === 0)) b.disabled = n === 0;
  const cover = $<HTMLButtonElement>("[data-make-cover]");
  if (cover && cover.disabled !== (n !== 1)) cover.disabled = n !== 1;
}

function selecting(on: boolean) {
  body.classList.toggle("selecting", on);
  const bar = $("[data-selectbar]");
  if (bar) bar.hidden = !on;
  if (!on) selected.clear();
  paintSelection();
}

function toggle(id: string) {
  if (selected.has(id)) selected.delete(id);
  else selected.add(id);
  paintSelection();
}

// Clicks on tiles are the viewer's unless selecting, or a drag just ended.
document.addEventListener(
  "click",
  (ev) => {
    const tile = (ev.target as Element).closest<HTMLElement>(".tile");
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
      toggle(tile.dataset["id"] ?? "");
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
  // A video the viewer shows while it is made is drawn again once it is
  // made, or failed: its tile has lost its waiting state.
  if (grid)
    new MutationObserver(() => {
      const waiting = $<HTMLElement>("#viewer[open] [data-progress-of]")?.dataset["progressOf"];
      if (!waiting) return;
      const tile = $<HTMLElement>(`#i-${waiting}`);
      // Refused once made, it left the album: so does the viewer.
      if (!tile) $<HTMLDialogElement>("#viewer")?.close();
      else if (!tile.querySelector("progress") && !body.classList.contains("selecting")) tile.click();
    }).observe(grid, { childList: true, subtree: true });
  // A video's bar in the viewer follows its tile's, which the live stream moves.
  if (grid)
    new MutationObserver((records) => {
      for (const r of records) {
        const bar = r.target;
        if (!(bar instanceof HTMLProgressElement) || !bar.id.startsWith("vp-")) continue;
        const shown = $<HTMLProgressElement>(`#viewer [data-progress-of="${bar.id.slice(3)}"]`);
        if (shown) shown.value = bar.value;
      }
    }).observe(grid, { attributes: true, attributeFilter: ["value"], subtree: true });
}

document.addEventListener("click", async (ev) => {
  const t = ev.target as Element;
  const album = albumId();
  if (t.closest("[data-select-mode]")) selecting(!body.classList.contains("selecting"));
  else if (t.closest("[data-select-done]")) selecting(false);
  else if (t.closest("[data-move]")) void openMover([...selected]);
  else if (t.closest("[data-make-cover]") && selected.size === 1 && album) {
    const [image] = selected;
    const res = await api("POST", `/albums/${album}/cover`, { image });
    if (res.ok) selecting(false);
    else notice(say("cover failed"), true);
  } else if (t.closest("[data-delete]") && selected.size) {
    const n = selected.size;
    if (await ask(sayN("delete n?", n), say("delete"))) {
      const res = await api("POST", "/images/delete", { ids: [...selected] });
      if (res.ok) selecting(false);
      else notice(sayN("delete failed n", n), true);
    }
  } else if (t.closest("[data-move-one]")) {
    void openMover([t.closest<HTMLElement>("[data-move-one]")?.dataset["moveOne"] ?? ""]);
  } else if (t.closest("[data-delete-one]")) {
    const id = t.closest<HTMLElement>("[data-delete-one]")?.dataset["deleteOne"] ?? "";
    if (await ask(sayN("delete n?", 1), say("delete"))) {
      const viewer = $<HTMLDialogElement>("#viewer");
      const next = viewer?.querySelector<HTMLElement>(".nav.next") ?? viewer?.querySelector<HTMLElement>(".nav.prev");
      const res = await api("POST", "/images/delete", { ids: [id] });
      if (!res.ok) notice(sayN("delete failed n", 1), true);
      else if (next) next.click();
      else viewer?.close();
    }
  } else if (t.closest("[data-delete-album]")) {
    const id = t.closest<HTMLElement>("[data-delete-album]")?.dataset["deleteAlbum"] ?? "";
    // How much goes with it is the cue an irreversible delete needs.
    const n = $$(".tile").length;
    if (await ask(n === 0 ? say("delete empty album?") : sayN("delete album n?", n), say("delete"))) {
      const res = await api("POST", `/albums/${id}/delete`);
      if (res.ok) location.href = "/";
      else notice(say("album delete failed"), true);
    }
  } else if (t.closest("[data-new-album]")) {
    const res = await api("POST", "/api/albums", {});
    const answer = await json<{ id: string }>(res);
    if (res.ok && answer.id) location.href = `/albums/${answer.id}`;
    else notice(say("album failed"), true);
  } else if (t.closest("[data-pick]")) {
    $<HTMLInputElement>("#pick")?.click();
  }
});

// ── Moving images to another album ───────────────────────────────────────

let moving: string[] = [];

async function openMover(ids: string[]) {
  const album = albumId();
  if (!ids.length || !album) return;
  moving = ids;
  const res = await fetch(`/albums/${album}/picker`, { headers: { "x-dither": "1" } });
  const target = $("#mover-body");
  if (!res.ok || !target) return notice(say("albums failed"), true);
  target.outerHTML = await res.text();
  show("mover");
}

document.addEventListener("click", async (ev) => {
  if (!(ev.target as Element).closest("[data-move-confirm]")) return;
  const dialog = $<HTMLDialogElement>("#mover");
  if (!dialog) return;
  const to = dialog.querySelector<HTMLInputElement>("input[name=to]:checked")?.value || null;
  const title = dialog.querySelector<HTMLInputElement>("input[name=title]")?.value ?? "";
  const res = await api("POST", "/images/move", { ids: moving, to, title });
  if (!res.ok) return notice(say("move failed"), true);
  const { moved = 0, to: dest } = await json<{ moved: number; to: string }>(res);
  dialog.close();
  $<HTMLDialogElement>("#viewer")?.close();
  selecting(false);
  // The count says if some did not move: gone meanwhile, or not theirs.
  notice(sayN("moved n", moved), false, dest ? { href: `/albums/${dest}`, label: say("open that album") } : undefined);
});

// Typing a new album's name picks that option.
document.addEventListener("input", (ev) => {
  const t = ev.target as Element;
  if (t.matches("#mover input[name=title]")) {
    const radio = t.closest(".choice")?.querySelector<HTMLInputElement>("input[type=radio]");
    if (radio) radio.checked = true;
  }
});

// ── Drag to reorder ──────────────────────────────────────────────────────
//
// With a mouse, a tile moves as soon as it is dragged. On a touch screen a
// press is a scroll unless it is held: hold a tile to lift it, then drag;
// let go without moving to start selecting from it. The lifted tile follows
// the pointer while the others slide out of its way.

interface Drag {
  tile: HTMLElement;
  pointer: number;
  x: number;
  y: number;
  px: number;
  py: number;
  ox: number;
  oy: number;
  touch: boolean;
  armed: boolean;
  live: boolean;
  order: string;
  timer?: ReturnType<typeof setTimeout>;
}

{
  const grid = () => $("#grid");
  let drag: Drag | null = null;
  let scrollFrame = 0;

  const flip = (tiles: HTMLElement[], change: () => void) => {
    const before = new Map(tiles.map((t) => [t, t.getBoundingClientRect()]));
    change();
    for (const t of tiles) {
      if (t === drag?.tile) continue;
      const a = before.get(t);
      if (!a) continue;
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

  const follow = (d: Drag) => {
    d.tile.style.transform = "none";
    const slot = d.tile.getBoundingClientRect();
    d.tile.style.transform = `translate(${d.px - d.ox - slot.left}px, ${d.py - d.oy - slot.top}px)`;
  };

  const reslot = (d: Drag) => {
    const { tile } = d;
    tile.style.visibility = "hidden";
    const under = document.elementFromPoint(d.px, d.py)?.closest<HTMLElement>(".tile");
    tile.style.visibility = "";
    const g = grid();
    if (!under || under === tile || under.parentElement !== tile.parentElement || !g) return;
    const tiles = $$(".tile", g);
    const after = tiles.indexOf(under) > tiles.indexOf(tile);
    flip(tiles, () => (after ? under.after(tile) : under.before(tile)));
    // Moving an element releases its pointer capture; take it back so the
    // release still arrives if the pointer leaves the window.
    try {
      tile.setPointerCapture(d.pointer);
    } catch {}
  };

  const autoscroll = () => {
    cancelAnimationFrame(scrollFrame);
    const d = drag;
    if (!d?.live) return;
    const edge = 70;
    const v = d.py < edge ? -(edge - d.py) / 3 : d.py > innerHeight - edge ? (d.py - innerHeight + edge) / 3 : 0;
    if (v) {
      scrollBy(0, v);
      reslot(d);
      follow(d);
    }
    scrollFrame = requestAnimationFrame(autoscroll);
  };

  const lift = () => {
    if (!drag) return;
    drag.armed = true;
    drag.tile.classList.add("lifted");
    navigator.vibrate?.(12);
  };

  const start = (d: Drag) => {
    d.live = true;
    try {
      d.tile.setPointerCapture(d.pointer);
    } catch {}
    const g = grid();
    d.order = g ? $$(".tile", g).map((t) => t.dataset["id"]).join() : "";
    const r = d.tile.getBoundingClientRect();
    d.ox = d.x - r.left;
    d.oy = d.y - r.top;
    d.tile.classList.add("dragging");
    // Patches from the live stream wait until the drop.
    g?.setAttribute("data-ignore-morph", "");
    autoscroll();
  };

  const end = async (commit: boolean) => {
    const d = drag;
    if (!d) return;
    drag = null;
    clearTimeout(d.timer);
    cancelAnimationFrame(scrollFrame);
    d.tile.classList.remove("lifted", "dragging");
    const g = grid();
    if (d.live) {
      d.tile.animate([{ transform: d.tile.style.transform || "none" }, { transform: "none" }], {
        duration: 140,
        easing: "cubic-bezier(.2,.8,.2,1)",
      });
      d.tile.style.transform = "";
      suppressClick = true;
      setTimeout(() => (suppressClick = false), 0);
      const ids = g ? $$(".tile", g).map((t) => t.dataset["id"] ?? "") : [];
      g?.removeAttribute("data-ignore-morph");
      const album = albumId();
      if (commit && album && ids.join() !== d.order) {
        const res = await api("POST", `/albums/${album}/order`, { ids });
        if (!res.ok) notice(say("order failed"), true);
      }
    } else if (d.armed && d.touch && commit) {
      // Held and let go: select from here.
      suppressClick = true;
      setTimeout(() => (suppressClick = false), 400);
      if (!body.classList.contains("selecting")) selecting(true);
      toggle(d.tile.dataset["id"] ?? "");
    }
  };

  document.addEventListener("pointerdown", (ev) => {
    const tile = (ev.target as Element).closest<HTMLElement>(".tile");
    if (!tile || ev.button !== 0 || !albumId() || body.classList.contains("selecting")) return;
    const touch = ev.pointerType !== "mouse";
    drag = {
      tile,
      pointer: ev.pointerId,
      x: ev.clientX,
      y: ev.clientY,
      px: ev.clientX,
      py: ev.clientY,
      ox: 0,
      oy: 0,
      touch,
      armed: !touch,
      live: false,
      order: "",
    };
    if (touch) drag.timer = setTimeout(lift, 380);
  });

  document.addEventListener("pointermove", (ev) => {
    const d = drag;
    if (!d) return;
    d.px = ev.clientX;
    d.py = ev.clientY;
    const moved = Math.hypot(d.px - d.x, d.py - d.y);
    if (!d.armed) {
      // A touch that moves before it is held is a scroll.
      if (moved > 8) {
        clearTimeout(d.timer);
        drag = null;
      }
      return;
    }
    if (!d.live && moved > 5) start(d);
    if (d.live) {
      reslot(d);
      follow(d);
    }
  });

  document.addEventListener("pointerup", () => void end(true));
  document.addEventListener("pointercancel", () => void end(drag?.live ?? false));
  // A held tile must not scroll the page under the finger.
  document.addEventListener(
    "touchmove",
    (ev) => {
      if (drag?.armed) ev.preventDefault();
    },
    { passive: false },
  );
  document.addEventListener("contextmenu", (ev) => {
    if ((ev.target as Element).closest(".tile")) ev.preventDefault();
  });
}

// ── Uploads ──────────────────────────────────────────────────────────────

type State = "queued" | "sending" | "processing" | "done" | "failed" | "cancelled";

interface Row {
  li: HTMLLIElement;
  state: HTMLElement;
  meter: HTMLProgressElement;
  again: HTMLButtonElement;
}

interface Item {
  file: File;
  name: string;
  size: number;
  acked: number;
  flight: number;
  state: State;
  error: string;
  id?: string;
  album?: string;
  xhr?: XMLHttpRequest | null;
  preview?: string;
  row?: Row;
}

class Failure extends Error {
  readonly retry: boolean;
  constructor(message: string, retry: boolean) {
    super(message);
    this.retry = retry;
  }
}

const queue = {
  items: [] as Item[],
  active: 0,
  album: null as string | null,
  albumIsNew: false,
  samples: [] as [number, number][],
};

const pending = () =>
  queue.items.filter((i) => i.state === "queued" || i.state === "sending" || i.state === "processing");

/** A state another part of the script may have changed meanwhile: a cancel lands between awaits. */
const cancelled = (item: Item) => item.state === "cancelled";

addEventListener("beforeunload", (ev) => {
  if (pending().length) {
    ev.preventDefault();
    // What browsers before preventDefault was honoured here read instead.
    ev.returnValue = "";
  }
});

/** A status that sending the same request again may get past. */
const retryable = (status: number) => status === 408 || status === 429 || (status >= 500 && status !== 507);

/** The album being made for a batch: a second drop while it is on its way joins it rather than making another. */
let making: Promise<string> | null = null;

/** Where this batch goes: the album on show, the one a batch still going went to, or a new one made for it. */
async function target(running: boolean) {
  const shown = albumId();
  if (shown) return shown;
  if (running && making) return making;
  if (running && queue.album) return queue.album;
  making = (async () => {
    const res = await api("POST", "/api/albums", {});
    const answer = await json<{ id: string }>(res);
    if (!res.ok || !answer.id) throw new Failure(say("album failed"), false);
    queue.album = answer.id;
    queue.albumIsNew = true;
    return answer.id;
  })();
  try {
    return await making;
  } finally {
    making = null;
  }
}

async function add(list: FileList | File[]) {
  const files = [...list];
  if (!files.length) return;
  const items: Item[] = files.map((file) => {
    const item: Item = {
      file,
      name: file.name || say("pasted image"),
      size: file.size,
      acked: 0,
      flight: 0,
      state: "queued",
      error: "",
    };
    const video = MAX_VIDEO > 0 && isVideo(file);
    const max = video ? MAX_VIDEO : MAX;
    if (/\.(heic|heif)$/i.test(item.name) || /hei[cf]/.test(file.type)) {
      item.state = "failed";
      item.error = say("heic");
    } else if (file.type && !TYPES.has(file.type) && !video) {
      item.state = "failed";
      item.error = MAX_VIDEO > 0 ? say("not media") : say("not an image");
    } else if (file.size > max) {
      item.state = "failed";
      item.error = say("too big", { size: human(file.size), max: human(max) });
    } else if (file.size === 0) {
      item.state = "failed";
      item.error = say("empty");
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
      files: go.map((i) => ({ name: i.name, size: i.size, type: i.file.type })),
      as_recorded: $<HTMLInputElement>("[data-as-recorded]")?.checked ?? false,
    });
    const answer = await json<{ uploads: { id: string }[]; error: string }>(res);
    if (!res.ok || !answer.uploads) throw new Failure(answer.error || say("refused", { status: res.status }), false);
    answer.uploads.forEach((u, n) => {
      const item = go[n];
      if (item) {
        item.id = u.id;
        item.album = album;
      }
    });
  } catch (e) {
    for (const i of go) {
      i.state = "failed";
      i.error = e instanceof Error ? e.message : say("failed");
    }
  }
  tray.draw();
  pump();
}

function pump() {
  while (queue.active < PARALLEL) {
    const next = queue.items.find((i) => i.state === "queued" && i.id);
    if (!next) break;
    void send(next);
  }
  tray.draw();
}

function put(item: Item, start: number, end: number) {
  return new Promise<number>((resolve, reject) => {
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
      let answer: { received?: number; error?: string } = {};
      try {
        answer = JSON.parse(xhr.responseText) as typeof answer;
      } catch {}
      // A 409 says where the server got to; anything without a place to
      // resume from is an answer, not a resume point.
      if (xhr.status === 200 || (xhr.status === 409 && typeof answer.received === "number")) {
        resolve(answer.received ?? start);
      } else if (xhr.status === 401) reject(new Failure(say("signed out"), false));
      else if (retryable(xhr.status))
        reject(new Failure(answer.error || say("server failed", { status: xhr.status }), true));
      else reject(new Failure(answer.error || say("refused", { status: xhr.status }), false));
    };
    xhr.onerror = () => {
      item.flight = 0;
      reject(new Failure(say("dropped"), true));
    };
    xhr.ontimeout = xhr.onerror;
    xhr.onabort = () => {
      item.flight = 0;
      reject(new Failure(say("cancelled"), false));
    };
    xhr.send(item.file.slice(start, end));
  });
}

async function send(item: Item) {
  queue.active++;
  item.state = "sending";
  let tries = 0;
  try {
    for (;;) {
      if (cancelled(item)) throw new Failure(say("cancelled"), false);
      try {
        while (item.acked < item.size) {
          if (cancelled(item)) throw new Failure(say("cancelled"), false);
          const end = Math.min(item.acked + CHUNK, item.size);
          item.acked = await put(item, item.acked, end);
          tries = 0;
        }
        item.state = "processing";
        tray.draw();
        // Processing carries on if the connection drops, and asking again
        // joins it.
        const res = await api("POST", `/api/uploads/${item.id}/finish`).catch(() => {
          throw new Failure(say("dropped"), true);
        });
        const answer = await json<{ received: number; error: string }>(res);
        if (res.ok) {
          item.state = "done";
          break;
        }
        if (res.status === 409 && typeof answer.received === "number") {
          item.acked = answer.received;
          item.state = "sending";
          continue;
        }
        throw new Failure(answer.error || say("processing failed", { status: res.status }), retryable(res.status));
      } catch (e) {
        if (!(e instanceof Failure) || !e.retry || tries >= RETRIES || cancelled(item)) throw e;
        tries++;
        item.state = "sending";
        item.error = say("retrying", { why: e.message });
        tray.draw();
        await sleep(1000 * 2 ** (tries - 1));
        // Resume from what the server has, whatever the last answer said.
        const res = await api("GET", `/api/uploads/${item.id}`).catch(() => null);
        if (res?.ok) item.acked = (await json<{ received: number }>(res)).received ?? item.acked;
        item.error = "";
      }
    }
  } catch (e) {
    // A cancel set the state itself; anything else is a failure.
    if (!cancelled(item)) item.state = "failed";
    item.error = e instanceof Error ? e.message : say("failed");
  } finally {
    item.flight = 0;
    item.xhr = null;
    queue.active--;
    pump();
  }
}

function retry(item: Item) {
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

interface Tray {
  open(): void;
  draw(): void;
  wake(): void;
}

const tray: Tray = (() => {
  const root = $("#tray");
  let built = false;
  let shown = 0;
  let last = 0;
  let frame = 0;
  let textAt = 0;
  let expanded = matchMedia("(min-width: 721px)").matches;
  let target = { sent: 0, total: 0, live: 0 };

  const make = <K extends keyof HTMLElementTagNameMap>(tag: K, cls = "", text = "") => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text) e.textContent = text;
    return e;
  };

  const title = make("span", "tray-title");
  const rate = make("span", "tray-rate");
  const toggleButton = make("button", "quiet");
  const cancel = make("button", "quiet", say("cancel"));
  const close = make("button", "quiet", say("close"));
  const meter = make("progress", "meter");
  const foot = make("div", "tray-foot");
  const list = make("ol", "tray-list");
  meter.max = 1;
  meter.value = 0;

  const build = () => {
    if (built || !root) return;
    built = true;
    const line = make("div", "tray-line");
    line.append(title, make("span", "flex-1"), rate, toggleButton, cancel, close);
    root.append(line, meter, foot, list);
    toggleButton.onclick = () => {
      expanded = !expanded;
      draw();
    };
    cancel.onclick = cancelAll;
    close.onclick = () => {
      root.hidden = true;
      for (const i of queue.items) if (i.preview) URL.revokeObjectURL(i.preview);
      queue.items = queue.items.filter((i) => pending().includes(i));
    };
  };

  const row = (item: Item) => {
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
      const bar = make("progress", "meter");
      bar.max = 1;
      const again = make("button", "quiet", say("retry"));
      again.onclick = () => retry(item);
      const text = make("div", "tray-text");
      text.append(name, state);
      const main = make("div", "tray-main");
      main.append(text, bar);
      li.append(thumb, main, again);
      item.row = { li, state, meter: bar, again };
    }
    return item.row;
  };

  const words: Partial<Record<State, string>> = {
    queued: say("waiting"),
    processing: say("processing"),
    done: say("done"),
    cancelled: say("cancelled"),
  };

  // Text changes four times a second at most; bars move every frame.
  const drawText = (now: number) => {
    const items = queue.items.filter((i) => i.state !== "cancelled");
    const live = pending();
    const done = items.filter((i) => i.state === "done").length;
    const failed = items.filter((i) => i.state === "failed").length;
    const kept = items.filter((i) => i.state !== "failed");
    const total = kept.reduce((s, i) => s + i.size, 0);
    const sent = kept.reduce((s, i) => s + Math.min(i.size, i.acked + i.flight), 0);
    const processing = live.filter((i) => i.state === "processing").length;

    // Rate over the last few seconds, from samples of bytes sent.
    queue.samples.push([now, sent]);
    while (queue.samples.length > 2 && now - (queue.samples[0]?.[0] ?? now) > 4000) queue.samples.shift();
    const [t0, b0] = queue.samples[0] ?? [now, sent];
    const bps = now - t0 > 400 ? ((sent - b0) * 1000) / (now - t0) : 0;

    if (live.length) {
      title.textContent =
        processing && sent >= total
          ? sayN("processing n", processing)
          : say("uploaded of", { done, total: items.length - failed });
      rate.textContent =
        bps > 0 && sent < total ? say("rate", { rate: human(bps), left: span((total - sent) / bps) }) : "";
    } else {
      title.textContent = failed
        ? say("uploaded failed", { done, failed })
        : say("uploaded of", { done, total: done });
      rate.textContent = "";
    }
    foot.replaceChildren(say("bytes of", { sent: human(Math.min(sent, total)), total: human(total) }));
    if (!live.length && queue.albumIsNew && queue.album && done) {
      const a = make("a", "", say("open the album"));
      a.href = `/albums/${queue.album}`;
      foot.append(" · ", a);
    }
    cancel.hidden = !live.some((i) => i.state !== "processing");
    close.hidden = live.length > 0;
    toggleButton.textContent = expanded ? say("hide") : say("details");
    list.hidden = !expanded;

    if (expanded) {
      const rows = items.map((i) => {
        const r = row(i);
        r.li.dataset["state"] = i.state;
        r.state.textContent = i.error || words[i.state] || human(i.size);
        r.again.hidden = !(i.state === "failed" && i.id);
        return r.li;
      });
      if (rows.length !== list.children.length || rows.some((r, n) => list.children[n] !== r)) {
        list.replaceChildren(...rows);
      }
    }
    return { sent, total, live: live.length };
  };

  const draw = () => {
    build();
    textAt = 0;
    wake();
  };

  const tick = (now: number) => {
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
    const ease = 1 - Math.exp(-dt / 140);
    shown += (goal - shown) * ease;
    // Close enough, or nothing on its way and the goal behind what is shown
    // (a file failed or was cancelled): settle, rather than run backwards.
    if (Math.abs(goal - shown) < 0.0005 || (!target.live && shown > goal)) shown = goal;
    meter.value = shown;
    for (const i of queue.items) {
      if (!i.row || !expanded) continue;
      const m = i.row.meter;
      if (i.state === "processing" || i.state === "queued") {
        m.removeAttribute("value");
      } else {
        const g = i.size ? Math.min(1, (i.acked + i.flight) / i.size) : 0;
        const v = Number(m.getAttribute("value") ?? 0);
        m.value = i.state === "done" ? 1 : v + (g - v) * ease;
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
      build();
      if (root) root.hidden = false;
      wake();
    },
    draw,
    wake,
  };
})();

// ── Ways in: the picker, dropping, pasting ───────────────────────────────

$<HTMLInputElement>("#pick")?.addEventListener("change", (ev) => {
  const input = ev.target as HTMLInputElement;
  if (input.files) void add(input.files);
  input.value = "";
});

{
  let depth = 0;
  const drop = $("#drop");
  const hasFiles = (ev: DragEvent) => [...(ev.dataTransfer?.types ?? [])].includes("Files");
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
    if ($("[data-dropzone]") && ev.dataTransfer) void add(ev.dataTransfer.files);
  });
}

addEventListener("paste", (ev) => {
  const files = [...(ev.clipboardData?.files ?? [])].filter(
    (f) => f.type.startsWith("image/") || (MAX_VIDEO > 0 && isVideo(f)),
  );
  if (!files.length || !$("[data-dropzone]")) return;
  ev.preventDefault();
  void add(files);
});

// Whether videos are kept as recorded is remembered between visits: a
// preference, not something the server keeps.
for (const box of $$<HTMLInputElement>("[data-as-recorded]")) {
  try {
    box.checked = localStorage.getItem("dither.as-recorded") === "true";
  } catch {}
  box.addEventListener("change", () => {
    try {
      localStorage.setItem("dither.as-recorded", String(box.checked));
    } catch {}
  });
}

// Datastar expressions reach these.
window.dither = { show, selecting, add };

// A module, so nothing above becomes a global a page could collide with.
export {};
