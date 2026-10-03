// Stands in for `@xterm/xterm` and `@xterm/addon-fit` in jsdom (no canvas, no layout): records
// what is written, and lets a test type into the terminal or change the size a fit picks.
// Use with `vi.mock("@xterm/xterm", () => import("./fakeXterm"))` (and the same for the addon).

type Listener<T> = (value: T) => void;

class Emitter<T> {
  readonly #listeners = new Set<Listener<T>>();
  readonly event = (l: Listener<T>) => {
    this.#listeners.add(l);
    return { dispose: () => this.#listeners.delete(l) };
  };
  fire(value: T): void {
    for (const l of this.#listeners) l(value);
  }
}

/** Every terminal created, newest last. */
export const terminals: Terminal[] = [];
/** The size the next `FitAddon.fit()` gives the terminal. */
export const layout = { cols: 80, rows: 24 };

export class Terminal {
  cols = 80;
  rows = 24;
  disposed = false;
  #written: Array<string | Uint8Array> = [];
  readonly #data = new Emitter<string>();
  readonly #binary = new Emitter<string>();
  readonly #resize = new Emitter<{ cols: number; rows: number }>();
  readonly onData = this.#data.event;
  readonly onBinary = this.#binary.event;
  readonly onResize = this.#resize.event;

  constructor() {
    terminals.push(this);
  }
  open(): void {}
  focus(): void {}
  loadAddon(addon: { activate(t: Terminal): void }): void {
    addon.activate(this);
  }
  write(data: string | Uint8Array): void {
    this.#written.push(data);
  }
  reset(): void {
    this.#written = [];
  }
  dispose(): void {
    this.disposed = true;
  }
  resize(cols: number, rows: number): void {
    if (cols === this.cols && rows === this.rows) return;
    this.cols = cols;
    this.rows = rows;
    this.#resize.fire({ cols, rows });
  }
  /** The user typing `text`. */
  type(text: string): void {
    this.#data.fire(text);
  }
  /** Everything written since the last reset, as text. */
  get text(): string {
    return this.#written.map((w) => (typeof w === "string" ? w : new TextDecoder().decode(w))).join("");
  }
}

export class FitAddon {
  #term: Terminal | null = null;
  activate(t: Terminal): void {
    this.#term = t;
  }
  fit(): void {
    this.#term?.resize(layout.cols, layout.rows);
  }
  dispose(): void {}
}
