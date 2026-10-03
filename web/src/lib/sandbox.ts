// Sandbox settings as the console edits them, checked with the server's rules before sending.
import type { SandboxSettings } from "../gen/api";

/**
 * Hosts a new sandbox may reach unless the form names its own list. Mirrors
 * `cloud_api::DEFAULT_SANDBOX_EGRESS` in `crates/api/src/lib.rs` (constants are not generated);
 * keep the two in step.
 */
export const DEFAULT_SANDBOX_EGRESS: readonly string[] = [
  "api.anthropic.com",
  "api.openai.com",
  "registry.npmjs.org",
  "pypi.org",
  "files.pythonhosted.org",
  "crates.io",
  "index.crates.io",
  "static.crates.io",
  "github.com",
  "*.github.com",
  "*.githubusercontent.com",
];

/** `SANDBOX_AUTO_STOP_DEFAULT_HOURS` / `SANDBOX_AUTO_STOP_MAX_HOURS` in `crates/api/src/lib.rs`. */
export const AUTO_STOP_DEFAULT_HOURS = 8;
export const AUTO_STOP_MAX_HOURS = 72;

/** Why `host` is not an egress entry (`cloud_contract::validate::is_hostname` with a wildcard), or `null`. */
export function egressProblem(host: string): string | null {
  const body = host.startsWith("*.") ? host.slice(2) : host;
  const labels = body.split(".");
  const ok =
    body.length <= 253 &&
    labels.length >= 2 &&
    labels.every((l) => l.length >= 1 && l.length <= 63 && /^[a-z0-9-]+$/.test(l) && !l.startsWith("-") && !l.endsWith("-"));
  return ok ? null : `\`${host}\` is not a host: use a lowercase name like api.example.com, optionally starting with *.`;
}

/** Why `name` is not an environment variable name (`A-Z`, then `A-Z`, `0-9` or `_`), or `null`. */
export function envNameProblem(name: string): string | null {
  if (name === "") return "Enter a name.";
  return /^[A-Z][A-Z0-9_]*$/.test(name) ? null : "Use capital letters, digits and _, starting with a letter.";
}

/** The settings form's fields; numbers stay text while the user types. */
export interface SettingsDraft {
  egress: string[];
  /** Empty = no public port. */
  port: string;
  autoStopHours: string;
}

export function settingsDraft(s?: SandboxSettings): SettingsDraft {
  return {
    egress: [...(s?.egress ?? DEFAULT_SANDBOX_EGRESS)],
    port: s?.port === null || s?.port === undefined ? "" : String(s.port),
    autoStopHours: String(s?.auto_stop_hours ?? AUTO_STOP_DEFAULT_HOURS),
  };
}

export interface SettingsProblems {
  port?: string;
  autoStopHours?: string;
}

/** The settings to send, or what is wrong with the draft. */
export function checkSettings(d: SettingsDraft): { settings: SandboxSettings; problems: null } | { settings: null; problems: SettingsProblems } {
  const problems: SettingsProblems = {};
  const port = d.port.trim() === "" ? null : Number(d.port);
  if (port !== null && !(Number.isInteger(port) && port >= 1 && port <= 65_535)) problems.port = "Use a port from 1 to 65535, or leave it empty.";
  const hours = Number(d.autoStopHours);
  if (d.autoStopHours.trim() === "" || !Number.isInteger(hours) || hours < 1 || hours > AUTO_STOP_MAX_HOURS) problems.autoStopHours = `Use 1 to ${AUTO_STOP_MAX_HOURS} hours.`;
  if (problems.port !== undefined || problems.autoStopHours !== undefined) return { settings: null, problems };
  return { settings: { egress: d.egress, port, auto_stop_hours: hours }, problems: null };
}
