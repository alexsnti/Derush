import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { confirm, open, save } from "@tauri-apps/plugin-dialog";
import type {
  Chapters, Loaded, ModelInfo, Overlay, Position, Project, RetakeOut, Segment, SegmentKind, SentenceView,
  SilenceParams, Source, TitleStyle, Waveform,
} from "./types";
import { programDuration, sourceToProgram } from "./types";

// ---------------------------------------------------------------- état

const $ = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
const video = $<HTMLVideoElement>("video");
const canvas = $<HTMLCanvasElement>("tl");
const ctx = canvas.getContext("2d")!;

interface Pos { source: string; time: number }
interface Sel { source: string; start: number; end: number }

const state = {
  project: null as Project | null,
  waves: {} as Record<string, Waveform>,
  projectPath: null as string | null,
  params: { min_silence: 0.6, padding: 0.15 } as SilenceParams,
  dirty: false,
  mode: "program" as "program" | "raw",
  loaded: null as string | null,
  cursor: { source: "", time: 0 } as Pos,
  selection: null as Sel | null,
  view: { start: 0, pps: 10 },
  zoom: 20,
  // Texte
  sentences: [] as SentenceView[],
  retakes: null as RetakeOut[] | null,
  models: [] as ModelInfo[],
  model: "turbo",
  prompt: "",
  search: "",
  chapters: null as Chapters | null,
  // Synchro avec le fichier (modifications faites par Claude)
  mtime: null as number | null,
};
let saveTimer: ReturnType<typeof setTimeout> | undefined;
let onCancel: (() => void) | null = null;
let jumping = false;

const VIDEO_EXT = ["mp4", "mov", "m4v", "mkv", "avi", "mts"];

// ---------------------------------------------------------------- outils

const esc = (s: string) =>
  s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
const basename = (p: string) => p.split(/[\\/]/).pop() ?? p;

