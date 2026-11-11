// Command palette (Ctrl/Cmd K), ported from design v2.1. It is used many times a day, so it opens with no
// animation. The command list comes from the app, which knows the state.

import { esc } from './format.js';

export interface PaletteCommand { label: string; hint?: string; key?: string; cls?: string; run(): void }

export function initPalette(opts: { booted(): boolean; commands(q: string): PaletteCommand[] }) {
  const pal = document.getElementById('pal') as HTMLDialogElement;
  const q = document.getElementById('pal-q') as HTMLInputElement;
  const list = document.getElementById('pal-list')!;
  let items: PaletteCommand[] = [], active = 0, ret: Element | null = null;
  if (/Mac|iPhone|iPad/.test(navigator.platform || '')) document.getElementById('pal-kbd')!.textContent = '⌘K';

  function render() {
    items = opts.commands(q.value); active = Math.min(active, Math.max(0, items.length - 1));
    list.innerHTML = items.length
      ? items.map((c, i) => `<li role="option" id="pal-o${i}" aria-selected="${i === active}" class="${c.cls ?? ''}" data-i="${i}"><span>${esc(c.label)}</span>${c.key ? `<span class="hint"><kbd>${esc(c.key)}</kbd></span>` : c.hint ? `<span class="hint">${esc(c.hint)}</span>` : ''}</li>`).join('')
      : `<li class="empty" role="option" aria-disabled="true" aria-selected="false">${opts.booted() ? 'No command matches. Type a step number to jump to it.' : 'Still connecting: nothing to control yet.'}</li>`;
    if (items.length) q.setAttribute('aria-activedescendant', 'pal-o' + active); else q.removeAttribute('aria-activedescendant');
    list.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
  }
  function open() { if (pal.open) return; ret = document.activeElement; q.value = ''; active = 0; render(); pal.showModal(); q.focus(); }
  function close(restore = true) { if (!pal.open) return; pal.close(); if (restore && ret instanceof HTMLElement && ret.isConnected) ret.focus(); }
  function run(i: number) { const c = items[i]; if (!c) return; close(true); c.run(); }

  document.getElementById('pal-open')!.addEventListener('click', open);
  q.addEventListener('input', () => { active = 0; render(); });
  q.addEventListener('keydown', e => {
    const n = Math.max(1, items.length);
    if (e.key === 'ArrowDown') { e.preventDefault(); active = (active + 1) % n; render(); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); active = (active - 1 + n) % n; render(); }
    else if (e.key === 'Home') { e.preventDefault(); active = 0; render(); }
    else if (e.key === 'End') { e.preventDefault(); active = Math.max(0, items.length - 1); render(); }
    else if (e.key === 'Enter') { e.preventDefault(); run(active); }
  });
  list.addEventListener('click', e => { const li = (e.target as HTMLElement).closest<HTMLElement>('li[data-i]'); if (li) run(+li.dataset.i!); });
  pal.addEventListener('cancel', e => { e.preventDefault(); close(true); });
  pal.addEventListener('click', e => { if (e.target === pal) close(true); }); // the backdrop

  return { toggle() { if (pal.open) close(true); else open(); }, isOpen: () => pal.open };
}
