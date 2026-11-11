// A small JSON Schema (draft 2020-12 subset) checker, written for the job view so the page can
// validate every payload at the edge with no new dependency. It reads the repo's own schema files
// (schemas/job-view.json, schemas/progress.json and the two proposed sidecars in contract/), so
// the schema stays the one source of truth.
//
// Supported keywords are exactly the ones those files use. Any other keyword makes compile()
// throw, so a schema change that this checker cannot honour fails loudly in the unit tests instead
// of being skipped silently.

export type Json = null | boolean | number | string | Json[] | { [k: string]: Json };
export type Schema = { [k: string]: unknown };

export interface Issue {
  /** JSON Pointer-ish path to the offending value, e.g. `/changes_in_force/0/before`. */
  path: string;
  msg: string;
}

const ANNOTATIONS = new Set(['$schema', '$id', 'title', 'description', '$comment', '$defs', 'examples']);
const KEYWORDS = new Set([
  'type', 'required', 'properties', 'additionalProperties', 'enum', 'const', 'minimum', 'maximum',
  'minLength', 'maxLength', 'pattern', 'items', 'oneOf', 'anyOf', '$ref', 'minItems', 'maxItems',
]);

export class SchemaUnsupported extends Error {}

function typeOf(v: unknown): string {
  if (v === null) return 'null';
  if (Array.isArray(v)) return 'array';
  if (typeof v === 'number') return Number.isInteger(v) ? 'integer' : 'number';
  return typeof v;
}

function typeMatches(want: string, v: unknown): boolean {
  const got = typeOf(v);
  if (want === 'number') return got === 'number' || got === 'integer';
  return got === want;
}

function equal(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== typeof b || a === null || b === null || typeof a !== 'object') return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const ka = Object.keys(a as object), kb = Object.keys(b as object);
  if (ka.length !== kb.length) return false;
  return ka.every(k => equal((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k]));
}

const short = (v: unknown) => {
  const s = JSON.stringify(v);
  return s === undefined ? String(v) : s.length > 40 ? s.slice(0, 37) + '...' : s;
};

/** Walk the schema once and refuse anything this checker does not implement. */
function audit(s: unknown, where: string): void {
  if (typeof s === 'boolean') return;
  if (!s || typeof s !== 'object' || Array.isArray(s)) throw new SchemaUnsupported(`${where}: a schema must be an object`);
  for (const [k, v] of Object.entries(s as Schema)) {
    if (k === '$defs') { for (const [dk, dv] of Object.entries(v as Schema)) audit(dv, `${where}/$defs/${dk}`); continue; }
    if (ANNOTATIONS.has(k)) continue;
    if (!KEYWORDS.has(k)) throw new SchemaUnsupported(`${where}: keyword "${k}" is not supported by the job view's checker`);
    if (k === 'properties') for (const [pk, pv] of Object.entries(v as Schema)) audit(pv, `${where}/properties/${pk}`);
    if (k === 'items' || (k === 'additionalProperties' && typeof v === 'object')) audit(v, `${where}/${k}`);
    if (k === 'oneOf' || k === 'anyOf') (v as unknown[]).forEach((x, i) => audit(x, `${where}/${k}/${i}`));
    if (k === '$ref' && !(typeof v === 'string' && v.startsWith('#/'))) throw new SchemaUnsupported(`${where}: only local $ref is supported`);
  }
}

export type Check = (value: unknown) => Issue[];

