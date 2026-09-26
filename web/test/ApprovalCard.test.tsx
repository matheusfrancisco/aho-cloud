import { render, screen, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { ApprovalCard } from "../src/components/ApprovalCard";
import { groupChanges } from "../src/lib/release";
import { release } from "./fixtures";

const diff = {
  changes: [
    { category: "code" as const, direction: "neutral" as const, detail: "source or build changed" },
    { category: "egress" as const, direction: "widen" as const, detail: "egress + api.stripe.com" },
    { category: "volume" as const, direction: "narrow" as const, detail: "volume - web:/data" },
    { category: "domain" as const, direction: "widen" as const, detail: "domain + shop.example.com" },
  ],
};

const ana = { id: "u-ana", email: "ana@example.com", name: "Ana" };
const bo = { id: "u-bo", email: "bo@example.com", name: "Bo" };

describe("approval card", () => {
  it("groups changes by direction, keeping diff order within a group", () => {
    const g = groupChanges(diff);
    expect(g.widen.map((c) => c.detail)).toEqual(["egress + api.stripe.com", "domain + shop.example.com"]);
    expect(g.narrow.map((c) => c.category)).toEqual(["volume"]);
    expect(g.neutral.map((c) => c.category)).toEqual(["code"]);
  });

  it("highlights widenings apart from narrowing and neutral changes, and shows the owner requirement", () => {
    const r = release({ diff, requirement: { approvals: 2, owner_required: true }, approvals: [{ stage: "live", user: bo, owner: false }] });
    const { container } = render(<ApprovalCard release={r} stage="live" />);

    const widen = container.querySelector('[data-direction="widen"]') as HTMLElement;
    const widenItems = within(widen).getAllByRole("listitem");
    expect(widenItems.map((li) => li.textContent)).toEqual(["egress egress + api.stripe.com", "domain domain + shop.example.com"]);
    for (const li of widenItems) expect(li).toHaveClass("change-widen");
    expect(within(container.querySelector('[data-direction="narrow"]') as HTMLElement).getByText(/volume - web:\/data/)).toHaveClass("change-narrow");
    expect(within(container.querySelector('[data-direction="neutral"]') as HTMLElement).getByText(/source or build changed/)).toHaveClass("change-neutral");
    // Widenings are listed before anything else on the card.
    const groups = [...container.querySelectorAll("[data-direction]")].map((el) => el.getAttribute("data-direction"));
    expect(groups).toEqual(["widen", "narrow", "neutral"]);

    const requirement = container.querySelector(".requirement") as HTMLElement;
    expect(requirement).toHaveTextContent("1 of 2 approvals");
    expect(within(requirement).getByText(/Owner approval required \(missing\)/)).toHaveClass("unmet");
  });

  it("counts only approvals for the target stage, each approver once, and marks the owner present", () => {
    const r = release({
      diff,
      requirement: { approvals: 2, owner_required: true },
      approvals: [
        { stage: "staging", user: bo, owner: false },
        { stage: "live", user: ana, owner: true },
        { stage: "live", user: ana, owner: true },
        { stage: "live", user: bo, owner: false },
      ],
    });
    const { container } = render(<ApprovalCard release={r} stage="live" />);
    const requirement = container.querySelector(".requirement") as HTMLElement;
    expect(requirement).toHaveTextContent("2 of 2 approvals");
    expect(within(requirement).getByText(/Owner approval required \(present\)/)).toHaveClass("met");
    expect(within(requirement).getByText("met")).toBeInTheDocument();
  });

  it("does not ask for an owner when the requirement does not", () => {
    render(<ApprovalCard release={release({ diff: { changes: [diff.changes[0]!] }, requirement: { approvals: 1, owner_required: false } })} stage="live" />);
    expect(screen.queryByText(/Owner approval required/)).toBeNull();
    expect(screen.queryByText("Widens")).toBeNull();
  });

  it("shows taint findings with rotate guidance and undeclared egress as blockers", () => {
    render(
      <ApprovalCard
        release={release({ status: "tainted", taint: [{ secret: "STRIPE_KEY", location: "aho-log:5f3a#c0ffee" }], undeclared_egress: ["evil.example"] })}
        stage="live"
      />,
    );
    expect(screen.getByText("Tainted secrets")).toBeInTheDocument();
    expect(screen.getByText("STRIPE_KEY")).toBeInTheDocument();
    expect(screen.getByText("aho-log:5f3a#c0ffee")).toBeInTheDocument();
    expect(screen.getByText(/revoke the credential at its/)).toBeInTheDocument();
    expect(screen.getByText("evil.example")).toBeInTheDocument();
  });
});
