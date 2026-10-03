import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes, useParams } from "react-router";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "../src/App";
import { CreateSandboxModal } from "../src/components/CreateSandbox";
import { SandboxSettingsPanel } from "../src/components/SandboxSettings";
import { SandboxTerminal } from "../src/components/SandboxTerminal";
import type { EnvVarView, Sandbox } from "../src/gen/api";
import { api } from "../src/lib/api";
import { encodeFrame } from "../src/lib/exec";
import { DEFAULT_SANDBOX_EGRESS } from "../src/lib/sandbox";
import { signIn } from "../src/lib/session";
import { SandboxPage } from "../src/pages/SandboxPage";
import { layout, terminals } from "./fakeXterm";

vi.mock("@xterm/xterm", () => import("./fakeXterm"));
vi.mock("@xterm/addon-fit", () => import("./fakeXterm"));

const ana = { id: "u-ana", email: "ana@example.com", name: "Ana" };
const nowSecs = Math.floor(Date.now() / 1000);

function sandboxOf(over: Partial<Sandbox> = {}): Sandbox {
  return {
    slug: "scratch",
    org: "acme",
    state: "running",
    created_by: ana,
    created_at: nowSecs - 3600,
    image: "aho.sandbox@sha256:aa",
    settings: { egress: [...DEFAULT_SANDBOX_EGRESS], port: 3000, auto_stop_hours: 8 },
    url: "https://scratch.preview.test",
    stops_at: nowSecs + 7 * 3600,
    cpus: 2,
    mem_mib: 2048,
    disk_mib: 10240,
    error: null,
    ...over,
  };
}

/** A WebSocket the test drives: it opens, receives and drops on command and records what is sent. */
class FakeSocket {
  static all: FakeSocket[] = [];
  readonly url: string;
  readyState = 0;
  binaryType = "blob";
  sent: number[][] = [];
  closedWith: number | null = null;
  onopen: ((ev: unknown) => void) | null = null;
  onmessage: ((ev: { data: unknown }) => void) | null = null;
  onclose: ((ev: { code: number; reason: string }) => void) | null = null;
  onerror: ((ev: unknown) => void) | null = null;
  constructor(url: string) {
    this.url = url;
    FakeSocket.all.push(this);
  }
  send(data: Uint8Array | string) {
    this.sent.push(typeof data === "string" ? [...new TextEncoder().encode(data)] : [...data]);
  }
  close(code = 1000) {
    this.closedWith = code;
    this.readyState = 3;
  }
  open() {
    act(() => {
      this.readyState = 1;
      this.onopen?.({});
    });
  }
  receive(bytes: Uint8Array) {
    act(() => this.onmessage?.({ data: bytes.slice().buffer }));
  }
  drop(code: number) {
    act(() => {
      this.readyState = 3;
      this.onclose?.({ code, reason: "" });
    });
  }
}

const terminalSockets = () => FakeSocket.all.filter((s) => s.url.startsWith("wss://sessions.test/terminal"));

beforeEach(() => {
  FakeSocket.all = [];
  layout.cols = 80;
  layout.rows = 24;
  vi.stubGlobal("WebSocket", FakeSocket);
  signIn({ token: "t", user: ana });
});
afterEach(() => {
  vi.unstubAllGlobals();
});

function SandboxMarker() {
  return <p>sandbox page for {useParams().s}</p>;
}

