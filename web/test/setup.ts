import "@testing-library/jest-dom/vitest";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";
import { signOut } from "../src/lib/session";

afterEach(() => {
  cleanup();
  signOut();
  localStorage.clear();
});
