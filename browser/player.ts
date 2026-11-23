// The video player: the bar templates/player.html draws in place of the
// browser's own controls, which stay for a page without script. Every word
// it shows comes from the markup; a toggle's other label is in `data-alt`.
//
// Players are enhanced as they appear, so one the app's viewer patches in
// later works as one on a page does. A tap shows the bar, then plays or
// pauses; a double tap on either side skips ten seconds; the keys are
// YouTube's. Only one player sounds at a time, and a short silent clip plays
// on a loop while it is on screen, unless motion is to be reduced.

interface Player {
  root: HTMLElement;
  video: HTMLVideoElement & { webkitEnterFullscreen?: () => void };
  seek: HTMLInputElement | null;
  volume: HTMLInputElement | null;
  now: HTMLElement | null;
  speed: HTMLElement | null;
  looping: boolean;
  /** Paused by a person, so a clip coming back on screen stays paused. */
  held: boolean;
  dragging: boolean;
  idle: number;
  tap: { at: number; timer: number } | null;
}

const SPEEDS = [0.5, 1, 1.25, 1.5, 2];
const SKIP = 10;
const NUDGE = 5;
const players = new Map<HTMLElement, Player>();
const still = matchMedia("(prefers-reduced-motion: reduce)");
const fine = matchMedia("(pointer: fine)");

const stored = {
  get(key: string) {
    try {
      return localStorage.getItem(`dither.player.${key}`);
    } catch {
      return null;
    }
  },
  set(key: string, value: string) {
    try {
      localStorage.setItem(`dither.player.${key}`, value);
    } catch {}
  },
};