describe("create sandbox", () => {
  function renderModal() {
    const onClose = vi.fn();
    render(
      <MemoryRouter initialEntries={["/orgs/acme"]}>
        <Routes>
          <Route path="/orgs/acme" element={<CreateSandboxModal org="acme" onClose={onClose} />} />
          <Route path="/sandboxes/:s" element={<SandboxMarker />} />
        </Routes>
      </MemoryRouter>,
    );
    return { onClose, slug: screen.getByLabelText("Sandbox name"), submit: screen.getByRole("button", { name: "Create sandbox" }) };
  }
  const hosts = () => within(screen.getByRole("list", { name: "Allowed hosts" })).getAllByRole("listitem").map((li) => li.textContent);

  it("prefills the default egress, no port and 8 hours, and refuses bad values before sending", async () => {
    const user = userEvent.setup();
    const create = vi.spyOn(api, "createSandbox");
    const { slug, submit } = renderModal();
    expect(hosts()).toEqual([...DEFAULT_SANDBOX_EGRESS]);
    expect(screen.getByLabelText("Public port")).toHaveValue("");
    expect(screen.getByLabelText("Auto-stop after (hours)")).toHaveValue(8);
    expect(submit).toBeDisabled();

    await user.type(slug, "shop-");
    expect(screen.getByText("Do not end with -.")).toBeInTheDocument();
    expect(submit).toBeDisabled();
    await user.clear(slug);
    await user.type(slug, "scratch");

    await user.type(screen.getByLabelText("Public port"), "70000");
    const hours = screen.getByLabelText("Auto-stop after (hours)");
    await user.clear(hours);
    await user.type(hours, "73");
    await user.click(submit);
    expect(screen.getByText("Use a port from 1 to 65535, or leave it empty.")).toBeInTheDocument();
    expect(screen.getByText("Use 1 to 72 hours.")).toBeInTheDocument();
    expect(create).not.toHaveBeenCalled();

    // Hosts follow the manifest's hostname rule; a bad one is not added.
    await user.type(screen.getByLabelText("Host to allow"), "localhost{Enter}");
    expect(screen.getByText(/`localhost` is not a host/)).toBeInTheDocument();
    expect(hosts()).toHaveLength(DEFAULT_SANDBOX_EGRESS.length);
    expect(create).not.toHaveBeenCalled();
  });

  it("sends the slug and the edited settings, then opens the sandbox", async () => {
    const user = userEvent.setup();
    const create = vi.spyOn(api, "createSandbox").mockResolvedValue(sandboxOf({ state: "starting" }));
    const { slug, submit, onClose } = renderModal();
    await user.type(slug, "Scratch");
    expect(slug).toHaveValue("scratch");
    await user.click(screen.getByRole("button", { name: "Remove api.openai.com" }));
    const host = screen.getByLabelText("Host to allow");
    await user.type(host, "*.Example.com");
    await user.click(screen.getByRole("button", { name: "Add host" }));
    expect(host).toHaveValue("");
    // Adding a host already listed keeps one copy.
    await user.type(host, "pypi.org{Enter}");
    await user.type(screen.getByLabelText("Public port"), "3000");
    const hours = screen.getByLabelText("Auto-stop after (hours)");
    await user.clear(hours);
    await user.type(hours, "72");
    await user.click(submit);

    expect(create).toHaveBeenCalledWith("acme", {
      slug: "scratch",
      settings: { egress: [...DEFAULT_SANDBOX_EGRESS.filter((h) => h !== "api.openai.com"), "*.example.com"], port: 3000, auto_stop_hours: 72 },
    });
    expect(await screen.findByText("sandbox page for scratch")).toBeInTheDocument();
    expect(onClose).toHaveBeenCalled();
  });

  it("sends no port when the field is left empty", async () => {
    const user = userEvent.setup();
    const create = vi.spyOn(api, "createSandbox").mockResolvedValue(sandboxOf());
    const { slug, submit } = renderModal();
    await user.type(slug, "scratch");
    await user.click(submit);
    expect(create).toHaveBeenCalledWith("acme", { slug: "scratch", settings: { egress: [...DEFAULT_SANDBOX_EGRESS], port: null, auto_stop_hours: 8 } });
  });
});

