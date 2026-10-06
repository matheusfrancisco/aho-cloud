// Line diff for the recipe editor ("diff vs proposal"): longest common subsequence over lines.

export type DiffLine = { op: " " | "-" | "+"; text: string };

/** Lines of `text`; the newline ending the last line does not start another. */
export function splitLines(text: string): string[] {
  const lines = text.split("\n");
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  return lines;
}

/** `before` → `after` as kept (` `), removed (`-`) and added (`+`) lines, in order. */
export function lineDiff(before: string, after: string): DiffLine[] {
  const a = splitLines(before);
  const b = splitLines(after);
  // Common prefix and suffix are kept as is; the LCS table covers only the middle.
  let start = 0;
  while (start < a.length && start < b.length && a[start] === b[start]) start++;
  let endA = a.length;
  let endB = b.length;
  while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) {
    endA--;
    endB--;
  }
  const n = endA - start;
  const m = endB - start;
  // lcs[i][j] = LCS length of a[start+i..endA) and b[start+j..endB), flattened.
  const lcs = new Uint32Array((n + 1) * (m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i * (m + 1) + j] = a[start + i] === b[start + j] ? lcs[(i + 1) * (m + 1) + j + 1]! + 1 : Math.max(lcs[(i + 1) * (m + 1) + j]!, lcs[i * (m + 1) + j + 1]!);
    }
  }
  const out: DiffLine[] = a.slice(0, start).map((text) => ({ op: " ", text }));
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[start + i] === b[start + j]) {
      out.push({ op: " ", text: a[start + i]! });
      i++;
      j++;
    } else if (lcs[(i + 1) * (m + 1) + j]! >= lcs[i * (m + 1) + j + 1]!) {
      out.push({ op: "-", text: a[start + i]! });
      i++;
    } else {
      out.push({ op: "+", text: b[start + j]! });
      j++;
    }
  }
  for (; i < n; i++) out.push({ op: "-", text: a[start + i]! });
  for (; j < m; j++) out.push({ op: "+", text: b[start + j]! });
  for (const text of a.slice(endA)) out.push({ op: " ", text });
  return out;
}
