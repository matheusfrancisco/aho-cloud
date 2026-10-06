import { describe, expect, it } from "vitest";
import { decodeFrame, encodeFrame, type Frame, MAX_FRAME } from "@aho/ui";

describe("exec frames", () => {
  it("has the same wire bytes as crates/api/src/exec.rs (wire_bytes_are_fixed)", () => {
    expect([...encodeFrame({ kind: "resize", cols: 0x0102, rows: 0x0304 })]).toEqual([3, 1, 2, 3, 4]);
    expect([...encodeFrame({ kind: "exit", code: 130 })]).toEqual([5, 0, 0, 0, 130]);
    expect([...encodeFrame({ kind: "stdin", data: new TextEncoder().encode("a") })]).toEqual([0, 97]);
    expect([...encodeFrame({ kind: "close_stdin" })]).toEqual([4]);
    // Exit is signed: -1 is all ones, as Rust's i32::to_be_bytes.
    expect([...encodeFrame({ kind: "exit", code: -1 })]).toEqual([5, 255, 255, 255, 255]);
  });

  it("round-trips every frame", () => {
    const frames: Frame[] = [
      { kind: "stdin", data: new TextEncoder().encode("ls -la\r") },
      { kind: "stdout", data: Uint8Array.of(0, 1, 2, 255) },
      { kind: "stderr", data: new TextEncoder().encode("oops") },
      { kind: "resize", cols: 120, rows: 40 },
      { kind: "resize", cols: 65_535, rows: 1 },
      { kind: "close_stdin" },
      { kind: "exit", code: -1 },
      { kind: "exit", code: 2_147_483_647 },
      { kind: "error", message: "no such sandbox — ünïcode" },
      { kind: "request", request: { argv: ["bash", "-l"], pty: { cols: 80, rows: 24 }, env: { TERM: "xterm-256color" }, cwd: "/workspace" } },
    ];
    // Byte payloads compared as plain arrays: jsdom's TextEncoder and Node's Uint8Array are different classes.
    const plain = (f: Frame) => ("data" in f ? { ...f, data: [...f.data] } : f);
    for (const f of frames) expect(plain(decodeFrame(encodeFrame(f)))).toEqual(plain(f));
  });

  it("decodes a payload that sits inside a larger buffer", () => {
    const backing = Uint8Array.of(9, 9, 5, 0, 0, 0, 7, 9);
    expect(decodeFrame(backing.subarray(2, 7))).toEqual({ kind: "exit", code: 7 });
  });

  it("refuses empty, malformed, unknown and oversized frames like the Rust decoder", () => {
    const refusal = (bytes: Uint8Array) => {
      try {
        decodeFrame(bytes);
        return null;
      } catch (e) {
        return (e as Error).message;
      }
    };
    expect(refusal(new Uint8Array())).toBe("empty frame");
    expect(refusal(Uint8Array.of(3, 0, 80))).toBe("malformed resize frame");
    expect(refusal(Uint8Array.of(5, 0))).toBe("malformed exit frame");
    expect(refusal(Uint8Array.of(4, 1))).toBe("malformed close-stdin frame");
    expect(refusal(Uint8Array.of(6, 0xff))).toBe("malformed error frame");
    expect(refusal(Uint8Array.of(7, 123))).toBe("malformed request frame");
    expect(refusal(Uint8Array.of(9))).toBe("unknown frame kind 9");
    expect(refusal(new Uint8Array(MAX_FRAME + 1))).toBe(`frame is ${MAX_FRAME + 1} bytes; the limit is ${MAX_FRAME}`);
    expect(refusal(new Uint8Array(MAX_FRAME))).toBeNull();
  });
});