/** Compile a schema document; `pointer` picks a sub-schema such as `#/$defs/job`. */
export function compile(root: Schema, pointer = '#'): Check {
  audit(root, '#');
  const regexCache = new Map<string, RegExp>();
  const resolve = (ref: string): unknown => {
    let node: unknown = root;
    for (const part of ref.replace(/^#\/?/, '').split('/').filter(Boolean)) {
      node = (node as Record<string, unknown> | undefined)?.[part.replace(/~1/g, '/').replace(/~0/g, '~')];
      if (node === undefined) throw new SchemaUnsupported(`unresolved $ref ${ref}`);
    }
    return node;
  };

  function check(s: unknown, v: unknown, path: string, out: Issue[]): void {
    if (s === true) return;
    if (s === false) { out.push({ path, msg: 'no value is allowed here' }); return; }
    const sc = s as Schema;
    if (typeof sc.$ref === 'string') check(resolve(sc.$ref), v, path, out);

    if (sc.type !== undefined) {
      const types = Array.isArray(sc.type) ? (sc.type as string[]) : [sc.type as string];
      if (!types.some(t => typeMatches(t, v))) { out.push({ path, msg: `expected ${types.join(' or ')}, got ${typeOf(v)}` }); return; }
    }
    if ('const' in sc && !equal(sc.const, v)) out.push({ path, msg: `must be ${short(sc.const)}, got ${short(v)}` });
    if (Array.isArray(sc.enum) && !sc.enum.some(e => equal(e, v))) out.push({ path, msg: `${short(v)} is not one of ${sc.enum.map(short).join(', ')}` });

    if (typeof v === 'number') {
      if (typeof sc.minimum === 'number' && v < sc.minimum) out.push({ path, msg: `${v} is below the minimum ${sc.minimum}` });
      if (typeof sc.maximum === 'number' && v > sc.maximum) out.push({ path, msg: `${v} is above the maximum ${sc.maximum}` });
    }
    if (typeof v === 'string') {
      const len = [...v].length;
      if (typeof sc.minLength === 'number' && len < sc.minLength) out.push({ path, msg: `shorter than ${sc.minLength} character${sc.minLength === 1 ? '' : 's'}` });
      if (typeof sc.maxLength === 'number' && len > sc.maxLength) out.push({ path, msg: `longer than ${sc.maxLength} characters` });
      if (typeof sc.pattern === 'string') {
        let re = regexCache.get(sc.pattern);
        if (!re) { re = new RegExp(sc.pattern, 'u'); regexCache.set(sc.pattern, re); }
        if (!re.test(v)) out.push({ path, msg: `${short(v)} does not match ${sc.pattern}` });
      }
    }
    if (Array.isArray(v)) {
      if (typeof sc.minItems === 'number' && v.length < sc.minItems) out.push({ path, msg: `fewer than ${sc.minItems} items` });
      if (typeof sc.maxItems === 'number' && v.length > sc.maxItems) out.push({ path, msg: `more than ${sc.maxItems} items` });
      if (sc.items !== undefined) v.forEach((item, i) => check(sc.items, item, `${path}/${i}`, out));
    }
    if (v && typeof v === 'object' && !Array.isArray(v)) {
      const obj = v as Record<string, unknown>;
      const props = (sc.properties ?? {}) as Schema;
      if (Array.isArray(sc.required)) for (const k of sc.required as string[]) if (!(k in obj)) out.push({ path: `${path}/${k}`, msg: 'is required and missing' });
      for (const [k, val] of Object.entries(obj)) {
        if (k in props) check(props[k], val, `${path}/${k}`, out);
        else if (sc.additionalProperties === false) out.push({ path: `${path}/${k}`, msg: 'is not allowed (additionalProperties is false)' });
        else if (sc.additionalProperties && typeof sc.additionalProperties === 'object') check(sc.additionalProperties, val, `${path}/${k}`, out);
      }
    }
    if (Array.isArray(sc.oneOf)) {
      const results = sc.oneOf.map(sub => { const o: Issue[] = []; check(sub, v, path, o); return o; });
      const ok = results.filter(r => r.length === 0).length;
      if (ok !== 1) {
        if (ok === 0) {
          // report the branch with the fewest issues: it is almost always the one that was meant
          const best = results.reduce((a, b) => (b.length < a.length ? b : a));
          out.push(...best);
        } else out.push({ path, msg: `matches ${ok} of the oneOf branches; exactly 1 is required` });
      }
    }
    if (Array.isArray(sc.anyOf)) {
      const results = sc.anyOf.map(sub => { const o: Issue[] = []; check(sub, v, path, o); return o; });
      if (!results.some(r => r.length === 0)) out.push(...results.reduce((a, b) => (b.length < a.length ? b : a)));
    }
  }

  const target = pointer === '#' ? root : resolve(pointer);
  return (value: unknown) => { const out: Issue[] = []; check(target, value, '', out); return out; };
}

export function describe(issues: Issue[], max = 2): string {
  const head = issues.slice(0, max).map(i => (i.path ? `${i.path} ${i.msg}` : i.msg)).join('; ');
  return issues.length > max ? `${head}; and ${issues.length - max} more` : head;
}
