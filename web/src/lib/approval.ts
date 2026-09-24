// What a human signs. Byte-for-byte ports of `cloud_contract::ledger::approval_message` and
// `cloud_contract::canonical`, because the server verifies the passkey challenge against them.
import { blake3 } from "@noble/hashes/blake3.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { bytesToHex } from "@noble/hashes/utils.js";
import type { Stage, StandingOrder } from "../gen/contract";

const utf8 = new TextEncoder();

/** `approval_message(project, release, manifest_hash, stage)` as UTF-8 text. */
export function approvalMessage(project: string, release: string, manifestHash: string, stage: Stage): string {
  return `aho-cloud/approve/v1\n${project}\n${release}\n${manifestHash}\n${stage}`;
}

/** The WebAuthn challenge for an approval: SHA-256 of the message bytes. */
export function approvalChallenge(message: string): Uint8Array<ArrayBuffer> {
  return new Uint8Array(sha256(utf8.encode(message)));
}

/** Rust `str` ordering (UTF-8 bytes = code points), not JS's UTF-16 code-unit order. */
function byCodePoint(a: string, b: string): number {
  const x = [...a];
  const y = [...b];
  for (let i = 0; i < Math.min(x.length, y.length); i++) {
    const d = x[i]!.codePointAt(0)! - y[i]!.codePointAt(0)!;
    if (d !== 0) return d;
  }
  return x.length - y.length;
}

/** `canonical::to_bytes` as text: object keys sorted by code point, no whitespace. */
export function canonicalJson(value: unknown): string {
  if (value === null || typeof value !== "object") {
    const s = JSON.stringify(value);
    if (s === undefined) throw new Error(`canonicalJson: ${typeof value} is not JSON`);
    return s;
  }
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  const entries = Object.entries(value as Record<string, unknown>).filter(([, v]) => v !== undefined);
  entries.sort(([a], [b]) => byCodePoint(a, b));
  return `{${entries.map(([k, v]) => `${JSON.stringify(k)}:${canonicalJson(v)}`).join(",")}}`;
}

/** `canonical::hash`: `blake3:<hex>` over the canonical bytes. */
export function canonicalHash(value: unknown): string {
  return `blake3:${bytesToHex(blake3(utf8.encode(canonicalJson(value))))}`;
}

/** What the control plane verifies for `GrantStandingOrders`: `canonical::hash(&orders)` at `live` (the TTL is not signed). */
export function standingOrdersMessage(project: string, orders: StandingOrder[]): string {
  return approvalMessage(project, "standing-orders", canonicalHash(orders), "live");
}