/** Seconds as a clock reads them, as the server writes them (`video::clock`). */
function clock(seconds: number) {
  const s = Math.max(0, Math.round(Number.isFinite(seconds) ? seconds : 0));
  const h = Math.floor(s / 3600);
  const m = Math.floor(s / 60) % 60;
  const ss = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

const length = (p: Player) =>
  Number.isFinite(p.video.duration) && p.video.duration > 0 ? p.video.duration : Number(p.root.dataset["length"]) || 0;

/** A toggle's label for its state: its own, or the one in `data-alt`. */
function label(button: Element | null, alternate: boolean) {
  if (!(button instanceof HTMLElement)) return;
  const own = (button.dataset["own"] ??= button.getAttribute("aria-label") ?? "");
  button.setAttribute("aria-label", alternate ? (button.dataset["alt"] ?? own) : own);
}

const act = (p: Player, name: string) => p.root.querySelector(`[data-act="${name}"]`);

function draw(p: Player) {
  const { video, root } = p;
  const total = length(p);
  const at = video.currentTime;
  root.classList.toggle("playing", !video.paused);
  root.classList.toggle("muted", video.muted);
  label(act(p, "toggle"), !video.paused);
  label(act(p, "mute"), video.muted);
  if (p.now) p.now.textContent = clock(at);
  if (p.seek && !p.dragging) {
    p.seek.max = String(total);
    p.seek.value = String(at);
  }
  if (p.seek) {
    p.seek.setAttribute("aria-valuetext", `${clock(at)} / ${clock(total)}`);
    let buffered = 0;
    for (let i = 0; i < video.buffered.length; i++) {
      if (video.buffered.start(i) <= at + 0.5) buffered = Math.max(buffered, video.buffered.end(i));
    }
    const share = (n: number) => `${total > 0 ? Math.min(100, (n / total) * 100) : 0}%`;
    p.seek.style.setProperty("--played", share(Number(p.seek.value)));
    p.seek.style.setProperty("--buffered", share(buffered));
  }
  if (p.volume) {
    p.volume.value = String(video.muted ? 0 : video.volume);
    p.volume.style.setProperty("--played", `${Number(p.volume.value) * 100}%`);
  }
  if (p.speed) p.speed.textContent = `${video.playbackRate}×`;
}

/** Show the bar, and hide it again after a while if the video is playing. */
function wake(p: Player) {
  p.root.classList.remove("idle");
  clearTimeout(p.idle);
  p.idle = window.setTimeout(() => {
    const busy = p.dragging || p.video.paused || p.root.querySelector("[data-player-controls]:focus-within");
    if (!busy) p.root.classList.add("idle");
  }, 2500);
}

function play(p: Player) {
  p.held = false;
  p.video.play().catch(() => {});
}

function toggle(p: Player) {
  if (p.video.paused) play(p);
  else {
    p.held = true;
    p.video.pause();
  }
}

function skip(p: Player, by: number) {
  const v = p.video;
  v.currentTime = Math.min(Math.max(0, v.currentTime + by), length(p));
  const jump = p.root.querySelector<HTMLElement>(".player-jump");
  if (jump) {
    jump.textContent = `${by > 0 ? "+" : "−"}${Math.abs(by)}`;
    jump.classList.toggle("back", by < 0);
    jump.classList.remove("shown");
    void jump.offsetWidth;
    jump.classList.add("shown");
  }
  draw(p);
}

function mute(p: Player, muted = !p.video.muted) {
  p.video.muted = muted;
  if (!muted && p.video.volume === 0) p.video.volume = 1;
  if (!p.looping) stored.set("muted", String(muted));
}

function speed(p: Player, step: number) {
  const now = SPEEDS.indexOf(p.video.playbackRate);
  const next = step === 0 ? (now + 1) % SPEEDS.length : Math.min(Math.max(0, (now < 0 ? 1 : now) + step), SPEEDS.length - 1);
  p.video.playbackRate = SPEEDS[next] ?? 1;
  draw(p);
}

function full(p: Player) {
  if (document.fullscreenElement) {
    void document.exitFullscreen().catch(() => {});
  } else if (document.fullscreenEnabled) {
    void p.root.requestFullscreen().catch(() => {});
  } else {
    // iOS Safari takes only the video full screen, with its own controls.
    p.video.webkitEnterFullscreen?.();
  }
}

function pip(p: Player) {
  if (document.pictureInPictureElement) void document.exitPictureInPicture().catch(() => {});
  else void p.video.requestPictureInPicture().catch(() => {});
}

/** Handle a key for `p`. `inside` when focus is in the player, which the arrows need. */
function key(p: Player, ev: KeyboardEvent, inside: boolean) {
  if (ev.metaKey || ev.ctrlKey || ev.altKey) return false;
  const k = ev.key.length === 1 ? ev.key.toLowerCase() : ev.key;
  switch (k) {
    case " ":
    case "k":
      toggle(p);
      break;
    case "j":
      skip(p, -SKIP);
      break;
    case "l":
      skip(p, SKIP);
      break;
    case "ArrowLeft":
    case "ArrowRight":
      if (!inside) return false;
      skip(p, k === "ArrowLeft" ? -NUDGE : NUDGE);
      break;
    case "ArrowUp":
    case "ArrowDown":
      if (!inside || !p.volume) return false;
      p.video.muted = false;
      p.video.volume = Math.min(1, Math.max(0, p.video.volume + (k === "ArrowUp" ? 0.1 : -0.1)));
      break;
    case "m":
      if (!p.volume) return false;
      mute(p);
      break;
    case "f":
      full(p);
      break;
    case "<":
    case ">":
      speed(p, k === "<" ? -1 : 1);
      break;
    default:
      if (!/^[0-9]$/.test(k)) return false;
      p.video.currentTime = (length(p) * Number(k)) / 10;
  }
  ev.preventDefault();
  wake(p);
  return true;
}

function enhance(root: HTMLElement) {
  const video = root.querySelector("video");
  const controls = root.querySelector<HTMLElement>("[data-player-controls]");
  if (!video || !controls || players.has(root)) return;
  const p: Player = {
    root,
    video,
    seek: root.querySelector<HTMLInputElement>("[data-seek]"),
    volume: root.querySelector<HTMLInputElement>("[data-volume]"),
    now: root.querySelector<HTMLElement>("[data-now]"),
    speed: root.querySelector<HTMLElement>("[data-speed]"),
    looping: root.classList.contains("looping"),
    held: false,
    dragging: false,
    idle: 0,
    tap: null,
  };
  players.set(root, p);
  video.controls = false;
  controls.hidden = false;
  root.classList.add("enhanced");
  root.tabIndex = -1;
  const start = root.querySelector<HTMLElement>(".player-start");
  if (start) start.hidden = p.looping && !still.matches;
  const pipButton = act(p, "pip");
  if (pipButton instanceof HTMLElement) pipButton.hidden = !document.pictureInPictureEnabled || video.disablePictureInPicture;
  if (!p.looping) {
    video.muted = stored.get("muted") === "true";
    const level = Number(stored.get("volume"));
    if (stored.get("volume") !== null && level >= 0 && level <= 1) video.volume = level;
  }

  const redraw = () => draw(p);
  for (const name of ["timeupdate", "progress", "durationchange", "loadedmetadata", "ratechange", "seeked"]) {
    video.addEventListener(name, redraw);
  }
  video.addEventListener("volumechange", () => {
    if (!p.looping) stored.set("volume", String(video.volume));
    draw(p);
  });
  video.addEventListener("play", () => {
    root.classList.add("started");
    // One sound at a time: a clip that loops silently is not one.
    if (!p.looping) {
      for (const other of players.values()) if (other !== p && !other.looping) other.video.pause();
    }
    session(p);
    draw(p);
    wake(p);
  });
  video.addEventListener("pause", () => {
    draw(p);
    wake(p);
  });
  for (const name of ["waiting", "stalled"]) video.addEventListener(name, () => root.classList.add("busy"));
  for (const name of ["playing", "canplay", "pause", "error"]) {
    video.addEventListener(name, () => root.classList.remove("busy"));
  }
  video.addEventListener("error", () => {
    root.classList.add("failed");
    controls.hidden = true;
    const failed = root.querySelector<HTMLElement>(".player-failed");
    if (failed) failed.hidden = false;
    if (start) start.hidden = true;
  });

  controls.addEventListener("click", (ev) => {
    const button = (ev.target as Element).closest<HTMLElement>("[data-act]");
    if (!button) return;
    const name = button.dataset["act"];
    if (name === "toggle") toggle(p);
    else if (name === "mute") mute(p);
    else if (name === "speed") speed(p, 0);
    else if (name === "pip") pip(p);
    else if (name === "full") full(p);
    wake(p);
  });
  start?.addEventListener("click", (ev) => {
    ev.stopPropagation();
    play(p);
  });

  if (p.seek) {
    const seek = p.seek;
    const quick = (to: number) => {
      if (typeof video.fastSeek === "function") video.fastSeek(to);
      else video.currentTime = to;
    };
    seek.addEventListener("pointerdown", () => (p.dragging = true));
    seek.addEventListener("input", () => {
      p.dragging = true;
      quick(Number(seek.value));
      if (p.now) p.now.textContent = clock(Number(seek.value));
      seek.style.setProperty("--played", `${(Number(seek.value) / (length(p) || 1)) * 100}%`);
    });
    seek.addEventListener("change", () => {
      video.currentTime = Number(seek.value);
      p.dragging = false;
      draw(p);
    });
    seek.addEventListener("pointerup", () => (p.dragging = false));
  }
  p.volume?.addEventListener("input", () => {
    const level = Number(p.volume?.value ?? 1);
    video.volume = level;
    mute(p, level === 0);
  });
  if (p.volume) p.volume.hidden = !fine.matches;

  // A tap on the picture: with a finger, the first shows the bar and a
  // double tap at either side skips; with a mouse, a click plays or pauses
  // and a double click goes full screen.
  root.addEventListener("pointermove", (ev) => {
    if (ev.pointerType === "mouse") wake(p);
  });
  root.addEventListener("pointerup", (ev) => {
    const target = ev.target as Element;
    if (target.closest("[data-player-controls], .player-start, .player-failed") || ev.button !== 0) return;
    if (ev.pointerType === "mouse") {
      toggle(p);
      wake(p);
      return;
    }
    const box = root.getBoundingClientRect();
    const third = (ev.clientX - box.left) / box.width;
    const side = third < 1 / 3 ? -1 : third > 2 / 3 ? 1 : 0;
    const now = performance.now();
    if (p.tap && now - p.tap.at < 300 && side !== 0 && root.classList.contains("started")) {
      clearTimeout(p.tap.timer);
      p.tap = { at: now, timer: 0 };
      skip(p, side * SKIP);
      return;
    }
    const hidden = root.classList.contains("idle") && !video.paused;
    p.tap = {
      at: now,
      timer: window.setTimeout(() => {
        p.tap = null;
        if (hidden) wake(p);
        else toggle(p);
      }, 250),
    };
  });
  root.addEventListener("dblclick", (ev) => {
    if (!(ev.target as Element).closest("[data-player-controls]")) full(p);
  });
  // Handled here, the keys go no further: the viewer's arrows move between
  // images only when focus is not in a player.
  root.addEventListener("keydown", (ev) => {
    if (key(p, ev, true)) ev.stopPropagation();
  });
  root.addEventListener("focusin", () => wake(p));

  if (p.looping) watch.observe(root);
  draw(p);
}

function release(root: HTMLElement) {
  const p = players.get(root);
  if (!p) return;
  watch.unobserve(root);
  clearTimeout(p.idle);
  if (p.tap) clearTimeout(p.tap.timer);
  p.video.pause();
  players.delete(root);
}

/** The system's media controls name what plays and drive it. */
function session(p: Player) {
  if (!("mediaSession" in navigator) || p.looping) return;
  const title = p.root.dataset["title"] ?? "";
  navigator.mediaSession.metadata = new MediaMetadata({ title });
  const handlers: [MediaSessionAction, MediaSessionActionHandler][] = [
    ["play", () => play(p)],
    ["pause", () => p.video.pause()],
    ["seekbackward", () => skip(p, -SKIP)],
    ["seekforward", () => skip(p, SKIP)],
    ["seekto", (d) => (p.video.currentTime = d.seekTime ?? p.video.currentTime)],
  ];
  for (const [action, handler] of handlers) {
    try {
      navigator.mediaSession.setActionHandler(action, handler);
    } catch {}
  }
}

// A looping clip plays while at least half of it is on screen.
const watch = new IntersectionObserver(
  (entries) => {
    for (const entry of entries) {
      const p = players.get(entry.target as HTMLElement);
      if (!p) continue;
      if (entry.isIntersecting && !still.matches && !p.held) p.video.play().catch(() => {});
      else if (!entry.isIntersecting) p.video.pause();
    }
  },
  { threshold: 0.5 },
);

// Keys reach the page's only player even when focus is elsewhere, unless it
// is in a field or on a control a key works itself.
document.addEventListener("keydown", (ev) => {
  const live = [...players.values()].filter((p) => p.root.isConnected);
  const [only] = live;
  if (live.length !== 1 || !only || ev.defaultPrevented) return;
  const at = document.activeElement;
  if (at?.closest("[data-player]")) return;
  if (at?.matches("input, textarea, select, [contenteditable]")) return;
  if (ev.key === " " && at?.matches("button, a, summary")) return;
  key(only, ev, false);
});

document.addEventListener("fullscreenchange", () => {
  for (const p of players.values()) {
    const on = document.fullscreenElement === p.root;
    p.root.classList.toggle("full", on);
    label(act(p, "full"), on);
  }
});

for (const root of document.querySelectorAll<HTMLElement>("[data-player]")) enhance(root);
new MutationObserver((records) => {
  for (const r of records) {
    for (const node of r.addedNodes) {
      if (!(node instanceof HTMLElement)) continue;
      if (node.matches("[data-player]")) enhance(node);
      for (const root of node.querySelectorAll<HTMLElement>("[data-player]")) enhance(root);
    }
  }
  for (const root of [...players.keys()]) if (!root.isConnected) release(root);
}).observe(document.body, { childList: true, subtree: true });

export {};