describe("sandboxes in the shell", () => {
  function renderApp(path: string, sandboxes: Sandbox[]) {
    vi.spyOn(api, "me").mockResolvedValue({ user: ana, orgs: [{ org: "acme", owner: true }] });
    vi.spyOn(api, "sandboxes").mockResolvedValue(sandboxes);
    render(
      <MemoryRouter initialEntries={[path]}>
        <App />
      </MemoryRouter>,
    );
  }

  it("lists the org's sandboxes with their state, creator and auto-stop", async () => {
    renderApp("/orgs/acme/sandboxes", [sandboxOf(), sandboxOf({ slug: "old", state: "stopped", url: null, stops_at: null, settings: { egress: [], port: null, auto_stop_hours: 4 } })]);
    const list = await screen.findByRole("region", { name: "Sandboxes" });
    const rows = await within(list).findAllByRole("listitem");
    expect(rows.map((r) => r.querySelector(".project-name")?.textContent)).toEqual(["scratch", "old"]);
    expect(rows[0]).toHaveTextContent("running");
    expect(rows[0]).toHaveTextContent("by Ana");
    expect(rows[0]).toHaveTextContent("stops in 7 hours");
    expect(within(rows[0]!).getByRole("link", { name: /Open/ })).toHaveAttribute("href", "https://scratch.preview.test");
    expect(rows[1]).toHaveTextContent("stopped");
    expect(rows[1]).toHaveTextContent("auto-stops 4 h after start");
    expect(screen.getByRole("link", { name: "Sandboxes" })).toHaveClass("active");
  });

  it("explains sandboxes when there are none, and creates one from the empty state or the Create menu", async () => {
    const user = userEvent.setup();
    renderApp("/orgs/acme/sandboxes", []);
    expect(await screen.findByText("You don't have any sandboxes yet")).toBeInTheDocument();
    expect(screen.getByText(/A sandbox is a VM with aho, for coding without deploying/)).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /Create a sandbox/ }));
    expect(screen.getByRole("dialog", { name: "New sandbox" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Cancel" }));
    await user.click(screen.getByRole("button", { name: "Create" }));
    await user.click(screen.getByRole("menuitem", { name: /New sandbox/ }));
    expect(screen.getByRole("dialog", { name: "New sandbox" })).toBeInTheDocument();
  });
});

