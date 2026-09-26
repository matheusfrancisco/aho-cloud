import { describe, expect, it } from "vitest";
import { approvalChallenge, approvalMessage, canonicalHash, canonicalJson, standingOrdersMessage } from "../src/lib/approval";
import { encode } from "../src/lib/base64url";
import type { StandingOrder } from "../src/gen/contract";

// Expected values printed by the Rust implementation (cloud_contract::ledger::approval_message and
// cloud_contract::canonical) for the same inputs; see the parity program in the build notes.
const RUST_LIVE_HEX =
  "61686f2d636c6f75642f617070726f76652f76310a73686f700a722d30316a387a330a626c616b65333a3966326334650a6c697665";
const RUST_STAGING_HEX =
  "61686f2d636c6f75642f617070726f76652f76310a73686f700a722d30316a387a330a626c616b65333a3966326334650a73746167696e67";

const hex = (s: string) => [...new TextEncoder().encode(s)].map((b) => b.toString(16).padStart(2, "0")).join("");

describe("approvalMessage", () => {
  it("is byte-identical to Rust approval_message", () => {
    expect(hex(approvalMessage("shop", "r-01j8z3", "blake3:9f2c4e", "live"))).toBe(RUST_LIVE_HEX);
    expect(hex(approvalMessage("shop", "r-01j8z3", "blake3:9f2c4e", "staging"))).toBe(RUST_STAGING_HEX);
  });

  it("binds the stage: the same release signs differently per stage", () => {
    const live = approvalChallenge(approvalMessage("shop", "r-01j8z3", "blake3:9f2c4e", "live"));
    const staging = approvalChallenge(approvalMessage("shop", "r-01j8z3", "blake3:9f2c4e", "staging"));
    expect(encode(live)).not.toBe(encode(staging));
  });

  it("challenge is SHA-256 of the message bytes (base64url, as the server compares it)", () => {
    const challenge = approvalChallenge(approvalMessage("shop", "r-01j8z3", "blake3:9f2c4e", "live"));
    expect(challenge.length).toBe(32);
    expect(encode(challenge)).toBe("e5QuPY7b7GoeZEkmlEEkKdp1AbP2x941jyi9lcZD2Qc");
  });
});

describe("canonical JSON", () => {
  it("sorts keys by code point like Rust, not by UTF-16 unit, and escapes like serde_json", () => {
    const v = { b: 1, é: 'x\u0001\n"', a: [true, null, 2.5], "\u{1F600}": 1, "\uFF61": 2 };
    expect(canonicalJson(v)).toBe('{"a":[true,null,2.5],"b":1,"é":"x\\u0001\\n\\"","｡":2,"😀":1}');
    expect(canonicalHash(v)).toBe("blake3:98505f5a8af7150a51f652729d8aced3509c04b82384920e0aae3a69943f83a3");
  });

  it("standing-orders message matches the control plane's (hash of the orders, at live)", () => {
    const orders: StandingOrder[] = [{ kind: "restart" }, { kind: "scale", min: 1, max: 4 }, { kind: "maintenance_page" }, { kind: "rollback_within_ceiling" }];
    expect(canonicalJson(orders)).toBe('[{"kind":"restart"},{"kind":"scale","max":4,"min":1},{"kind":"maintenance_page"},{"kind":"rollback_within_ceiling"}]');
    expect(standingOrdersMessage("shop", orders)).toBe(
      "aho-cloud/approve/v1\nshop\nstanding-orders\nblake3:fe5aafafec68602859cbe412023a8a304b12b0f14b1b4044a905654c6f745156\nlive",
    );
  });
});
