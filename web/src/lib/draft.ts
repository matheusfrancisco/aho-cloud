// Draft helpers mirroring `cloud_contract::draft`: which fields need a human, and the manifest
// JSON the fields assemble into. Drafts are treated as immutable values.
import type { Draft, DraftField, Strength } from "../gen/contract";

/** Weakest first: the order the confirm screen shows them. */
export const STRENGTHS: readonly Strength[] = ["inferred", "observed", "chained", "declared"];

/** Inferred and observed values must be confirmed before the draft becomes a manifest. */
export function needsConfirmation(strength: Strength): boolean {
  return strength === "inferred" || strength === "observed";
}

/** Pointers of fields still waiting for confirmation, sorted like the Rust `BTreeMap`. */
export function unconfirmed(draft: Draft): string[] {
  return Object.entries(draft.fields)
    .filter(([, f]) => f !== undefined && needsConfirmation(f.strength) && !f.confirmed)
    .map(([k]) => k)
    .sort();
}

/** A human accepted the drafted value as is. */
export function confirm(draft: Draft, pointer: string): Draft {
  const f = draft.fields[pointer];
  if (f === undefined) return draft;
  return { fields: { ...draft.fields, [pointer]: { ...f, confirmed: true } } };
}

/** A human typed or corrected the value: it becomes `declared`, keeping the evidence. */
export function declare(draft: Draft, pointer: string, value: unknown): Draft {
  const evidence = draft.fields[pointer]?.evidence ?? [];
  const field: DraftField = { value, strength: "declared", evidence, confirmed: true };
  return { fields: { ...draft.fields, [pointer]: field } };
}

/** Fields by strength, pointers sorted within each group. */
export function byStrength(draft: Draft): Record<Strength, Array<[string, DraftField]>> {
  const groups: Record<Strength, Array<[string, DraftField]>> = { inferred: [], observed: [], chained: [], declared: [] };
  for (const [k, f] of Object.entries(draft.fields).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) {
    if (f !== undefined) groups[f.strength].push([k, f]);
  }
  return groups;
}

type Json = unknown;

/** `Draft::to_value`: inserts every field at its JSON pointer; numeric segments make arrays. */
export function draftToValue(draft: Draft): Record<string, Json> {
  const root: Record<string, Json> = {};
  for (const [pointer, field] of Object.entries(draft.fields).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) {
    if (field !== undefined) insert(root, pointer, field.value);
  }
  return root;
}

function isIndex(seg: string | undefined): boolean {
  return seg !== undefined && /^\d+$/.test(seg);
}

function insert(root: Record<string, Json>, pointer: string, value: Json): void {
  if (!pointer.startsWith("/")) throw new Error(`field \`${pointer}\` is not a JSON pointer`);
  const segments = pointer
    .slice(1)
    .split("/")
    .map((s) => s.replaceAll("~1", "/").replaceAll("~0", "~"));
  let node: Json = root;
  for (let i = 0; i < segments.length; i++) {
    const seg = segments[i]!;
    const last = i + 1 === segments.length;
    const fresh = (): Json => (isIndex(segments[i + 1]) ? [] : {});
    if (Array.isArray(node)) {
      if (!isIndex(seg)) throw new Error(`field \`${pointer}\` is not a JSON pointer`);
      const idx = Number(seg);
      while (node.length <= idx) node.push(null);
      if (last) {
        node[idx] = value;
        return;
      }
      if (node[idx] === null) node[idx] = fresh();
      node = node[idx];
    } else if (typeof node === "object" && node !== null) {
      const map = node as Record<string, Json>;
      if (last) {
        if (Object.hasOwn(map, seg)) throw new Error(`field \`${pointer}\` conflicts with \`${seg}\``);
        map[seg] = value;
        return;
      }
      if (!Object.hasOwn(map, seg)) map[seg] = fresh();
      node = map[seg];
    } else {
      throw new Error(`field \`${pointer}\` conflicts with \`${segments.slice(0, i).join("/")}\``);
    }
  }
}