describe("sandbox page", () => {
  let current: Sandbox;
  beforeEach(() => {
    current = sandboxOf();
    vi.spyOn(api, "sandbox").mockImplementation(async () => current);
    vi.spyOn(api, "sandboxTerminal").mockResolvedValue({ url: "wss://sessions.test/terminal", token: "tok" });
    vi.spyOn(api, "sandboxChat").mockResolvedValue({ rpc_url: "wss://sessions.test/rpc/s1", token: "chat", session_id: null });
    vi.spyOn(api, "sandboxFiles").mockResolvedValue({ files: [{ path: "main.py", kind: "file", size: 12 }], truncated: false });
    vi.spyOn(api, "sandboxEnv").mockResolvedValue([]);
  });

  function renderPage() {
    render(
      <MemoryRouter initialEntries={["/sandboxes/scratch"]}>
        <Routes>
          <Route path="/sandboxes/:s" element={<SandboxPage pollMs={5} />} />
          <Route path="/orgs/:org/sandboxes" element={<p>sandboxes list</p>} />
        </Routes>
      </MemoryRouter>,
    );
  }

  it("shows a running sandbox with Stop, its URL and four tabs; the terminal stays connected across tabs", async () => {
    const user = userEvent.setup();
    renderPage();
    expect(await screen.findByRole("heading", { name: "scratch" })).toBeInTheDocument();
    expect(screen.getByText("running")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /scratch\.preview\.test/ })).toHaveAttribute("href", "https://scratch.preview.test");
    expect(screen.getByRole("button", { name: /Stop/ })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /^Start/ })).toBeNull();
    expect(screen.getAllByRole("tab").map((t) => t.textContent)).toEqual(["Terminal", "Chat", "Files", "Settings"]);
    expect(screen.getByRole("tab", { name: "Terminal" })).toHaveAttribute("aria-selected", "true");
    await waitFor(() => expect(api.sandboxTerminal).toHaveBeenCalledWith("scratch", { cols: 80, rows: 24, command: null, no_pty: false }));

    await user.click(screen.getByRole("tab", { name: "Chat" }));
    expect(await screen.findByRole("region", { name: "Chat with aho" })).toBeInTheDocument();
    expect(api.sandboxChat).toHaveBeenCalledWith("scratch", undefined);
    // The sandbox chat has no deploy flow: no manifest-draft wording in its empty state.
    expect(screen.getByText(/this sandbox's \/workspace/)).toBeInTheDocument();

    await user.click(screen.getByRole("tab", { name: "Files" }));
    expect(await screen.findByRole("button", { name: "main.py" })).toBeInTheDocument();
    expect(api.sandboxFiles).toHaveBeenCalledWith("scratch");

    await user.click(screen.getByRole("tab", { name: "Settings" }));
    expect(await screen.findByText("No variables set.")).toBeInTheDocument();

    await user.click(screen.getByRole("tab", { name: "Terminal" }));
    expect(api.sandboxTerminal).toHaveBeenCalledTimes(1);
    expect(api.sandboxChat).toHaveBeenCalledTimes(1);
    expect(terminalSockets().every((s) => s.closedWith === null)).toBe(true);
  });

  it("offers Start instead of a terminal while stopped, and connects once it runs", async () => {
    const user = userEvent.setup();
    current = sandboxOf({ state: "stopped", stops_at: null, url: null });
    const start = vi.spyOn(api, "startSandbox").mockImplementation(async () => {
      current = sandboxOf({ state: "starting" });
      return current;
    });
    renderPage();
    expect(await screen.findByText("The sandbox is stopped.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Stop/ })).toBeNull();
    expect(screen.getAllByRole("button", { name: /^Start/ })).toHaveLength(2);
    expect(api.sandboxTerminal).not.toHaveBeenCalled();

    await user.click(within(screen.getByRole("tabpanel")).getByRole("button", { name: /Start/ }));
    expect(start).toHaveBeenCalledWith("scratch");
    expect(await screen.findByText("The sandbox is starting…")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Stop/ })).toBeInTheDocument();
    // Followed while starting; the terminal connects once it runs.
    current = sandboxOf();
    await waitFor(() => expect(api.sandboxTerminal).toHaveBeenCalledTimes(1));
  });

  it("shows why a sandbox failed and lets it be started again", async () => {
    current = sandboxOf({ state: "failed", error: "image pull failed", stops_at: null });
    renderPage();
    expect(await screen.findByText(/image pull failed/)).toBeInTheDocument();
    expect(screen.getByText("The sandbox is failed.")).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: /^Start/ })).toHaveLength(2);
  });

  it("stops a running sandbox", async () => {
    const user = userEvent.setup();
    const stop = vi.spyOn(api, "stopSandbox").mockImplementation(async () => {
      current = sandboxOf({ state: "stopped", stops_at: null });
      return current;
    });
    renderPage();
    await user.click(await screen.findByRole("button", { name: /Stop/ }));
    expect(stop).toHaveBeenCalledWith("scratch");
    expect(await screen.findByText("The sandbox is stopped.")).toBeInTheDocument();
    // Leaving the running state closed the terminal session.
    expect(terminalSockets().every((s) => s.closedWith !== null)).toBe(true);
  });

  it("deletes only after confirming, then returns to the org's sandboxes", async () => {
    const user = userEvent.setup();
    const remove = vi.spyOn(api, "deleteSandbox").mockResolvedValue(undefined);
    renderPage();
    await user.click(await screen.findByRole("button", { name: "Delete" }));
    const dialog = screen.getByRole("dialog", { name: "Delete scratch?" });
    await user.click(within(dialog).getByRole("button", { name: "Cancel" }));
    expect(remove).not.toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Delete" }));
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Delete sandbox" }));
    expect(remove).toHaveBeenCalledWith("scratch");
    expect(await screen.findByText("sandboxes list")).toBeInTheDocument();
  });
});

