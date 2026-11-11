// Where the checks write. Defaults stay inside web/jobview (git-ignored); set JOBVIEW_CHECKS and
// JOBVIEW_SHOTS to write elsewhere (for example to keep checks/ and shots/ folders elsewhere).
import { mkdirSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

const dirUrl = (env, fallback) => {
  const u = process.env[env] ? pathToFileURL(process.env[env].replace(/[\\/]?$/, '/')) : new URL(fallback, import.meta.url);
  mkdirSync(u, { recursive: true });
  return u;
};
export const CHECKS = dirUrl('JOBVIEW_CHECKS', '../.checks/');
export const SHOTS = dirUrl('JOBVIEW_SHOTS', '../.shots/');
