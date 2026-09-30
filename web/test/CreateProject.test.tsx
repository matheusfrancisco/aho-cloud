import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter, Route, Routes, useParams } from "react-router";
import { describe, expect, it, vi } from "vitest";
import { CreateProjectModal } from "../src/components/CreateProject";
import { api, ApiError } from "../src/lib/api";
import { slugProblem } from "../src/lib/slug";

describe("slug rule", () => {
  it("accepts 1–40 lowercase letters, digits and -, starting with a letter and not ending in -", () => {
    for (const ok of ["a", "shop", "shop-2", "a1-b2-c3", "x".repeat(40)]) expect(slugProblem(ok), ok).toBeNull();
  });

  it("says what is wrong with each kind of bad slug", () => {
    expect(slugProblem("")).toBe("Enter a name.");
    expect(slugProblem("x".repeat(41))).toBe("Use at most 40 characters.");
    expect(slugProblem("2shop")).toBe("Start with a lowercase letter.");
    expect(slugProblem("-shop")).toBe("Start with a lowercase letter.");
    expect(slugProblem("Shop")).toBe("Start with a lowercase letter.");
    expect(slugProblem("my_shop")).toBe("Use only lowercase letters, digits and -.");
    expect(slugProblem("my shop")).toBe("Use only lowercase letters, digits and -.");
    expect(slugProblem("shöp")).toBe("Use only lowercase letters, digits and -.");
    expect(slugProblem("shop-")).toBe("Do not end with -.");
  });
});

function ProjectMarker() {
  return <p>project page for {useParams().p}</p>;
}

function renderModal(onClose = vi.fn()) {
  render(
    <MemoryRouter initialEntries={["/orgs/acme"]}>
      <Routes>
        <Route path="/orgs/acme" element={<CreateProjectModal org="acme" onClose={onClose} />} />
        <Route path="/projects/:p" element={<ProjectMarker />} />
      </Routes>
    </MemoryRouter>,
  );
  return { onClose, input: screen.getByLabelText("Project name"), submit: screen.getByRole("button", { name: "Create project" }) };
}

describe("create project modal", () => {
  it("keeps submit disabled and explains the rule while the slug is invalid", async () => {
    const user = userEvent.setup();
    const create = vi.spyOn(api, "createProject");
    const { input, submit } = renderModal();
    expect(submit).toBeDisabled();

    await user.type(input, "9lives");
    expect(screen.getByText("Start with a lowercase letter.")).toBeInTheDocument();
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(submit).toBeDisabled();

    await user.clear(input);
    await user.type(input, "shop-");
    expect(screen.getByText("Do not end with -.")).toBeInTheDocument();
    await user.type(input, "{Enter}");
    expect(create).not.toHaveBeenCalled();

    await user.clear(input);
    await user.type(input, "my_shop");
    expect(screen.getByText("Use only lowercase letters, digits and -.")).toBeInTheDocument();
    expect(create).not.toHaveBeenCalled();
  });

  it("lowercases input, creates the project in the org and opens it", async () => {
    const user = userEvent.setup();
    const create = vi.spyOn(api, "createProject").mockResolvedValue({ slug: "shop", org: "acme", active: [], domains: [] });
    const { input, submit, onClose } = renderModal();
    await user.type(input, "Shop");
    expect(input).toHaveValue("shop");
    expect(screen.getByText(/1–40 lowercase letters/)).toBeInTheDocument();
    expect(submit).toBeEnabled();
    await user.click(submit);
    expect(create).toHaveBeenCalledWith("acme", { slug: "shop" });
    expect(await screen.findByText("project page for shop")).toBeInTheDocument();
    expect(onClose).toHaveBeenCalled();
  });

  it("shows the server's refusal and stays open", async () => {
    const user = userEvent.setup();
    vi.spyOn(api, "createProject").mockRejectedValue(new ApiError(409, "conflict", "project shop already exists"));
    const { input, submit, onClose } = renderModal();
    await user.type(input, "shop");
    await user.click(submit);
    expect(await screen.findByRole("alert")).toHaveTextContent("conflict: project shop already exists");
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByRole("dialog", { name: "New project" })).toBeInTheDocument();
  });
});