describe("sandbox settings", () => {
  it("saves checked settings and warns that a running sandbox restarts", async () => {
    const user = userEvent.setup();
    vi.spyOn(api, "sandboxEnv").mockResolvedValue([]);
    const save = vi.spyOn(api, "setSandboxSettings").mockResolvedValue(sandboxOf());
    const onChanged = vi.fn();
    render(<SandboxSettingsPanel sandbox={sandboxOf({ settings: { egress: ["pypi.org"], port: 3000, auto_stop_hours: 8 } })} onChanged={onChanged} />);
    expect(screen.getByText(/Saving restarts the running sandbox/)).toBeInTheDocument();
    expect(screen.getByLabelText("Public port")).toHaveValue("3000");
    const port = screen.getByLabelText("Public port");
    await user.clear(port);
    await user.type(port, "0");
    await user.click(screen.getByRole("button", { name: "Save settings" }));
    expect(screen.getByText("Use a port from 1 to 65535, or leave it empty.")).toBeInTheDocument();
    expect(save).not.toHaveBeenCalled();

    await user.clear(port);
    await user.click(screen.getByRole("button", { name: "Remove pypi.org" }));
    await user.click(screen.getByRole("button", { name: "Save settings" }));
    expect(save).toHaveBeenCalledWith("scratch", { egress: [], port: null, auto_stop_hours: 8 });
    expect(await screen.findByText("Saved.")).toBeInTheDocument();
    expect(onChanged).toHaveBeenCalled();
  });

  it("adds a variable without ever rendering its value back, and deletes one", async () => {
    const user = userEvent.setup();
    let vars: EnvVarView[] = [{ name: "OLD_TOKEN", fingerprint: "blake3:0123456789abcdef" }];
    vi.spyOn(api, "sandboxEnv").mockImplementation(async () => vars);
    const set = vi.spyOn(api, "setSandboxEnv").mockImplementation(async (_s, { name }) => {
      const view = { name, fingerprint: "blake3:feedfacecafe0000" };
      vars = [...vars, view];
      return view;
    });
    const del = vi.spyOn(api, "deleteSandboxEnv").mockImplementation(async (_s, name) => {
      vars = vars.filter((v) => v.name !== name);
    });
    render(<SandboxSettingsPanel sandbox={sandboxOf({ state: "stopped" })} onChanged={vi.fn()} />);
    const env = screen.getByRole("region", { name: "Environment variables" });
    expect(within(env).getByText(/ANTHROPIC_API_KEY/)).toBeInTheDocument();
    expect(within(env).getByText(/next start/)).toBeInTheDocument();
    expect(await within(env).findByText("OLD_TOKEN")).toBeInTheDocument();

    const value = within(env).getByLabelText("Value");
    expect(value).toHaveAttribute("type", "password");
    await user.type(within(env).getByLabelText("Name"), "anthropic_api_key");
    expect(within(env).getByLabelText("Name")).toHaveValue("ANTHROPIC_API_KEY");
    await user.type(value, "sk-ant-secret-42");
    await user.click(within(env).getByRole("button", { name: "Set variable" }));
    expect(set).toHaveBeenCalledWith("scratch", { name: "ANTHROPIC_API_KEY", value: "sk-ant-secret-42" });
    expect(await within(env).findByRole("cell", { name: "ANTHROPIC_API_KEY" })).toBeInTheDocument();
    expect(value).toHaveValue("");
    expect(document.body.innerHTML).not.toContain("sk-ant-secret-42");

    await user.click(within(env).getByRole("button", { name: "Delete OLD_TOKEN" }));
    expect(del).toHaveBeenCalledWith("scratch", "OLD_TOKEN");
    await waitFor(() => expect(within(env).queryByText("OLD_TOKEN")).toBeNull());
  });

  it("refuses a variable name the server would refuse", async () => {
    const user = userEvent.setup();
    vi.spyOn(api, "sandboxEnv").mockResolvedValue([]);
    const set = vi.spyOn(api, "setSandboxEnv");
    render(<SandboxSettingsPanel sandbox={sandboxOf()} onChanged={vi.fn()} />);
    const env = screen.getByRole("region", { name: "Environment variables" });
    await user.type(within(env).getByLabelText("Name"), "1KEY");
    await user.type(within(env).getByLabelText("Value"), "v");
    expect(within(env).getByText("Use capital letters, digits and _, starting with a letter.")).toBeInTheDocument();
    expect(within(env).getByRole("button", { name: "Set variable" })).toBeDisabled();
    expect(set).not.toHaveBeenCalled();
  });
});

