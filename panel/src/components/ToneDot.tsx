import { Field, show, Tone, TONE_COLOR, toneOf } from '@/lib/descriptor';
import type { Locale } from '@/lib/i18n';

/** A tone's dot: decoration beside the word that says the same. */
export function ToneDot({ tone }: { tone: Tone }) {
  return (
    <span aria-hidden data-tone={tone} className="inline-block h-2.5 w-2.5 shrink-0 rounded-full"
      style={{ background: TONE_COLOR[tone] }} />
  );
}

/** A field's value as a table shows it: with its tone's dot, where its
 * descriptor gives its values tones. */
export function Shown({ f, v, locale }: { f: Field | undefined; v: unknown; locale: Locale }) {
  const tone = toneOf(f, v);
  if (!tone) return <>{show(f, v, locale)}</>;
  return (
    <span className="inline-flex items-center gap-1.5">
      <ToneDot tone={tone} />
      {show(f, v, locale)}
    </span>
  );
}