function fmt(t: number, tenths = false): string {
  t = Math.max(0, t);
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const s = t % 60;
  const ss = tenths ? s.toFixed(1).padStart(4, "0") : Math.floor(s).toString().padStart(2, "0");
  return h ? `${h}:${m.toString().padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

function toast(msg: string, error = false) {
  const el = $("toast");
  el.textContent = msg;
  el.classList.toggle("error", error);
  el.hidden = false;
  clearTimeout((toast as any).t);
  (toast as any).t = setTimeout(() => (el.hidden = true), error ? 8000 : 3000);
}

function busy(text: string | null, cancel: (() => void) | null = null) {
  $("busy").hidden = text === null;
  if (text) $("busy-text").textContent = text;
  onCancel = text === null ? null : cancel;
  $("busy-cancel").hidden = !onCancel;
}
$("busy-cancel").addEventListener("click", () => onCancel?.());

function setDirty(d = true) {
  state.dirty = d;
  // Enregistrement automatique dès que le projet a un fichier : c'est ce fichier que lit Claude.
  clearTimeout(saveTimer);
  if (d && state.projectPath) saveTimer = setTimeout(() => saveProject(true), 1200);
  $("dirty").style.visibility = d ? "visible" : "hidden";
}

const P = () => state.project!;
const src = (id: string) => P().sources.find((s) => s.id === id)!;
const order = (id: string) => P().sources.findIndex((s) => s.id === id);

// Coordonnées « globales » de la timeline : les rushes bout à bout, en temps brut.
function offsets(): Map<string, number> {
  const m = new Map<string, number>();
  let acc = 0;
  for (const s of P().sources) {
    m.set(s.id, acc);
    acc += s.duration;
  }
  return m;
}
const rawTotal = () => P().sources.reduce((t, s) => t + s.duration, 0);
const toGlobal = (p: Pos) => (offsets().get(p.source) ?? 0) + p.time;
function toLocal(g: number): Pos {
  let acc = 0;
  for (const s of P().sources) {
    if (g < acc + s.duration) return { source: s.id, time: Math.max(0, g - acc) };
    acc += s.duration;
  }
  const last = P().sources[P().sources.length - 1];
  return { source: last.id, time: last.duration };
}

const segmentAt = (p: Pos) =>
  P().segments.find((s) => s.source === p.source && p.time >= s.start && p.time < s.end);

function nextKept(p: Pos): Segment | undefined {
  const o = order(p.source);
  return P().segments.find(
    (s) => s.keep && (order(s.source) > o || (s.source === p.source && s.start >= p.time - 1e-3)),
  );
}

// ---------------------------------------------------------------- lecteur

async function loadSource(id: string) {
  if (state.loaded === id) return;
  state.loaded = id;
  const ready = new Promise<void>((resolve, reject) => {
    const ok = () => (cleanup(), resolve());
    const ko = () => (cleanup(), reject(new Error(`Aperçu impossible pour ${basename(src(id).path)}. L'analyse et l'export ne sont pas concernés.`)));
    const timer = setTimeout(ko, 10_000);
    const cleanup = () => {
      clearTimeout(timer);
      video.removeEventListener("loadedmetadata", ok);
      video.removeEventListener("error", ko);
    };
    video.addEventListener("loadedmetadata", ok);
    video.addEventListener("error", ko);
  });
  video.src = convertFileSrc(src(id).path);
  await ready;
}

async function seek(p: Pos, play?: boolean) {
  const resume = play ?? !video.paused;
  state.cursor = { ...p };
  try {
    await loadSource(p.source);
  } catch (e) {
    moved();
    return toast(String((e as Error).message ?? e), true);
  }
  video.currentTime = p.time;
  state.cursor = { ...p };
  follow();
  moved();
  if (resume) await video.play().catch(() => {});
}

async function jumpTo(seg: Segment | undefined) {
  if (!seg) {
    video.pause();
    return;
  }
  jumping = true;
  try {
    if (seg.source === state.loaded) video.currentTime = seg.start;
    else await seek({ source: seg.source, time: seg.start }, true);
  } finally {
    jumping = false;
  }
}

async function togglePlay() {
  if (!state.project) return;
  if (!video.paused) return video.pause();
  if (state.mode === "program") {
    const here = segmentAt(state.cursor);
    if (!here || !here.keep) {
      // Hors d'une partie gardée : on part de la suivante, ou du début si on est à la fin.
      const next = nextKept(state.cursor) ?? P().segments.find((s) => s.keep);
      if (!next) return toast("Tout est coupé : il n'y a rien à lire.");
      return seek({ source: next.source, time: next.start }, true);
    }
  }
  await loadSource(state.cursor.source);
  await video.play().catch(() => {});
}

video.addEventListener("play", () => ($("btn-play").textContent = "❚❚"));
video.addEventListener("pause", () => ($("btn-play").textContent = "▶"));
video.addEventListener("ended", () => {
  if (state.mode !== "program" || !state.loaded) return;
  const next = nextKept({ source: state.loaded, time: src(state.loaded).duration + 1 });
  if (next) jumpTo(next);
});

function tick() {
  if (state.project && state.loaded && !jumping && !video.paused) {
    state.cursor = { source: state.loaded, time: video.currentTime };
    if (state.mode === "program") {
      const seg = segmentAt(state.cursor);
      if (!seg || !seg.keep) jumpTo(nextKept(state.cursor));
    }
    follow();
    moved();
  }
  requestAnimationFrame(tick);
}

/** Met à jour horloge, calques et timeline après un déplacement de la tête de lecture. */
function moved() {
  updateClock();
  updateLayer();
  draw();
  highlightSentence();
}

function updateClock() {
  const p = P();
  if (state.mode === "program") {
    const pt = sourceToProgram(p, state.cursor) ?? programDuration(p);
    $("clock").innerHTML = `<b>${fmt(pt)}</b> / ${fmt(programDuration(p))} <span class="muted">· montage</span>`;
  } else {
    const s = src(state.cursor.source);
    $("clock").innerHTML = `<b>${fmt(state.cursor.time, true)}</b> / ${fmt(s.duration)} <span class="muted">· ${esc(basename(s.path))}</span>`;
  }
}

// Calque titres/images, calé sur le cadre de la vidéo exportée.
let layerKey = "";
function updateLayer() {
  const p = P();
  const screen = $("screen");
  const layer = $("layer");
  const ar = p.export.width / p.export.height;
  const W = screen.clientWidth;
  const H = screen.clientHeight;
  const [w, h] = W / H > ar ? [H * ar, H] : [W, W / ar];
  Object.assign(layer.style, { left: `${(W - w) / 2}px`, top: `${(H - h) / 2}px`, width: `${w}px`, height: `${h}px` });
  layer.style.setProperty("--h", `${h}px`);

  const seg = segmentAt(state.cursor);
  const pt = seg?.keep ? sourceToProgram(p, state.cursor) : null;
  const visible = p.overlays.filter((o) => {
    const at = sourceToProgram(p, o.anchor);
    return pt !== null && at !== null && pt >= at && pt < at + o.duration;
  });
  const key = visible.map((o) => o.id + JSON.stringify(o.content)).join("|") + w;
  if (key === layerKey) return;
  layerKey = key;
  layer.innerHTML = visible
    .map((o) => {
      const c = o.content;
      if (c.type === "title") return `<div class="ov-title ${c.style}">${esc(c.text)}</div>`;
      const m = 4;
      const pos: Record<Position, string> = {
        "top-left": `left:${m}%;top:${m * ar}%`,
        "top-right": `right:${m}%;top:${m * ar}%`,
        "bottom-left": `left:${m}%;bottom:${m * ar}%`,
        "bottom-right": `right:${m}%;bottom:${m * ar}%`,
        center: "left:50%;top:50%;transform:translate(-50%,-50%)",
      };
      return `<img class="ov-image" style="width:${c.scale * 100}%;${pos[c.position]}" src="${convertFileSrc(c.path)}">`;
    })
    .join("");
}

// ---------------------------------------------------------------- timeline

let colors: Record<string, string> = {};
const hatches = new Map<string, CanvasPattern>();
function hatch(color: string): CanvasPattern {
  let p = hatches.get(color);
  if (!p) {
    const c = document.createElement("canvas");
    c.width = c.height = 8;
    const g = c.getContext("2d")!;
    g.fillStyle = colors.bg;
    g.fillRect(0, 0, 8, 8);
    g.strokeStyle = color;
    g.globalAlpha = 0.45;
    g.lineWidth = 1.5;
    g.beginPath();
    g.moveTo(-2, 10);
    g.lineTo(10, -2);
    g.moveTo(-2, 2);
    g.lineTo(2, -2);
    g.moveTo(6, 10);
    g.lineTo(10, 6);
    g.stroke();
    p = ctx.createPattern(c, "repeat")!;
    hatches.set(color, p);
  }
  return p;
}
function readColors() {
  const cs = getComputedStyle(document.documentElement);
  for (const k of ["bg", "keep", "wave", "silence", "retake", "manual", "accent", "select", "line", "muted", "text"])
    colors[k] = cs.getPropertyValue(`--${k}`).trim();
}

function fitView() {
  const minPps = canvas.clientWidth / Math.max(1, rawTotal());
  const maxPps = 200;
  state.view.pps = minPps * Math.pow(maxPps / minPps, state.zoom / 100);
  clampView();
}
function clampView() {
  const span = canvas.clientWidth / state.view.pps;
  state.view.start = Math.min(Math.max(0, state.view.start), Math.max(0, rawTotal() - span));
}
function follow() {
  const g = toGlobal(state.cursor);
  const span = canvas.clientWidth / state.view.pps;
  if (g < state.view.start || g > state.view.start + span * 0.92) {
    state.view.start = g - span * 0.1;
    clampView();
  }
}

const xOf = (g: number) => (g - state.view.start) * state.view.pps;
const gOf = (x: number) => state.view.start + x / state.view.pps;

function draw() {
  const dpr = window.devicePixelRatio || 1;
  const W = canvas.clientWidth;
  const H = canvas.clientHeight;
  if (canvas.width !== W * dpr || canvas.height !== H * dpr) {
    canvas.width = W * dpr;
    canvas.height = H * dpr;
  }
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.fillStyle = colors.bg;
  ctx.fillRect(0, 0, W, H);
  if (!state.project) return;

  const p = P();
  const off = offsets();
  const top = 18;
  const lane = H - top;
  const g0 = state.view.start;
  const g1 = gOf(W);

  // Segments.
  for (const s of p.segments) {
    const o = off.get(s.source)!;
    if (o + s.end < g0 || o + s.start > g1) continue;
    const x0 = xOf(o + s.start);
    const x1 = xOf(o + s.end);
    const w = Math.max(1, x1 - x0);
    if (s.keep) {
      ctx.fillStyle = colors.keep;
      ctx.fillRect(x0, top, w, lane);
    } else {
      // Partie coupée : fond sombre hachuré + liseré de la couleur du motif.
      const c = colors[s.kind === "speech" ? "manual" : s.kind] ?? colors.manual;
      ctx.fillStyle = hatch(c);
      ctx.fillRect(x0, top, w, lane);
      ctx.fillStyle = c;
      ctx.fillRect(x0, top, w, 3);
      ctx.fillRect(x0, H - 3, w, 3);
    }
  }

  // Forme d'onde : maximum des échantillons couverts par chaque colonne de pixels.
  for (let x = 0; x < W; x++) {
    const ga = gOf(x);
    if (ga >= rawTotal()) break;
    const pos = toLocal(ga);
    const wave = state.waves[pos.source];
    if (!wave) continue;
    const i0 = Math.floor(pos.time * wave.per_second);
    const i1 = Math.max(i0 + 1, Math.floor((pos.time + 1 / state.view.pps) * wave.per_second));
    let db = -90;
    for (let i = i0; i < i1 && i < wave.db.length; i++) db = Math.max(db, wave.db[i]);
    const v = Math.min(1, Math.max(0, (db + 60) / 60));
    const seg = segmentAt(pos);
    ctx.globalAlpha = seg?.keep ? 0.9 : 0.35;
    ctx.fillStyle = colors.wave;
    const bh = v * (lane - 8);
    ctx.fillRect(x, top + (lane - bh) / 2, 1, bh);
  }
  ctx.globalAlpha = 1;

  // Graduations, en temps de chaque rush.
  const step = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1800].find((s) => s * state.view.pps >= 70) ?? 3600;
  ctx.font = "11px -apple-system, system-ui, sans-serif";
  ctx.textBaseline = "top";
  for (const s of p.sources) {
    const o = off.get(s.id)!;
    if (o > g1 || o + s.duration < g0) continue;
    for (let t = Math.ceil(Math.max(0, g0 - o) / step) * step; t <= s.duration && o + t <= g1; t += step) {
      const x = xOf(o + t);
      ctx.fillStyle = colors.line;
      ctx.fillRect(x, 12, 1, 6);
      ctx.fillStyle = colors.muted;
      ctx.fillText(fmt(t), x + 3, 1);
    }
    // Début de rush : séparateur et nom.
    const xs = xOf(o);
    if (xs >= -1 && xs <= W) {
      ctx.fillStyle = colors.text;
      ctx.fillRect(xs, 0, 2, H);
      if (p.sources.length > 1) {
        ctx.fillStyle = colors.bg;
        const name = basename(s.path);
        const tw = ctx.measureText(name).width;
        ctx.fillRect(xs + 2, top, tw + 10, 16);
        ctx.fillStyle = colors.text;
        ctx.fillText(name, xs + 7, top + 2);
      }
    }
  }

  // Calques (repères en haut de la piste).
  for (const ov of p.overlays) {
    const x = xOf(off.get(ov.anchor.source)! + ov.anchor.time);
    if (x < -10 || x > W) continue;
    ctx.fillStyle = colors.accent;
    ctx.beginPath();
    ctx.moveTo(x, top);
    ctx.lineTo(x + 6, top);
    ctx.lineTo(x, top + 8);
    ctx.fill();
  }

  // Sélection et tête de lecture.
  if (state.selection) {
    const o = off.get(state.selection.source)!;
    ctx.fillStyle = colors.select;
    const x0 = xOf(o + state.selection.start);
    ctx.fillRect(x0, top, xOf(o + state.selection.end) - x0, lane);
  }
  const xp = xOf(toGlobal(state.cursor));
  ctx.fillStyle = colors.accent;
  ctx.fillRect(xp - 1, 0, 2, H);
}

let drag: { g: number; x: number; moved: boolean } | null = null;

canvas.addEventListener("mousedown", (e) => {
  if (!state.project) return;
  drag = { g: gOf(e.offsetX), x: e.offsetX, moved: false };
});
window.addEventListener("mousemove", (e) => {
  if (!drag || !state.project) return;
  const x = e.clientX - canvas.getBoundingClientRect().left;
  if (Math.abs(x - drag.x) < 4 && !drag.moved) return;
  drag.moved = true;
  const a = toLocal(drag.g);
  const s = src(a.source);
  const o = offsets().get(a.source)!;
  const b = Math.min(Math.max(gOf(x) - o, 0), s.duration); // la sélection reste dans un seul rush
  state.selection = { source: a.source, start: Math.min(a.time, b), end: Math.max(a.time, b) };
  updateSelection();
});
window.addEventListener("mouseup", () => {
  if (!drag) return;
  if (!drag.moved) {
    state.selection = null;
    updateSelection();
    seek(toLocal(drag.g));
  }
  drag = null;
});
canvas.addEventListener("dblclick", (e) => {
  if (!state.project) return;
  const seg = segmentAt(toLocal(gOf(e.offsetX)));
  if (!seg) return;
  const kind: SegmentKind = seg.keep && seg.kind === "speech" ? "manual" : seg.kind;
  markRange(seg.source, seg.start, seg.end, kind, !seg.keep);
});
canvas.addEventListener(
  "wheel",
  (e) => {
    if (!state.project) return;
    e.preventDefault();
    if (e.ctrlKey || e.metaKey) {
      // Pincement sur le trackpad : zoom autour du pointeur.
      const g = gOf(e.offsetX);
      state.zoom = Math.min(100, Math.max(0, state.zoom - e.deltaY * 0.5));
      $<HTMLInputElement>("zoom").value = String(state.zoom);
      fitView();
      state.view.start = g - e.offsetX / state.view.pps;
    } else {
      state.view.start += (e.deltaX || e.deltaY) / state.view.pps;
    }
    clampView();
    draw();
  },
  { passive: false },
);
$<HTMLInputElement>("zoom").addEventListener("input", (e) => {
  if (!state.project) return;
  const center = gOf(canvas.clientWidth / 2);
  state.zoom = Number((e.target as HTMLInputElement).value);
  fitView();
  state.view.start = center - canvas.clientWidth / 2 / state.view.pps;
  clampView();
  draw();
});

function updateSelection() {
  const s = state.selection;
  $<HTMLButtonElement>("btn-cut-sel").disabled = !s;
  $<HTMLButtonElement>("btn-keep-sel").disabled = !s;
  $("sel-info").textContent = s ? `${fmt(s.start, true)} → ${fmt(s.end, true)} (${(s.end - s.start).toFixed(1)} s)` : "";
  draw();
}

async function markSelection(keep: boolean) {
  const s = state.selection;
  if (!s || s.end - s.start < 0.05) return;
  await markRange(s.source, s.start, s.end, keep ? "speech" : "manual", keep);
  state.selection = null;
  updateSelection();
}

async function markRange(source: string, start: number, end: number, kind: SegmentKind, keep: boolean) {
  try {
    state.project = await invoke<Project>("mark_range", { project: P(), source, start, end, kind, keep });
    setDirty();
    refresh();
  } catch (e) {
    toast(String(e), true);
  }
}

// ---------------------------------------------------------------- panneaux

function refresh() {
  const p = state.project;
  for (const id of ["btn-save", "btn-export", "btn-play"]) $<HTMLButtonElement>(id).disabled = !p;
  $("empty").hidden = !!p;
  if (!p) return;
  const prog = programDuration(p);
  const raw = rawTotal();
  $("stats").innerHTML = `<b>${fmt(prog)}</b> montés sur ${fmt(raw)} de rushes`;
  renderCuts();
  renderDress();
  renderExport();
  renderText();
  updateClock();
  layerKey = "";
  updateLayer();
  draw();
}

const kindLabel: Record<SegmentKind, string> = {
  silence: "Blanc",
  retake: "Prise ratée",
  manual: "Coupé à la main",
  speech: "Parole",
};

function renderCuts() {
  const p = P();
  const raw = rawTotal();
  const prog = programDuration(p);
  const cuts = p.segments.filter((s) => !s.keep);
  const multi = p.sources.length > 1;
  $("tab-cuts").innerHTML = `
    <div class="kpis">
      <div class="kpi"><b>${fmt(raw)}</b><span>rushes</span></div>
      <div class="kpi"><b>${fmt(prog)}</b><span>montage</span></div>
      <div class="kpi"><b>−${Math.round((1 - prog / Math.max(raw, 1)) * 100)} %</b><span>${cuts.length} coupes</span></div>
    </div>
    <div class="group">
      <h3>Détection des blancs</h3>
      <div class="row"><label for="p-min">Blanc minimum coupé</label>
        <input id="p-min" data-param="min_silence" type="range" min="0.3" max="2" step="0.05" value="${state.params.min_silence}">
        <output>${state.params.min_silence.toFixed(2)} s</output></div>
      <div class="row"><label for="p-pad">Marge autour de la voix</label>
        <input id="p-pad" data-param="padding" type="range" min="0.05" max="0.5" step="0.01" value="${state.params.padding}">
        <output>${state.params.padding.toFixed(2)} s</output></div>
      <div class="row"><button class="btn small" data-act="reanalyze">Réanalyser les rushes</button></div>
      <p class="hint">Plus la marge est grande, plus le rythme est posé. Les coupes faites à la main sont refaites à la réanalyse.</p>
    </div>
    <div class="group">
      <h3>Coupes (${cuts.length})</h3>
      <div class="list">
        ${cuts
          .slice(0, 400)
          .map(
            (s) => `
          <div class="cut-item">
            <span class="dot" style="background:var(--${s.kind === "speech" ? "manual" : s.kind})"></span>
            <span><span class="when">${multi ? esc(basename(src(s.source).path)) + " · " : ""}${fmt(s.start, true)} · ${(s.end - s.start).toFixed(1)} s</span><br><span class="why">${kindLabel[s.kind]}</span></span>
            <button class="btn small ghost" data-act="listen" data-seg="${s.id}" title="Écouter autour de la coupe">▶</button>
            <button class="btn small" data-act="restore" data-seg="${s.id}">Garder</button>
          </div>`,
          )
          .join("")}
        ${cuts.length > 400 ? `<p class="hint">… et ${cuts.length - 400} autres.</p>` : ""}
      </div>
    </div>`;
}

function overlayCard(o: Overlay): string {
  const c = o.content;
  const head = `<div class="row"><span class="file">${c.type === "title" ? "Titre" : esc(basename(c.path))} · à ${fmt(sourceToProgram(P(), o.anchor) ?? 0, true)}</span>
      <button class="btn small ghost" data-act="goto" data-ov="${o.id}">Voir</button>
      <button class="btn small danger" data-act="del-ov" data-ov="${o.id}">Supprimer</button></div>`;
  const dur = `<div class="row"><label>Durée</label><input type="number" min="0.5" step="0.5" value="${o.duration}" data-ov="${o.id}" data-field="duration"> s</div>`;
  if (c.type === "title") {
    const opt = (v: TitleStyle, l: string) => `<option value="${v}" ${c.style === v ? "selected" : ""}>${l}</option>`;
    return `<div class="card">${head}
      <textarea data-ov="${o.id}" data-field="text">${esc(c.text)}</textarea>
      <div class="row"><label>Style</label><select data-ov="${o.id}" data-field="style">${opt("lower", "Bandeau bas")}${opt("center", "Centré")}${opt("big", "Gros titre")}</select></div>
      ${dur}</div>`;
  }
  const opt = (v: Position, l: string) => `<option value="${v}" ${c.position === v ? "selected" : ""}>${l}</option>`;
  return `<div class="card">${head}
    <div class="row"><label>Position</label><select data-ov="${o.id}" data-field="position">
      ${opt("top-left", "Haut gauche")}${opt("top-right", "Haut droite")}${opt("bottom-left", "Bas gauche")}${opt("bottom-right", "Bas droite")}${opt("center", "Centre")}</select></div>
    <div class="row"><label>Taille</label><input type="range" min="0.05" max="1" step="0.01" value="${c.scale}" data-ov="${o.id}" data-field="scale"></div>
    ${dur}</div>`;
}

function renderDress() {
  const p = P();
  const sorted = [...p.overlays].sort((a, b) => (sourceToProgram(p, a.anchor) ?? 0) - (sourceToProgram(p, b.anchor) ?? 0));
  const fileRow = (label: string, path: string | null, act: string) => `
    <div class="row"><label>${label}</label><span class="file">${path ? esc(basename(path)) : "—"}</span>
      <button class="btn small" data-act="pick-${act}">${path ? "Changer" : "Choisir"}</button>
      ${path ? `<button class="btn small danger" data-act="clear-${act}">Retirer</button>` : ""}</div>`;
  $("tab-dress").innerHTML = `
    <div class="group">
      <h3>Titres et images</h3>
      <div class="row">
        <button class="btn small" data-act="add-title">＋ Titre ici</button>
        <button class="btn small" data-act="add-image">＋ Image ici</button>
      </div>
      <p class="hint">Ajoutés à la position de lecture, ils restent accrochés au bon moment même si les coupes changent.</p>
      <div class="list">${sorted.map(overlayCard).join("")}</div>
    </div>
    <div class="group">
      <h3>Musique de fond</h3>
      ${fileRow("Fichier", p.music?.path ?? null, "music")}
      ${
        p.music
          ? `<div class="row"><label>Volume</label><input type="range" min="-40" max="0" step="1" value="${p.music.volume_db}" data-music="volume_db"><output>${p.music.volume_db} dB</output></div>
             <div class="row"><label><input type="checkbox" data-music="duck" ${p.music.duck ? "checked" : ""}> Baisser la musique quand je parle</label></div>`
          : ""
      }
    </div>
    <div class="group">
      <h3>Intro et outro</h3>
      ${fileRow("Intro", p.intro, "intro")}
      ${fileRow("Outro", p.outro, "outro")}
    </div>`;
}

const PRESETS = [
  { label: "1080p · 30 i/s", width: 1920, height: 1080, fps: 30 },
  { label: "1080p · 25 i/s", width: 1920, height: 1080, fps: 25 },
  { label: "4K · 30 i/s", width: 3840, height: 2160, fps: 30 },
  { label: "4K · 25 i/s", width: 3840, height: 2160, fps: 25 },
  { label: "Vertical 1080×1920 · 30 i/s (Shorts)", width: 1080, height: 1920, fps: 30 },
];

function renderExport() {
  const e = P().export;
  const cur = PRESETS.findIndex((x) => x.width === e.width && x.height === e.height && x.fps === e.fps);
  const hw = e.video_encoder.includes("videotoolbox");
  $("tab-export").innerHTML = `
    <div class="group">
      <h3>Format</h3>
      <div class="row"><select data-exp="preset">${PRESETS.map((x, i) => `<option value="${i}" ${i === cur ? "selected" : ""}>${x.label}</option>`).join("")}</select></div>
    </div>
    <div class="group">
      <h3>Encodage</h3>
      <div class="row"><select data-exp="encoder">
        <option value="h264_videotoolbox" ${hw ? "selected" : ""}>Puce Apple (rapide)</option>
        <option value="libx264" ${!hw ? "selected" : ""}>Logiciel x264 (plus lent, plus fin)</option>
      </select></div>
      ${
        hw
          ? `<div class="row"><label>Débit</label><select data-exp="bitrate">${["8M", "12M", "16M", "35M", "50M"].map((b) => `<option ${b === e.video_bitrate ? "selected" : ""}>${b}</option>`).join("")}</select></div>
             <p class="hint">12M convient en 1080p, 35–50M en 4K.</p>`
          : `<div class="row"><label>Qualité (CRF, plus bas = mieux)</label><input type="number" min="14" max="28" value="${e.crf}" data-exp="crf"></div>`
      }
    </div>
    <div class="group">
      <h3>Son</h3>
      <p class="hint">Volume normalisé à ${e.loudness_lufs} LUFS, la référence YouTube, et petit fondu de ${e.cut_fade_ms} ms à chaque coupe pour éviter les clics.</p>
    </div>
    <div class="group">
      <h3>Pour YouTube</h3>
      <div class="row">
        <button class="btn small" data-act="write-srt" ${Object.keys(P().transcripts ?? {}).length ? "" : "disabled"}>Sous-titres .srt</button>
        <button class="btn small" data-act="chapters">Chapitres</button>
      </div>
      ${Object.keys(P().transcripts ?? {}).length ? "" : `<p class="hint">Les sous-titres demandent une transcription (onglet Texte).</p>`}
      ${
        state.chapters
          ? `<textarea class="chapters" readonly>${esc(state.chapters.text)}</textarea>
             <div class="row"><button class="btn small" data-act="copy-chapters">Copier</button></div>
             ${state.chapters.warnings.map((w) => `<p class="hint">⚠ ${esc(w)}</p>`).join("")}
             <p class="hint">Les chapitres viennent des titres du montage (onglet Habillage).</p>`
          : ""
      }
    </div>
    <div class="group">
      <h3>Police des titres</h3>
      <div class="row"><span class="file">${e.font ? esc(basename(e.font)) : "Police système"}</span>
        <button class="btn small" data-act="pick-font">Choisir</button>
        ${e.font ? `<button class="btn small danger" data-act="clear-font">Retirer</button>` : ""}</div>
    </div>`;
}

// Délégation d'événements pour les trois panneaux.
const panel = document.querySelector(".panel")!;

panel.addEventListener("input", (ev) => {
  const t = ev.target as HTMLInputElement;
  if (!state.project) return;
  if (t.dataset.search !== undefined) {
    state.search = t.value;
    return renderSentences();
  }
  if (t.dataset.prompt !== undefined) {
    state.prompt = t.value;
    return;
  }
  if (t.dataset.model !== undefined && t.type === "radio") {
    state.model = t.value;
    return renderText();
  }
  if (t.dataset.param) {
    (state.params as any)[t.dataset.param] = Number(t.value);
    (t.nextElementSibling as HTMLOutputElement).textContent = `${Number(t.value).toFixed(2)} s`;
    return;
  }
  if (t.dataset.ov) {
    const o = P().overlays.find((x) => x.id === t.dataset.ov)!;
    const f = t.dataset.field!;
    if (f === "duration") o.duration = Math.max(0.5, Number(t.value) || 0.5);
    else (o.content as any)[f] = f === "scale" ? Number(t.value) : t.value;
    layerKey = "";
    setDirty();
    return;
  }
  if (t.dataset.music && P().music) {
    const m = P().music!;
    if (t.dataset.music === "duck") m.duck = t.checked;
    else {
      m.volume_db = Number(t.value);
      (t.nextElementSibling as HTMLOutputElement).textContent = `${t.value} dB`;
    }
    setDirty();
  }
});

panel.addEventListener("change", (ev) => {
  const t = ev.target as HTMLSelectElement;
  if (state.project && t.dataset.model !== undefined && (t as unknown as HTMLInputElement).type === "radio") {
    state.model = t.value;
    return renderText();
  }
  if (!state.project || !t.dataset.exp) return;
  const e = P().export;
  if (t.dataset.exp === "preset") {
    const { width, height, fps } = PRESETS[Number(t.value)];
    Object.assign(e, { width, height, fps });
  }
  if (t.dataset.exp === "encoder") e.video_encoder = t.value;
  if (t.dataset.exp === "bitrate") e.video_bitrate = t.value;
  if (t.dataset.exp === "crf") e.crf = Number(t.value);
  setDirty();
  refresh();
});

async function pickFile(name: string, extensions: string[]): Promise<string | null> {
  const r = await open({ multiple: false, filters: [{ name, extensions }] });
  return typeof r === "string" ? r : null;
}

panel.addEventListener("click", async (ev) => {
  const row = (ev.target as HTMLElement).closest<HTMLElement>("[data-sent]");
  const b = (ev.target as HTMLElement).closest<HTMLElement>("[data-act]");
  if (row && !b && state.project) {
    const x = state.sentences[Number(row.dataset.sent)];
    if (x.kept === 0) setMode("raw");
    return seek({ source: x.source, time: x.start });
  }
  if (!b || !state.project) return;
  const p = P();
  const act = b.dataset.act!;
  const seg = b.dataset.seg ? p.segments.find((s) => s.id === b.dataset.seg) : undefined;
  const ov = b.dataset.ov ? p.overlays.find((o) => o.id === b.dataset.ov) : undefined;
  switch (act) {
    case "reanalyze":
      return reanalyze();
    case "listen":
      if (seg) {
        setMode("raw");
        await seek({ source: seg.source, time: Math.max(0, seg.start - 1.5) }, true);
      }
      return;
    case "restore":
      if (seg) await markRange(seg.source, seg.start, seg.end, seg.kind, true);
      return;
    case "add-title":
    case "add-image": {
      const anchor = { ...state.cursor };
      if (act === "add-title") {
        p.overlays.push({ id: `ov-${Date.now()}`, anchor, duration: 3.5, content: { type: "title", text: "Mon titre", style: "lower" } });
      } else {
        const path = await pickFile("Images", ["png", "jpg", "jpeg", "webp"]);
        if (!path) return;
        p.overlays.push({ id: `ov-${Date.now()}`, anchor, duration: 4, content: { type: "image", path, position: "top-right", scale: 0.15 } });
      }
      setDirty();
      return refresh();
    }
    case "del-ov":
      p.overlays = p.overlays.filter((o) => o !== ov);
      setDirty();
      return refresh();
    case "goto":
      if (ov) await seek(ov.anchor);
      return;
    case "pick-music": {
      const path = await pickFile("Audio", ["mp3", "m4a", "wav", "aac", "flac", "ogg"]);
      if (path) p.music = { path, volume_db: p.music?.volume_db ?? -20, duck: p.music?.duck ?? true };
      break;
    }
    case "clear-music":
      p.music = null;
      break;
    case "pick-intro":
    case "pick-outro": {
      const path = await pickFile("Vidéos", VIDEO_EXT);
      if (path) p[act === "pick-intro" ? "intro" : "outro"] = path;
      break;
    }
    case "clear-intro":
      p.intro = null;
      break;
    case "clear-outro":
      p.outro = null;
      break;
    case "pick-font": {
      const path = await pickFile("Polices", ["ttf", "otf", "ttc"]);
      if (path) p.export.font = path;
      break;
    }
    case "clear-font":
      p.export.font = null;
      break;
    case "download-model":
      return downloadModel(b.dataset.model!);
    case "transcribe":
      return transcribeAll(b.dataset.all === "1");
    case "find-retakes":
      state.retakes = await invoke<RetakeOut[]>("detect_retakes", { project: p });
      if (!state.retakes.length) toast("Aucune prise ratée repérée dans le montage.");
      return renderText();
    case "cut-retakes": {
      const n = state.retakes?.length ?? 0;
      state.project = await invoke<Project>("cut_retakes", { project: p, retakes: state.retakes ?? [] });
      state.retakes = [];
      toast(`${n} prise${n > 1 ? "s" : ""} ratée${n > 1 ? "s" : ""} coupée${n > 1 ? "s" : ""}.`);
      break;
    }
    case "sent-cut":
    case "sent-keep": {
      const row = b.closest<HTMLElement>("[data-sent]")!;
      const x = state.sentences[Number(row.dataset.sent)];
      ev.stopPropagation();
      return markRange(x.source, x.start, x.end, act === "sent-cut" ? "manual" : "speech", act === "sent-keep");
    }
    case "write-srt": {
      const path = await save({ defaultPath: "sous-titres.srt", filters: [{ name: "Sous-titres", extensions: ["srt"] }] });
      if (!path) return;
      try {
        const n = await invoke<number>("write_srt", { project: p, path });
        toast(`${n} sous-titres écrits. À importer dans YouTube Studio > Sous-titres.`);
      } catch (e) {
        toast(String(e), true);
      }
      return;
    }
    case "chapters":
      state.chapters = await invoke<Chapters>("chapters", { project: p });
      return renderExport();
    case "copy-chapters":
      await navigator.clipboard.writeText(state.chapters?.text ?? "").catch(() => {});
      return toast("Chapitres copiés : colle-les dans la description YouTube.");
    default:
      return;
  }
  setDirty();
  refresh();
});

document.querySelectorAll<HTMLButtonElement>(".tabs button").forEach((b) =>
  b.addEventListener("click", () => {
    document.querySelectorAll(".tabs button").forEach((x) => x.classList.toggle("on", x === b));
    for (const t of ["cuts", "text", "dress", "export"]) $(`tab-${t}`).hidden = t !== b.dataset.tab;
  }),
);

function setMode(m: "program" | "raw") {
  state.mode = m;
  $("mode-program").classList.toggle("on", m === "program");
  $("mode-raw").classList.toggle("on", m === "raw");
  if (state.project) updateClock();
}
$("mode-program").addEventListener("click", () => setMode("program"));
$("mode-raw").addEventListener("click", () => setMode("raw"));

// ---------------------------------------------------------------- texte

async function renderText() {
  const p = state.project;
  const el = $("tab-text");
  if (!p) return;
  try {
    [state.models, state.sentences] = await Promise.all([
      invoke<ModelInfo[]>("models_status"),
      invoke<SentenceView[]>("transcript", { project: p }),
    ]);
  } catch (e) {
    el.innerHTML = `<p class="hint">${esc(String(e))}</p>`;
    return;
  }
  const missing = p.sources.filter((s) => !p.transcripts?.[s.id]);
  const has = missing.length < p.sources.length;
  const model = state.models.find((m) => m.id === state.model) ?? state.models[0];
  const setup = missing.length
    ? `<div class="group">
        <h3>Transcription</h3>
        <p class="hint">Tout se passe sur le Mac, rien n'est envoyé en ligne. Elle sert à repérer les prises ratées, à faire les sous-titres et à laisser Claude retrouver un passage.</p>
        <div class="list">
          ${state.models
            .map(
              (m) => `<label class="card model">
                <span><input type="radio" name="model" value="${m.id}" data-model ${m.id === model.id ? "checked" : ""}> ${esc(m.label)}</span>
                <span class="meta">${m.installed ? "installé" : `${m.size_mb} Mo`}</span>
              </label>`,
            )
            .join("")}
        </div>
        <div class="row"><input type="text" data-prompt placeholder="Mots à bien reconnaître : viscose, Atelier Brunette…" value="${esc(state.prompt)}" style="flex:1"></div>
        <div class="row">
          ${
            model.installed
              ? `<button class="btn primary small" data-act="transcribe">Transcrire ${missing.length > 1 ? `les ${missing.length} rushes` : "le rush"}</button>`
              : `<button class="btn small" data-act="download-model" data-model="${model.id}">Télécharger le modèle (${model.size_mb} Mo)</button>`
          }
        </div>
      </div>`
    : "";
  if (!has) {
    el.innerHTML = setup;
    return;
  }
  const retakes = state.retakes;
  el.innerHTML = `${setup}
    <div class="group">
      <h3>Prises ratées</h3>
      <div class="row">
        <button class="btn small" data-act="find-retakes">Chercher</button>
        ${retakes?.length ? `<button class="btn small primary" data-act="cut-retakes">Couper les ${retakes.length}</button>` : ""}
        <span class="muted">${retakes === null ? "" : retakes.length ? `${retakes.length} repérée${retakes.length > 1 ? "s" : ""}, en orange` : "aucune dans le montage"}</span>
      </div>
      <p class="hint">Phrases recommencées un peu plus loin (on garde la dernière) et « je recommence », « non attends »…</p>
    </div>
    <div class="group">
      <div class="sent-tools">
        <input type="search" data-search placeholder="Chercher dans le texte" value="${esc(state.search)}">
        ${has && !missing.length ? `<button class="btn small ghost" data-act="transcribe" data-all="1" title="Refaire la transcription">Refaire</button>` : ""}
      </div>
      <p class="hint">Clic sur une phrase : y aller. Survol : couper ou garder la phrase.</p>
      <div id="sentences" class="sentences"></div>
    </div>`;
  renderSentences();
}

const norm = (x: string) => x.normalize("NFD").replace(/[̀-ͯ]/g, "").toLowerCase();

function renderSentences() {
  const box = document.getElementById("sentences");
  if (!box) return;
  const q = norm(state.search.trim());
  const retake = (x: SentenceView) => state.retakes?.find((r) => r.source === x.source && Math.abs(r.start - x.start) < 0.01);
  box.innerHTML = state.sentences
    .map((x, i) => {
      if (q && !norm(x.text).includes(q)) return "";
      const r = retake(x);
      const cls = [x.kept === 0 ? "cut" : x.kept < 0.9 ? "partial" : "", r ? "retake" : ""].join(" ");
      return `<div class="sent ${cls}" data-sent="${i}">
        <span class="t">${x.program === null ? "coupé" : fmt(x.program)}</span>
        <span class="tx">${esc(x.text)}${r ? `<span class="why">${esc(r.reason)}</span>` : ""}</span>
        <span class="acts">${
          x.kept > 0
            ? `<button class="btn small ghost" data-act="sent-cut" title="Couper la phrase">✕</button>`
            : `<button class="btn small ghost" data-act="sent-keep" title="Garder la phrase">↺</button>`
        }</span>
      </div>`;
    })
    .join("");
  highlightSentence(true);
}

let nowSentence = -1;
function highlightSentence(force = false) {
  const c = state.cursor;
  const i = state.sentences.findIndex((x) => x.source === c.source && c.time >= x.start - 0.1 && c.time < x.end + 0.3);
  if (i === nowSentence && !force) return;
  nowSentence = i;
  const box = document.getElementById("sentences");
  if (!box) return;
  box.querySelector(".sent.now")?.classList.remove("now");
  const row = box.querySelector<HTMLElement>(`[data-sent="${i}"]`);
  if (row) {
    row.classList.add("now");
    if (!video.paused && !$("tab-text").hidden) row.scrollIntoView({ block: "nearest" });
  }
}

async function downloadModel(id: string) {
  busy("Téléchargement du modèle… 0 %");
  const un = await listen<number>("model-progress", (e) => busy(`Téléchargement du modèle… ${Math.round(e.payload * 100)} %`));
  try {
    await invoke("download_model", { id });
    toast("Modèle installé.");
  } catch (e) {
    toast(String(e), true);
  } finally {
    un();
    busy(null);
    renderText();
  }
}

async function transcribeAll(all: boolean) {
  const p = P();
  const sources = all ? p.sources.map((s) => s.id) : null;
  const started = Date.now();
  busy("Transcription…", () => invoke("cancel_transcription"));
  const un = await listen<{ source: string; index: number; total: number; progress: number }>("transcribe-progress", (e) => {
    const { index, total, progress } = e.payload;
    const el = (Date.now() - started) / 1000;
    const done = (index + progress) / total;
    const left = done > 0.03 ? el / done - el : 0;
    busy(
      `Transcription${total > 1 ? ` du rush ${index + 1}/${total}` : ""}… ${Math.round(progress * 100)} %${left ? ` · encore ~${fmt(left)}` : ""}`,
      () => invoke("cancel_transcription"),
    );
  });
  try {
    const t = await invoke<Record<string, any>>("transcribe_rushes", { project: p, model: state.model, sources, prompt: state.prompt });
    // La transcription a pu prendre du temps : on fusionne dans le projet actuel.
    P().transcripts = { ...(P().transcripts ?? {}), ...t };
    state.retakes = null;
    setDirty();
    toast("Transcription terminée. Cherche les prises ratées, ou parcours le texte.");
  } catch (e) {
    toast(String(e), true);
  } finally {
    un();
    busy(null);
    refresh();
  }
}

// ---------------------------------------------------------------- synchro avec Claude

async function reloadFromDisk() {
  if (!state.projectPath) return;
  const keepCursor = { ...state.cursor };
  const keepView = { ...state.view };
  try {
    const l = await invoke<Loaded>("load_project", { path: state.projectPath, known: Object.keys(state.waves) });
    state.project = l.project;
    Object.assign(state.waves, l.waveforms);
    state.mtime = await invoke<number | null>("project_mtime", { path: state.projectPath });
    state.retakes = null;
    setDirty(false);
    if (P().sources.some((s) => s.id === keepCursor.source)) state.cursor = keepCursor;
    state.view = keepView;
    refresh();
  } catch (e) {
    toast(String(e), true);
  }
}

setInterval(async () => {
  if (!state.projectPath || !state.mtime || !$("banner").hidden) return;
  const m = await invoke<number | null>("project_mtime", { path: state.projectPath }).catch(() => null);
  if (!m || m <= state.mtime) return;
  if (state.dirty) {
    clearTimeout(saveTimer); // ne pas écraser la version de Claude sans demander
    $("banner").hidden = false;
  } else {
    await reloadFromDisk();
    toast("Projet mis à jour depuis Claude.");
  }
}, 1500);

$("banner-reload").addEventListener("click", async () => {
  $("banner").hidden = true;
  await reloadFromDisk();
});
$("banner-ignore").addEventListener("click", async () => {
  $("banner").hidden = true;
  await saveProject(true);
});

// ---------------------------------------------------------------- fichiers

async function confirmDiscard(): Promise<boolean> {
  return !state.dirty || (await confirm("Les modifications non enregistrées seront perdues. Continuer ?", { title: "Derush", kind: "warning" }));
}

async function setLoaded(l: Loaded, path: string | null) {
  video.pause();
  state.project = l.project;
  state.waves = l.waveforms;
  state.projectPath = path;
  state.loaded = null;
  state.selection = null;
  const first = l.project.segments.find((s) => s.keep) ?? l.project.segments[0];
  state.cursor = { source: first?.source ?? l.project.sources[0].id, time: first?.start ?? 0 };
  state.view.start = 0;
  fitView();
  state.retakes = null;
  state.chapters = null;
  setDirty(path === null);
  state.mtime = path ? await invoke<number | null>("project_mtime", { path }) : null;
  refresh();
  busy(null);
  await seek(state.cursor, false);
}

async function importRushes(paths: string[]) {
  if (!paths.length) return;
  try {
    if (state.project) {
      const add = await confirm(`Ajouter ${paths.length > 1 ? "ces rushes" : "ce rush"} à la fin du montage ?\n(Non : nouveau projet)`, {
        title: "Derush",
        okLabel: "Ajouter",
        cancelLabel: "Nouveau projet",
      });
      if (add) {
        busy("Analyse du son…");
        const l = await invoke<Loaded>("append_rushes", { project: P(), paths, params: state.params });
        state.project = l.project;
        Object.assign(state.waves, l.waveforms);
        fitView();
        setDirty();
        return refresh();
      }
      if (!(await confirmDiscard())) return;
    }
    busy(paths.length > 1 ? `Analyse de ${paths.length} rushes…` : "Analyse du son…");
    await setLoaded(await invoke<Loaded>("import_rushes", { paths, params: state.params }), null);
    const cuts = P().segments.filter((s) => !s.keep).length;
    toast(`${cuts} blancs repérés et coupés. Vérifie en lecture, double-clic pour changer d'avis.`);
  } catch (e) {
    toast(String(e), true);
  } finally {
    busy(null);
  }
}

