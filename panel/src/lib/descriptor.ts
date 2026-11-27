// A provider's descriptor (docs/mgmt.md, mgmtwire::descriptor), as the
// panel builds its pages from it: what each method is and takes, the
// resources it lists and what may be done to each row, how each field is
// shown. Nothing here is a provider's own: a provider the panel has never
// heard of -- a storage pool's, say -- gets its pages the same way.

import { Locale, Text, text } from './i18n';

/** How an enum's value says a thing is (the format's `tones`, mgmtwire's
 * TONES): a client colours it so. */
export type Tone = 'good' | 'warn' | 'alarm' | 'bad' | 'idle';

export const TONE_COLOR: Record<Tone, string> = {
  good: 'var(--tone-good)', warn: 'var(--tone-warn)', alarm: 'var(--tone-alarm)', bad: 'var(--tone-bad)',
  idle: 'var(--tone-idle)',
};

/** The tones from the worst: what two of them make together is the worse. */
const TONE_RANK: Tone[] = ['bad', 'alarm', 'warn', 'good', 'idle'];

export function worst(a: Tone, b: Tone): Tone {
  return TONE_RANK.indexOf(a) <= TONE_RANK.indexOf(b) ? a : b;
}

/** Where a parameter's values come from (the format's `choices`): a read of
 * a provider's on the same host -- `items` the list in its answer, `key`
 * each item's value. `require` keeps only items whose fields have those exact
 * values; `show` adds fields to the option's label. A form offers them as a
 * list; a value not among them is the host's to refuse, and a read that fails
 * leaves the field typed. */
export interface Choices {
  provider: string;
  method: string;
  items: string;
  key: string;
  require?: Record<string, string | number | boolean | null>;
  show?: string[];
}

export interface Field {
  type: string;
  title: Text;
  help?: Text;
  unit?: string;
  min?: number;
  max?: number;
  values?: Record<string, Text>;
  tones?: Record<string, Tone>;
  required?: boolean;
  default?: string | number | boolean | null;
  of?: string;
  choices?: Choices;
}

export interface Method {
  kind: 'read' | 'action' | 'job';
  title: Text;
  help?: Text;
  confirm?: Text;
  /** The parameter whose value the user types again before the call: a
   * pool's name before it is destroyed. */
  confirm_typed?: string;
  params?: Record<string, Field>;
  result?: string;
}

export interface Resource {
  title: Text;
  help?: Text;
  list: string;
  get?: string;
  items: string;
  key: string;
  fields: Record<string, Field>;
  columns: string[];
  actions?: Array<{ method: string; params?: Record<string, string | number | boolean> }>;
  parent?: string;
}

export interface Descriptor {
  schema: number;
  provider: string;
  title: Text;
  help?: Text;
  ui?: { section?: string; order?: number; icon?: string };
  methods: Record<string, Method>;
  resources?: Record<string, Resource>;
  metrics?: Record<string, { title: Text; method: string; items?: string; key?: string; field: string;
    kind: 'gauge' | 'rate'; unit: string }>;
}

const KIB = 1024;
const UNITS = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];

export function bytes(n: number): string {
  let v = n;
  let u = 0;
  while (v >= KIB && u < UNITS.length - 1) {
    v /= KIB;
    u += 1;
  }
  return (u === 0 ? String(v) : v.toFixed(v < 10 ? 1 : 0)) + ' ' + UNITS[u];
}

export function duration(ms: number, locale: Locale): string {
  const s = Math.floor(ms / 1000);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ru = locale === 'ru';
  if (d) return `${d} ${ru ? 'д' : 'd'} ${h} ${ru ? 'ч' : 'h'}`;
  if (h) return `${h} ${ru ? 'ч' : 'h'} ${m} ${ru ? 'мин' : 'min'}`;
  if (m) return `${m} ${ru ? 'мин' : 'min'} ${s % 60} ${ru ? 'с' : 's'}`;
  return `${s} ${ru ? 'с' : 's'}`;
}

/** A field's value as a cell shows it: by the field's type. */
export function show(f: Field | undefined, v: unknown, locale: Locale): string {
  if (v === null || v === undefined) {
    return '—';
  }
  switch (f?.type) {
    case 'bytes':
      return typeof v === 'number' ? bytes(v) : String(v);
    case 'duration_ns':
      return typeof v === 'number' ? duration(v / 1e6, locale) : String(v);
    case 'duration_ms':
      return typeof v === 'number' ? duration(v, locale) : String(v);
    case 'time_s':
      return typeof v === 'number' ? new Date(v * 1000).toLocaleString(locale) : String(v);
    case 'bool':
      return v ? (locale === 'ru' ? 'да' : 'yes') : (locale === 'ru' ? 'нет' : 'no');
    case 'enum':
      return text(f.values?.[String(v)], locale) || String(v);
    case 'int':
      return typeof v === 'number' ? v.toLocaleString(locale) + (f.unit ? ' ' + f.unit : '') : String(v);
    case 'list':
      return Array.isArray(v) ? v.map(String).join(', ') : String(v);
    default:
      return typeof v === 'object' ? JSON.stringify(v) : String(v);
  }
}

/** The tone a field's value is in, where its descriptor says one. */
export function toneOf(f: Field | undefined, v: unknown): Tone | undefined {
  return f?.type === 'enum' && typeof v === 'string' ? f.tones?.[v] : undefined;
}

/** The methods a section of the sidebar is made of: a provider's
 * `ui.section`, or its own name. */
export function sectionOf(d: Descriptor): string {
  return d.ui?.section || d.provider;
}
