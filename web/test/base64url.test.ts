import { describe, expect, it } from "vitest";
import { decode, encode } from "../src/lib/base64url";

describe("base64url", () => {
  it("round-trips every length and byte value, matching RFC 4648 §5 without padding", () => {
    for (let len = 0; len < 70; len++) {
      const bytes = new Uint8Array(len).map((_, i) => (i * 97 + len * 31 + 255) & 0xff);
      const text = encode(bytes);
      expect(text).toBe(Buffer.from(bytes).toString("base64url"));
      expect(text).not.toMatch(/[=+/]/);
      expect([...decode(text)]).toEqual([...bytes]);
    }
    const all = new Uint8Array(256).map((_, i) => i);
    expect([...decode(encode(all))]).toEqual([...all]);
  });

  it("encodes a view over part of a buffer, not the whole buffer", () => {
    const buf = new Uint8Array([1, 2, 3, 250, 251, 252, 9]);
    expect(encode(buf.subarray(3, 6))).toBe("-vv8");
  });

  it("tolerates padding but rejects characters outside the alphabet and impossible lengths", () => {
    expect([...decode("-vv8")]).toEqual([250, 251, 252]);
    expect([...decode("AQ==")]).toEqual([1]);
    expect(() => decode("+vv8")).toThrow(/unexpected "\+"/);
    expect(() => decode("ab/c")).toThrow(/unexpected/);
    expect(() => decode("abcde")).toThrow(/length/);
    expect(() => decode("ab=c")).toThrow(/unexpected "="/);
  });
});
