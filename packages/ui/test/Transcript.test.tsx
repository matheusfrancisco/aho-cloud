import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { Transcript, type TranscriptMessage } from "../src/Transcript";

const rows = () => within(screen.getByRole("list", { name: "transcript" })).getAllByRole("listitem");

describe("transcript", () => {
  it("renders prompts and replies with who said them, and every block of a reply", () => {
    const messages: TranscriptMessage[] = [
      { kind: "user", id: "1", text: "Fix the login bug" },
      {
        kind: "assistant",
        id: "2",
        blocks: [
          { type: "thinking", text: "the cookie is not set" },
          { type: "text", text: "Found it." },
          { type: "image", mediaType: "image/png" },
        ],
      },
    ];
    render(<Transcript messages={messages} assistantLabel="mage" />);
    const [user, assistant] = rows();
    expect(user).toHaveClass("msg", "user");
    expect(within(user!).getByText("you")).toHaveClass("who");
    expect(within(user!).getByText("Fix the login bug")).toBeInTheDocument();
    expect(assistant).toHaveClass("msg", "assistant");
    expect(within(assistant!).getByText("mage")).toHaveClass("who");
    expect(within(assistant!).getByText("Found it.")).toBeInTheDocument();
    expect(within(assistant!).getByText("[image image/png]")).toBeInTheDocument();
    // Thinking is collapsed behind its summary.
    const thinking = within(assistant!).getByText("thinking").closest("details")!;
    expect(thinking).not.toHaveAttribute("open");
    expect(within(thinking).getByText("the cookie is not set")).not.toBeVisible();
  });

  it("collapses a tool call's input and a result's output behind one-line summaries", async () => {
    const user = userEvent.setup();
    const messages: TranscriptMessage[] = [
      { kind: "tool_call", id: "1", tool: "bash", label: "exec", input: '{\n  "cmd": "npm test"\n}' },
      { kind: "tool_result", id: "2", outcome: "ok", summary: "ok", output: "12 passed" },
      { kind: "tool_call", id: "3", tool: "read", input: "{}" },
      { kind: "tool_result", id: "4", outcome: "err", summary: "error 2: no such file", output: "ENOENT" },
    ];
    render(<Transcript messages={messages} assistantLabel="aho" />);
    const [call, ok, bare, err] = rows();

    expect(call).toHaveClass("msg", "tool");
    expect(call!.querySelector("summary")).toHaveTextContent("bash exec");
    const input = within(call!).getByText(/npm test/);
    expect(input).not.toBeVisible();
    await user.click(call!.querySelector("summary")!);
    expect(input).toBeVisible();

    expect(ok).toHaveClass("msg", "tool-result", "ok");
    expect(ok!.querySelector("summary")).toHaveTextContent("result: ok");
    expect(within(ok!).getByText("12 passed")).not.toBeVisible();

    // No label: just the tool's name.
    expect(bare!.querySelector("summary")!.textContent).toBe("read");

    expect(err).toHaveClass("msg", "tool-result", "err");
    expect(err!.querySelector("summary")).toHaveTextContent("result: error 2: no such file");
  });

  it("styles notices by tone and renders their content as given", () => {
    const messages: TranscriptMessage[] = [
      { kind: "notice", id: "1", tone: "note", content: "approval granted (once)" },
      { kind: "notice", id: "2", tone: "error", content: "model request failed: overloaded" },
      {
        kind: "notice",
        id: "3",
        tone: "highlight",
        content: (
          <>
            <strong>Fix ready</strong> <button type="button">Open PR</button>
          </>
        ),
      },
    ];
    render(<Transcript messages={messages} assistantLabel="aho" />);
    const [note, error, highlight] = rows();
    expect(note!.className).toBe("msg note");
    expect(note).toHaveTextContent("approval granted (once)");
    expect(error!.className).toBe("msg note error");
    expect(highlight!.className).toBe("msg highlight");
    expect(within(highlight!).getByRole("button", { name: "Open PR" })).toBeInTheDocument();
  });

  it("shows the reply being streamed after the messages, and nothing when it is empty", () => {
    const messages: TranscriptMessage[] = [{ kind: "user", id: "1", text: "hi" }];
    const { rerender } = render(<Transcript messages={messages} streaming="" assistantLabel="aho" userLabel="Ana" />);
    expect(rows()).toHaveLength(1);
    expect(within(rows()[0]!).getByText("Ana")).toBeInTheDocument();
    rerender(<Transcript messages={messages} streaming="Hel" assistantLabel="aho" />);
    const last = rows().at(-1)!;
    expect(rows()).toHaveLength(2);
    expect(last).toHaveClass("msg", "assistant", "streaming");
    expect(last).toHaveTextContent("ahoHel");
  });
});
