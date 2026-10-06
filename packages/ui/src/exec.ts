// The exec frame protocol (`crates/api/src/exec.rs`, mirrored byte for byte): one WebSocket
// binary message per frame, first byte the kind, the rest the payload. `Resize` = cols u16 BE,
// rows u16 BE; `Exit` = i32 BE (`128 + signal` when killed); `Error` = UTF-8 text and ends the
// session. Terminal sockets never carry `Request`: the session host's token is bound to the
// request the control plane made.

/** A terminal size in character cells. */
export interface PtySize {
  cols: number;
  rows: number;
}

/** The command a `Request` frame opens a session for (`ExecRequest` in `crates/api/src/exec.rs`). */
export interface ExecRequest {
  argv: string[];
  /** Allocate a terminal of this size; without it, stdout and stderr are separate pipes. */
  pty: PtySize | null;
  /** Added to the target's own environment. */
  env: { [key: string]: string };
  cwd: string | null;
}

export const KIND_STDIN = 0;
export const KIND_STDOUT = 1;
export const KIND_STDERR = 2;
export const KIND_RESIZE = 3;
export const KIND_CLOSE_STDIN = 4;
export const KIND_EXIT = 5;
export const KIND_ERROR = 6;
/** Only on byte streams: the JSON `ExecRequest` that opens the session. */
export const KIND_REQUEST = 7;

/** Largest frame (kind byte included) either side sends or accepts. */
export const MAX_FRAME = 1 << 20;

export type Frame =
  | { kind: "stdin"; data: Uint8Array }
  | { kind: "stdout"; data: Uint8Array }
  | { kind: "stderr"; data: Uint8Array }
  | { kind: "resize"; cols: number; rows: number }
  | { kind: "close_stdin" }
  | { kind: "exit"; code: number }
  | { kind: "error"; message: string }
  | { kind: "request"; request: ExecRequest };

/** A frame that does not decode; the message matches the Rust `FrameError`. */
export class FrameError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "FrameError";
  }
}

function withKind(kind: number, payload: Uint8Array): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(payload.length + 1);
  out[0] = kind;
  out.set(payload, 1);
  return out;
}

export function encodeFrame(f: Frame): Uint8Array<ArrayBuffer> {
  switch (f.kind) {
    case "stdin":
      return withKind(KIND_STDIN, f.data);
    case "stdout":
      return withKind(KIND_STDOUT, f.data);
    case "stderr":
      return withKind(KIND_STDERR, f.data);
    case "resize": {
      const out = new Uint8Array(5);
      const view = new DataView(out.buffer);
      out[0] = KIND_RESIZE;
      view.setUint16(1, f.cols);
      view.setUint16(3, f.rows);
      return out;
    }
    case "close_stdin":
      return Uint8Array.of(KIND_CLOSE_STDIN);
    case "exit": {
      const out = new Uint8Array(5);
      out[0] = KIND_EXIT;
      new DataView(out.buffer).setInt32(1, f.code);
      return out;
    }
    case "error":
      return withKind(KIND_ERROR, new TextEncoder().encode(f.message));
    case "request":
      return withKind(KIND_REQUEST, new TextEncoder().encode(JSON.stringify(f.request)));
  }
}

/** Throws `FrameError` for an empty, oversized, unknown or malformed frame. */
export function decodeFrame(bytes: Uint8Array): Frame {
  if (bytes.length > MAX_FRAME) throw new FrameError(`frame is ${bytes.length} bytes; the limit is ${MAX_FRAME}`);
  const kind = bytes[0];
  if (kind === undefined) throw new FrameError("empty frame");
  const rest = bytes.subarray(1);
  const view = new DataView(rest.buffer, rest.byteOffset, rest.byteLength);
  switch (kind) {
    case KIND_STDIN:
      return { kind: "stdin", data: rest.slice() };
    case KIND_STDOUT:
      return { kind: "stdout", data: rest.slice() };
    case KIND_STDERR:
      return { kind: "stderr", data: rest.slice() };
    case KIND_RESIZE:
      if (rest.length !== 4) throw new FrameError("malformed resize frame");
      return { kind: "resize", cols: view.getUint16(0), rows: view.getUint16(2) };
    case KIND_CLOSE_STDIN:
      if (rest.length !== 0) throw new FrameError("malformed close-stdin frame");
      return { kind: "close_stdin" };
    case KIND_EXIT:
      if (rest.length !== 4) throw new FrameError("malformed exit frame");
      return { kind: "exit", code: view.getInt32(0) };
    case KIND_ERROR:
      try {
        return { kind: "error", message: new TextDecoder("utf-8", { fatal: true }).decode(rest) };
      } catch {
        throw new FrameError("malformed error frame");
      }
    case KIND_REQUEST:
      try {
        return { kind: "request", request: JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(rest)) as ExecRequest };
      } catch {
        throw new FrameError("malformed request frame");
      }
    default:
      throw new FrameError(`unknown frame kind ${kind}`);
  }
}
