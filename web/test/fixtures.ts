import type { Release } from "../src/gen/api";
import type { Manifest } from "../src/gen/contract";

export const manifest: Manifest = {
  api_version: "aho.cloud/v1",
  project: "shop",
  source: { kind: "archive", hash: "blake3:aa" },
  build: { kind: "dockerfile", path: "Dockerfile", context: "." },
  processes: [{ name: "web", command: null, port: 8080, expose: "public", health: { kind: "http", path: "/health", port: null }, volume: null }],
  domains: [{ hostname: "shop.example.com", verification: "cname" }],
  auth: { mode: "none" },
  secrets: [{ name: "STRIPE_KEY", reach: ["api.stripe.com"], expires: null }],
  egress: ["api.stripe.com"],
  data_class: "none",
  region: null,
  owner: { name: "Ana", email: "ana@example.com", phone: null, escalation: [] },
  budget: { ceiling_usd_month: 20, degrade: "sleep" },
};

export function release(over: Partial<Release> = {}): Release {
  return {
    id: "r-01j8z3",
    project: "shop",
    manifest_hash: "blake3:9f2c4e",
    manifest,
    provenance: { kind: "attested", session_id: "5f3a", head_hash: "c0ffee" },
    image_digest: "sha256:1234",
    stage: "staging",
    status: "awaiting_approval",
    diff: { changes: [] },
    requirement: { approvals: 1, owner_required: false },
    approvals: [],
    taint: [],
    undeclared_egress: [],
    ...over,
  };
}
