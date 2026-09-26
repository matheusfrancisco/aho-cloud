import { afterEach, beforeEach, describe, expect, it, type Mock, vi } from "vitest";
import { Api } from "../src/lib/api";
import { approvalChallenge, approvalMessage } from "../src/lib/approval";
import { decode, encode } from "../src/lib/base64url";
import { currentSession } from "../src/lib/session";
import { creationOptions, passkeyProof, registerPasskey } from "../src/lib/webauthn";

const bytes = (...b: number[]) => new Uint8Array(b).buffer;

let get: Mock;
let create: Mock;

/** The bytes of a WebAuthn BufferSource, for comparison. */
const u8 = (b: BufferSource) => [...(ArrayBuffer.isView(b) ? new Uint8Array(b.buffer, b.byteOffset, b.byteLength) : new Uint8Array(b))];

beforeEach(() => {
  get = vi.fn(async () => ({
    id: "AQID",
    rawId: bytes(1, 2, 3),
    type: "public-key",
    response: { clientDataJSON: bytes(123, 125), authenticatorData: bytes(9, 9), signature: bytes(0xfb, 0xff), userHandle: bytes(7) },
  }));
  create = vi.fn(async () => ({ id: "BAU", rawId: bytes(4, 5), type: "public-key", response: { clientDataJSON: bytes(1), attestationObject: bytes(0xa3, 0x01) } }));
  vi.stubGlobal("PublicKeyCredential", class {});
  Object.defineProperty(navigator, "credentials", { value: { get, create }, configurable: true });
});
afterEach(() => {
  vi.unstubAllGlobals();
  Object.defineProperty(navigator, "credentials", { value: undefined, configurable: true });
});

function apiReplying(body: unknown | ((url: string) => unknown)) {
  const sent: Array<{ url: string; body: unknown }> = [];
  const api = new Api({
    token: () => "tok",
    fetch: (async (url: RequestInfo | URL, init?: RequestInit) => {
      sent.push({ url: String(url), body: init?.body === undefined ? undefined : JSON.parse(init.body as string) });
      return new Response(JSON.stringify(typeof body === "function" ? body(String(url)) : body), { status: 200 });
    }) as typeof fetch,
  });
  return { api, sent };
}

describe("passkey approval", () => {
  it("asserts over SHA-256(approval message) with the server's RP id and credential ids, and returns base64url JSON", async () => {
    const { api, sent } = apiReplying({
      ceremony_id: "c1",
      options: { publicKey: { challenge: "c2VydmVyLWNoYWxsZW5nZQ", rpId: "aho.cloud", userVerification: "required", allowCredentials: [{ type: "public-key", id: "AQID", transports: ["internal"] }] } },
    });
    const message = approvalMessage("shop", "r-01j8z3", "blake3:9f2c4e", "live");
    const proof = await passkeyProof(api, "ana@example.com", message);

    expect(sent).toEqual([{ url: "/v1/auth/passkey/login/start", body: { email: "ana@example.com" } }]);
    const request: CredentialRequestOptions = get.mock.calls[0]![0];
    const pk = request.publicKey!;
    // The server's login challenge is replaced by the approval challenge.
    expect(u8(pk.challenge)).toEqual([...approvalChallenge(message)]);
    expect(pk.rpId).toBe("aho.cloud");
    expect(pk.userVerification).toBe("required");
    expect(u8(pk.allowCredentials![0]!.id)).toEqual([1, 2, 3]);
    expect(pk.allowCredentials![0]!.transports).toEqual(["internal"]);

    expect(proof).toEqual({
      method: "passkey",
      credential: { id: "AQID", rawId: "AQID", type: "public-key", response: { clientDataJSON: "e30", authenticatorData: "CQk", signature: "-_8", userHandle: "Bw" } },
    });
  });

  it("registers: decodes challenge, user id and excluded credentials, sends the attestation, stores the session", async () => {
    const options = {
      publicKey: {
        rp: { id: "aho.cloud", name: "aho cloud" },
        user: { id: encode(new Uint8Array([42, 43])), name: "ana@example.com", displayName: "Ana" },
        challenge: "AAEC",
        pubKeyCredParams: [{ type: "public-key", alg: -7 }],
        excludeCredentials: [{ type: "public-key", id: "BAU" }],
        attestation: "none",
      },
    };
    const converted = creationOptions(options);
    expect(u8(converted.challenge)).toEqual([0, 1, 2]);
    expect(u8(converted.user.id)).toEqual([42, 43]);
    expect(converted.user.displayName).toBe("Ana");
    expect(u8(converted.excludeCredentials![0]!.id)).toEqual([4, 5]);
    expect(converted.pubKeyCredParams).toEqual([{ type: "public-key", alg: -7 }]);

    const user = { id: "u1", email: "ana@example.com", name: "Ana" };
    const { api, sent } = apiReplying((url: string) => (url.endsWith("/start") ? { ceremony_id: "c9", options } : { token: "new-token", user }));
    await registerPasskey(api, "ana@example.com", "Ana");
    expect(sent[1]).toEqual({
      url: "/v1/auth/passkey/register/finish",
      body: { ceremony_id: "c9", credential: { id: "BAU", rawId: "BAU", type: "public-key", response: { clientDataJSON: "AQ", attestationObject: "owE" } } },
    });
    expect(currentSession()).toEqual({ token: "new-token", user });
    expect([...decode("owE")]).toEqual([0xa3, 0x01]);
  });

  it("refuses options without a challenge rather than sending an empty one", () => {
    expect(() => creationOptions({ publicKey: { user: { id: "AA" } } })).toThrow(/challenge is missing/);
  });
});
