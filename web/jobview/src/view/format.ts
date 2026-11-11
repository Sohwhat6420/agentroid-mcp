// Formatting shared by every region. Copy rules (DESIGN §3): a no-break space joins every number
// to its unit, temperatures and step durations have 1 decimal, times are HH:MM:SS SGT.

export const NB = ' ';
const SGT = 8 * 3600_000;

export const clock = (ms: number) => new Date(ms + SGT).toISOString().slice(11, 19);
export const hm = (ms: number) => new Date(ms + SGT).toISOString().slice(11, 16);

export function dur(s: number): string {
  s = Math.max(0, Math.round(s));
  if (s < 60) return s + NB + 's';
  if (s < 3600) return Math.floor(s / 60) + NB + 'min ' + String(s % 60).padStart(2, '0') + NB + 's';
  return Math.floor(s / 3600) + NB + 'h ' + String(Math.floor((s % 3600) / 60)).padStart(2, '0') + NB + 'min';
}
/** "12 min" without the seconds, for figures */
export const durMin = (s: number) => dur(s).replace(/ \d+ s$/, '');
export const durShort = (s: number) => s.toFixed(1) + NB + 's';
export const c1 = (v: number) => v.toFixed(1);
export const kb = (bytes: number) => Math.round(bytes / 1024) + NB + 'KB';

export const esc = (s: unknown) => String(s).replace(/[&<>"']/g, ch => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[ch]!));

export const glyph = (name: string, label?: string) =>
  `<svg class="g" ${label ? `role="img" aria-label="${esc(label)}"` : 'aria-hidden="true"'}><use href="#g-${name}"/></svg>`;

/** Selectors break after : _ / so they wrap cleanly (CRITIQUE P3: "shutter_b utton"). */
export const breakable = (s: string) => esc(s).replace(/([:_/])/g, '$1<wbr>');

/** "Shutter to 1/250 s: ok" -> "Shutter to 1/250 s" (the engine's step message ends with its verdict). */
export const stepLabel = (msg: string) => msg.replace(/:\s*ok$/i, '').trim();

/** Write only when the markup changed, so a 1 s tick never steals focus from a button inside. */
export function setHTML(el: HTMLElement, html: string): void {
  const e = el as HTMLElement & { _html?: string };
  if (e._html !== html) { e._html = html; el.innerHTML = html; }
}

export const plural = (n: number, one: string, many = one + 's') => `${n}${NB}${n === 1 ? one : many}`;
