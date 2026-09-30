// The manifest's slug rule (`cloud_contract::validate::is_slug`), checked before the server does
// so the create forms can say what is wrong while the user types.

/** Why `s` is not a valid project or org slug, or `null` when it is. */
export function slugProblem(s: string): string | null {
  if (s === "") return "Enter a name.";
  if (s.length > 40) return "Use at most 40 characters.";
  if (!/^[a-z]/.test(s)) return "Start with a lowercase letter.";
  if (!/^[a-z0-9-]+$/.test(s)) return "Use only lowercase letters, digits and -.";
  if (s.endsWith("-")) return "Do not end with -.";
  return null;
}
