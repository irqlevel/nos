// The panel's own version, as its footer says it: what the environment says
// (`PANEL_VERSION`, the tests'), else the first line of the bundle's VERSION
// beside the app -- /opt/nos-panel/VERSION, the app run from
// /opt/nos-panel/app (nos-panel.service) -- else "dev". Read once.

import fs from 'node:fs';
import path from 'node:path';

/** The bundle's VERSION's first line: `nos-panel <version>`. */
const LINE = /^nos-panel\s+(\S+)$/;

let known: string | null = null;

export function panelVersion(): string {
  if (known === null) {
    known = process.env.PANEL_VERSION || fromBundle() || 'dev';
  }
  return known;
}

function fromBundle(): string | null {
  try {
    const text = fs.readFileSync(/*turbopackIgnore: true*/ path.join(process.cwd(), '..', 'VERSION'), 'utf8');
    return LINE.exec((text.split('\n')[0] ?? '').trim())?.[1] ?? null;
  } catch {
    return null;
  }
}