describe("sandbox terminal", () => {
  const enc = (s: string) => [...new TextEncoder().encode(s)];

  async function connected() {
    const access = vi.spyOn(api, "sandboxTerminal").mockResolvedValue({ url: "wss://sessions.test/terminal/abc", token: "t/1" });
    render(<SandboxTerminal sandbox="scratch" />);
    await waitFor(() => expect(terminalSockets()).toHaveLength(1));
    const socket = terminalSockets()[0]!;
    socket.open();
    return { access, socket, term: terminals.at(-1)! };
  }

  it("asks for the fitted size, opens the token URL as a binary socket, and sends keystrokes and resizes as frames", async () => {
    const { access, socket, term } = await connected();
    expect(access).toHaveBeenCalledWith("scratch", { cols: 80, rows: 24, command: null, no_pty: false });
    expect(socket.url).toBe("wss://sessions.test/terminal/abc?token=t%2F1");
    expect(socket.binaryType).toBe("arraybuffer");
    expect(screen.getByText("connected")).toBeInTheDocument();

    act(() => term.type("ls\r"));
    expect(socket.sent).toEqual([[0, ...enc("ls\r")]]);

    layout.cols = 100;
    layout.rows = 30;
    fireEvent(window, new Event("resize"));
    expect(socket.sent.at(-1)).toEqual([3, 0, 100, 0, 30]);

    socket.receive(encodeFrame({ kind: "stdout", data: new TextEncoder().encode("hello ") }));
    socket.receive(encodeFrame({ kind: "stderr", data: new TextEncoder().encode("world") }));
    expect(term.text).toBe("hello world");
  });

  it("shows the exit code and reconnects only when asked", async () => {
    const user = userEvent.setup();
    const { access, socket } = await connected();
    socket.receive(encodeFrame({ kind: "exit", code: 130 }));
    expect(screen.getByText("[process exited with code 130]")).toBeInTheDocument();
    socket.drop(1000);
    // Still the exit, not a drop; and nothing reconnects on its own.
    expect(screen.getByText("[process exited with code 130]")).toBeInTheDocument();
    const later = Promise.withResolvers<void>();
    setTimeout(later.resolve, 30);
    await act(() => later.promise);
    expect(access).toHaveBeenCalledTimes(1);

    await user.click(screen.getByRole("button", { name: "Reconnect" }));
    await waitFor(() => expect(terminalSockets()).toHaveLength(2));
    expect(access).toHaveBeenCalledTimes(2);
    terminalSockets()[1]!.open();
    expect(screen.queryByText("[process exited with code 130]")).toBeNull();
    expect(screen.queryByRole("button", { name: "Reconnect" })).toBeNull();
  });

  it("shows the server's error frame with Reconnect", async () => {
    const { socket } = await connected();
    socket.receive(encodeFrame({ kind: "error", message: "sandbox is not running" }));
    expect(screen.getByRole("alert")).toHaveTextContent("sandbox is not running");
    expect(screen.getByRole("button", { name: "Reconnect" })).toBeInTheDocument();
  });

  it("says when the socket drops without an exit, with Reconnect", async () => {
    const { socket } = await connected();
    socket.drop(1006);
    expect(screen.getByText("The connection closed (1006).")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Reconnect" })).toBeInTheDocument();
  });

  it("does not send input before the socket opens or after it closes, and closes it on unmount", async () => {
    vi.spyOn(api, "sandboxTerminal").mockResolvedValue({ url: "wss://sessions.test/terminal", token: "t" });
    const { unmount } = render(<SandboxTerminal sandbox="scratch" />);
    await waitFor(() => expect(terminalSockets()).toHaveLength(1));
    const socket = terminalSockets()[0]!;
    const term = terminals.at(-1)!;
    act(() => term.type("early"));
    expect(socket.sent).toEqual([]);
    socket.open();
    unmount();
    expect(socket.closedWith).toBe(1000);
    expect(term.disposed).toBe(true);
  });
});