async function reanalyze() {
  if (!state.project) return;
  const manual = P().segments.some((s) => s.kind === "manual" || (s.keep && s.kind !== "speech"));
  if (manual && !(await confirm("Les coupes faites à la main seront recalculées. Continuer ?", { title: "Derush", kind: "warning" }))) return;
  const keep = P();
  busy("Nouvelle analyse…");
  try {
    const l = await invoke<Loaded>("import_rushes", { paths: keep.sources.map((s) => s.path), params: state.params });
    // On garde l'habillage et les réglages d'export : les identifiants de rushes sont stables.
    Object.assign(l.project, {
      overlays: keep.overlays, music: keep.music, intro: keep.intro, outro: keep.outro, export: keep.export, transcripts: keep.transcripts,
    });
    state.project = l.project;
    state.waves = l.waveforms;
    setDirty();
    refresh();
  } catch (e) {
    toast(String(e), true);
  } finally {
    busy(null);
  }
}

$("btn-import").addEventListener("click", async () => {
  const r = await open({ multiple: true, filters: [{ name: "Vidéos", extensions: VIDEO_EXT }] });
  if (r) importRushes(Array.isArray(r) ? r : [r]);
});

$("btn-open").addEventListener("click", async () => {
  if (!(await confirmDiscard())) return;
  const path = await pickFile("Projet Derush", ["json"]);
  if (!path) return;
  busy("Ouverture…");
  try {
    await setLoaded(await invoke<Loaded>("load_project", { path }), path);
  } catch (e) {
    toast(String(e), true);
  } finally {
    busy(null);
  }
});

