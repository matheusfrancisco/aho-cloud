import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { AccountMenu, PublicShell, Shell } from "../src/Shell";

describe("shell", () => {
  it("puts the brand and account in the top bar, the sidebar beside the content", () => {
    render(
      <Shell brand={<a href="/">mage</a>} account={<button type="button">Account</button>} sidebar={<nav aria-label="Pages">Tickets</nav>}>
        <h1>Ticket 42</h1>
      </Shell>,
    );
    const banner = screen.getByRole("banner");
    expect(within(banner).getByRole("link", { name: "mage" })).toBeInTheDocument();
    expect(within(banner).getByRole("button", { name: "Account" }).parentElement).toHaveClass("topbar-right");
    const sidebar = screen.getByRole("complementary");
    expect(within(sidebar).getByRole("navigation", { name: "Pages" })).toBeInTheDocument();
    const main = screen.getByRole("main");
    expect(main).toHaveClass("content");
    expect(within(main).getByRole("heading", { name: "Ticket 42" })).toBeInTheDocument();
    // Sidebar and content share the layout row; the content is not inside the sidebar.
    expect(sidebar.parentElement).toBe(main.parentElement);
    expect(sidebar.parentElement).toHaveClass("layout");
  });

  it("signed out: only the brand and the page, no sidebar", () => {
    render(
      <PublicShell brand={<span>aho cloud</span>}>
        <p>Sign in</p>
      </PublicShell>,
    );
    expect(within(screen.getByRole("banner")).getByText("aho cloud")).toBeInTheDocument();
    expect(screen.getByRole("main")).toHaveClass("public");
    expect(within(screen.getByRole("main")).getByText("Sign in")).toBeInTheDocument();
    expect(screen.queryByRole("complementary")).toBeNull();
  });

  it("account menu: shows who is signed in above the product's items, which can close it", async () => {
    const user = userEvent.setup();
    const logOut = vi.fn();
    render(
      <AccountMenu user={{ name: "Ana", email: "ana@example.com" }}>
        {(close) => (
          <button
            type="button"
            role="menuitem"
            onClick={() => {
              close();
              logOut();
            }}
          >
            Log out
          </button>
        )}
      </AccountMenu>,
    );
    const toggle = screen.getByRole("button", { name: "Account" });
    expect(screen.queryByRole("menu")).toBeNull();
    await user.click(toggle);
    const menu = screen.getByRole("menu");
    expect(within(menu).getByText("Ana")).toBeInTheDocument();
    expect(within(menu).getByText("ana@example.com")).toBeInTheDocument();
    await user.click(within(menu).getByRole("menuitem", { name: "Log out" }));
    expect(logOut).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("menu")).toBeNull();
  });
});
