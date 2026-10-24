// Miroir des types Rust de derush-core (model.rs).

export type SegmentKind = "speech" | "silence" | "retake" | "manual";
export type TitleStyle = "lower" | "center" | "big";
export type Position = "top-left" | "top-right" | "bottom-left" | "bottom-right" | "center";

export interface Source {
  id: string;
  path: string;
  duration: number;
  width?: number | null;
  height?: number | null;
  fps?: number | null;
}

export interface Segment {
  id: string;
  source: string;
  start: number;
  end: number;
  kind: SegmentKind;
  keep: boolean;
}

export interface Anchor {
  source: string;
  time: number;
}

export type OverlayContent =
  | { type: "title"; text: string; style: TitleStyle }
  | { type: "image"; path: string; position: Position; scale: number };

export interface Overlay {
  id: string;
  anchor: Anchor;
  duration: number;
  content: OverlayContent;
}

export interface Music {
  path: string;
  volume_db: number;
  duck: boolean;
}

export interface ExportSettings {
  width: number;
  height: number;
  fps: number;
  video_encoder: string;
  video_bitrate: string;
  crf: number;
  loudness_lufs: number;
  cut_fade_ms: number;
  font: string | null;
}

export interface Project {
  version: number;
  sources: Source[];
  segments: Segment[];
  overlays: Overlay[];
  music: Music | null;
  intro: string | null;
  outro: string | null;
  export: ExportSettings;
  transcripts?: Record<string, Word[]>;
}

export interface Word {
  text: string;
  start: number;
  end: number;
}

export interface SentenceView {
  source: string;
  start: number;
  end: number;
  text: string;
  program: number | null;
  kept: number;
}

export interface RetakeOut {
  source: string;
  start: number;
  end: number;
  text: string;
  reason: string;
}

export interface ModelInfo {
  id: string;
  label: string;
  file: string;
  size_mb: number;
  installed: boolean;
  path: string;
}

export interface Chapters {
  text: string;
  warnings: string[];
}

export interface Waveform {
  per_second: number;
  db: number[];
}

export interface Loaded {
  project: Project;
  waveforms: Record<string, Waveform>;
}

export interface SilenceParams {
  min_silence: number;
  padding: number;
  threshold_db?: number | null;
}

const order = (p: Project, id: string) => p.sources.findIndex((s) => s.id === id);

/** Port de Project::source_to_program (model.rs). */
export function sourceToProgram(p: Project, a: Anchor): number | null {
  const target = order(p, a.source);
  let acc = 0;
  for (const s of p.segments) {
    if (!s.keep) continue;
    const idx = order(p, s.source);
    if (idx > target || (idx === target && s.start > a.time)) return acc;
    if (idx === target && a.time < s.end) return acc + (a.time - s.start);
    acc += s.end - s.start;
  }
  return null;
}

export const programDuration = (p: Project) =>
  p.segments.filter((s) => s.keep).reduce((t, s) => t + s.end - s.start, 0);