async function saveProject(auto = false) {
  if (!state.project) return;
  let path = state.projectPath;
  if (!path) {
    if (auto) return;
    path = await save({ defaultPath: "projet.derush.json", filters: [{ name: "Projet Derush", extensions: ["json"] }] });
    if (!path) return;
  }
  try {
    await invoke("save_project", { path, project: P() });
    const first = !state.projectPath;
    state.projectPath = path;
    state.mtime = await invoke<number | null>("project_mtime", { path });
    setDirty(false);
    if (!auto) toast(first ? "Projet enregistré. Il s'enregistre désormais tout seul, et Claude peut y accéder." : "Projet enregistré.");
  } catch (e) {
    toast(String(e), true);
  }
}
$("btn-save").addEventListener("click", () => saveProject());

$("btn-export").addEventListener("click", async () => {
  if (!state.project) return;
  const output = await save({ defaultPath: "video.mp4", filters: [{ name: "Vidéo MP4", extensions: ["mp4"] }] });
  if (!output) return;
  video.pause();
  const dlg = $<HTMLDialogElement>("export-dialog");
  const bar = $<HTMLProgressElement>("export-progress");
  $("export-title").textContent = "Export en cours…";
  $("export-detail").textContent = basename(output);
  $("export-close").hidden = true;
  bar.value = 0;
  dlg.showModal();
  const started = Date.now();
  const unlisten = await listen<number>("export-progress", (e) => {
    bar.value = e.payload;
    const el = (Date.now() - started) / 1000;
    const left = e.payload > 0.02 ? el / e.payload - el : 0;
    $("export-detail").textContent = `${Math.round(e.payload * 100)} %${left ? ` · encore ~${fmt(left)}` : ""}`;
  });
  try {
    const warnings = await invoke<string[]>("export_video", { project: P(), output });
    bar.value = 1;
    $("export-title").textContent = "Vidéo prête";
    $("export-detail").textContent = [`${basename(output)} · ${fmt((Date.now() - started) / 1000)}`, ...warnings].join("\n");
  } catch (e) {
    $("export-title").textContent = "L'export a échoué";
    $("export-detail").textContent = String(e);
  } finally {
    unlisten();
    $("export-close").hidden = false;
  }
});
$("export-close").addEventListener("click", () => $<HTMLDialogElement>("export-dialog").close());

