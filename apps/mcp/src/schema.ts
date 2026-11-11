// A tiny JSON Schema checker: only the keywords the Agentroid schemas use (no runtime dependency).
// Supported: type (string or array), enum, const, required, properties, additionalProperties
// (boolean or schema), items (schema or tuple array), minItems, maxItems, minimum, maximum,
// minLength, maxLength, pattern, anyOf, oneOf, allOf, boolean schemas, local $ref
// ("#/definitions/x", "#/$defs/x"). `format`, `description`, `default`, `title`, `$schema`, `$id` are ignored.

export type Schema = boolean | { [key: string]: unknown };

export interface SchemaIssue {
  path: string;
  message: string;
}

function typeOf(v: unknown): string {
  if (v === null) return "null";
  if (Array.isArray(v)) return "array";
  return typeof v;
}

function matchesType(v: unknown, t: string): boolean {
  switch (t) {
    case "integer":
      return typeof v === "number" && Number.isInteger(v);
    case "number":
      return typeof v === "number" && Number.isFinite(v);
    default:
      return typeOf(v) === t;
  }
}

function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (typeof a !== "object" || typeof b !== "object" || a === null || b === null) return false;
  if (Array.isArray(a) !== Array.isArray(b)) return false;
  const ka = Object.keys(a as object);
  const kb = Object.keys(b as object);
  if (ka.length !== kb.length) return false;
  return ka.every((k) => deepEqual((a as Record<string, unknown>)[k], (b as Record<string, unknown>)[k]));
}

function resolveRef(ref: string, root: Schema): Schema | undefined {
  if (!ref.startsWith("#/")) return undefined;
  let cur: unknown = root;
  for (const part of ref.slice(2).split("/")) {
    if (typeof cur !== "object" || cur === null) return undefined;
    cur = (cur as Record<string, unknown>)[part.replace(/~1/g, "/").replace(/~0/g, "~")];
  }
  return cur as Schema | undefined;
}

function check(value: unknown, schema: Schema, root: Schema, path: string, out: SchemaIssue[], depth: number): void {
  if (depth > 40) {
    out.push({ path, message: "schema nesting too deep" });
    return;
  }
  if (schema === true) return;
  if (schema === false) {
    out.push({ path, message: "not allowed" });
    return;
  }
  const s = schema;
  if (typeof s.$ref === "string") {
    const target = resolveRef(s.$ref, root);
    if (target === undefined) out.push({ path, message: `unresolved $ref ${s.$ref}` });
    else check(value, target, root, path, out, depth + 1);
    return;
  }
  if (s.type !== undefined) {
    const types = Array.isArray(s.type) ? (s.type as string[]) : [s.type as string];
    if (!types.some((t) => matchesType(value, t))) {
      out.push({ path, message: `expected ${types.join("|")}, got ${typeOf(value)}` });
      return;
    }
  }
  if (Array.isArray(s.enum) && !s.enum.some((e) => deepEqual(e, value))) {
    out.push({ path, message: `not one of ${JSON.stringify(s.enum)}` });
  }
  if ("const" in s && !deepEqual(s.const, value)) {
    out.push({ path, message: `must equal ${JSON.stringify(s.const)}` });
  }
  if (typeof value === "number") {
    if (typeof s.minimum === "number" && value < s.minimum) out.push({ path, message: `below minimum ${s.minimum}` });
    if (typeof s.maximum === "number" && value > s.maximum) out.push({ path, message: `above maximum ${s.maximum}` });
  }
  if (typeof value === "string") {
    if (typeof s.minLength === "number" && [...value].length < s.minLength) out.push({ path, message: `shorter than ${s.minLength}` });
    if (typeof s.maxLength === "number" && [...value].length > s.maxLength) out.push({ path, message: `longer than ${s.maxLength}` });
    if (typeof s.pattern === "string" && !new RegExp(s.pattern, "u").test(value)) out.push({ path, message: `does not match ${s.pattern}` });
  }
  if (Array.isArray(value)) {
    if (typeof s.minItems === "number" && value.length < s.minItems) out.push({ path, message: `fewer than ${s.minItems} items` });
    if (typeof s.maxItems === "number" && value.length > s.maxItems) out.push({ path, message: `more than ${s.maxItems} items` });
    if (Array.isArray(s.items)) {
      value.forEach((item, i) => {
        const sub = (s.items as Schema[])[i];
        if (sub !== undefined) check(item, sub, root, `${path}/${i}`, out, depth + 1);
      });
    } else if (s.items !== undefined) {
      value.forEach((item, i) => check(item, s.items as Schema, root, `${path}/${i}`, out, depth + 1));
    }
  }
  if (typeof value === "object" && value !== null && !Array.isArray(value)) {
    const obj = value as Record<string, unknown>;
    if (Array.isArray(s.required)) {
      for (const k of s.required as string[]) if (!(k in obj)) out.push({ path, message: `missing required "${k}"` });
    }
    const props = (s.properties ?? {}) as Record<string, Schema>;
    for (const [k, sub] of Object.entries(props)) {
      if (k in obj) check(obj[k], sub, root, `${path}/${k}`, out, depth + 1);
    }
    if (s.additionalProperties !== undefined) {
      for (const k of Object.keys(obj)) {
        if (k in props) continue;
        if (s.additionalProperties === false) out.push({ path: `${path}/${k}`, message: "unexpected property" });
        else if (s.additionalProperties !== true) check(obj[k], s.additionalProperties as Schema, root, `${path}/${k}`, out, depth + 1);
      }
    }
  }
  if (Array.isArray(s.allOf)) {
    for (const sub of s.allOf as Schema[]) check(value, sub, root, path, out, depth + 1);
  }
  if (Array.isArray(s.anyOf)) {
    const ok = (s.anyOf as Schema[]).some((sub) => {
      const tmp: SchemaIssue[] = [];
      check(value, sub, root, path, tmp, depth + 1);
      return tmp.length === 0;
    });
    if (!ok) out.push({ path, message: "matches none of anyOf" });
  }
  if (Array.isArray(s.oneOf)) {
    const n = (s.oneOf as Schema[]).filter((sub) => {
      const tmp: SchemaIssue[] = [];
      check(value, sub, root, path, tmp, depth + 1);
      return tmp.length === 0;
    }).length;
    if (n !== 1) out.push({ path, message: `matches ${n} of oneOf, needs exactly 1` });
  }
}

/** All issues; an empty array means the value is valid. */
export function validate(value: unknown, schema: Schema): SchemaIssue[] {
  const out: SchemaIssue[] = [];
  check(value, schema, schema, "", out, 0);
  return out;
}

export function isValid(value: unknown, schema: Schema): boolean {
  return validate(value, schema).length === 0;
}

/** One line for an error message. */
export function describeIssues(issues: SchemaIssue[], max = 3): string {
  const shown = issues.slice(0, max).map((i) => `${i.path || "/"}: ${i.message}`);
  const more = issues.length > max ? ` (+${issues.length - max} more)` : "";
  return shown.join("; ") + more;
}
