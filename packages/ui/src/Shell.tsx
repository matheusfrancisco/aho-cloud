// The app frame: a top bar with the brand and the account menu, a sidebar, and the content. The
// product fills the slots (links, org switcher, menus); the frame owns the layout only.
import type { ReactNode } from "react";
import { ChevronDown, UserIcon } from "./icons";
import { Dropdown } from "./overlay";

/** The signed-in frame. `account` goes on the right of the top bar; `sidebar` fills the sidebar. */
export function Shell({ brand, account, sidebar, children }: { brand: ReactNode; account?: ReactNode; sidebar: ReactNode; children: ReactNode }) {
  return (
    <>
      <header className="topbar">
        {brand}
        <div className="topbar-right">{account}</div>
      </header>
      <div className="layout">
        <aside className="sidebar">{sidebar}</aside>
        <main className="content">{children}</main>
      </div>
    </>
  );
}

/** The signed-out frame: the brand and the page. */
export function PublicShell({ brand, children }: { brand: ReactNode; children: ReactNode }) {
  return (
    <>
      <header className="topbar">{brand}</header>
      <main className="public">{children}</main>
    </>
  );
}

/** The top bar's account menu: who is signed in, then the product's items (`children` gets `close`). */
export function AccountMenu({ user, children }: { user: { name: string; email: string }; children: (close: () => void) => ReactNode }) {
  return (
    <Dropdown
      buttonClass="topbar-button"
      align="right"
      label={
        <>
          <UserIcon /> Account <ChevronDown />
        </>
      }
    >
      {(close) => (
        <>
          <div className="menu-head">
            <strong>{user.name}</strong>
            <span className="muted">{user.email}</span>
          </div>
          {children(close)}
        </>
      )}
    </Dropdown>
  );
}