$("btn-play").addEventListener("click", togglePlay);
$("btn-cut-sel").addEventListener("click", () => markSelection(false));
$("btn-keep-sel").addEventListener("click", () => markSelection(true));

window.addEventListener("keydown", (e) => {
  const tag = (e.target as HTMLElement).tagName;
  if (["INPUT", "TEXTAREA", "SELECT"].includes(tag)) return;
  if ((e.metaKey || e.ctrlKey) && e.key === "s") {
    e.preventDefault();
    return saveProject();
  }
  if (!state.project) return;
  if (e.key === " ") {
    e.preventDefault();
    togglePlay();
  } else if (e.key === "c") markSelection(false);
  else if (e.key === "g") markSelection(true);
  else if (e.key === "Escape") {
    state.selection = null;
    updateSelection();
  } else if (e.key === "ArrowLeft" || e.key === "ArrowRight") {
    const d = (e.key === "ArrowLeft" ? -1 : 1) * (e.shiftKey ? 5 : 1);
    const s = src(state.cursor.source);
    seek({ source: s.id, time: Math.min(Math.max(0, state.cursor.time + d), s.duration) });
  }
});

// Glisser-déposer de fichiers sur la fenêtre.
getCurrentWebview().onDragDropEvent((ev) => {
  const p = ev.payload;
  $("empty").classList.toggle("drag", p.type === "over" || p.type === "enter");
  if (p.type === "drop") {
    $("empty").classList.remove("drag");
    const vids = p.paths.filter((x) => VIDEO_EXT.includes(x.split(".").pop()?.toLowerCase() ?? ""));
    if (vids.length) importRushes(vids);
    else toast("Glisse des fichiers vidéo (mp4, mov…).", true);
  }
});

window.addEventListener("resize", () => {
  if (state.project) {
    fitView();
    layerKey = "";
  }
  draw();
});

readColors();
refresh();
draw();
requestAnimationFrame(tick);

invoke<string[]>("startup_paths").then(async (paths) => {
  const proj = paths.find((x) => x.endsWith(".json"));
  if (proj) {
    busy("Ouverture…");
    try {
      await setLoaded(await invoke<Loaded>("load_project", { path: proj }), proj);
    } catch (e) {
      toast(String(e), true);
    } finally {
      busy(null);
    }
  } else if (paths.length) importRushes(paths);
});

// Exposé pour le débogage dans l'inspecteur.
(window as any).derush = { state, src: (id: string): Source => src(id) };
