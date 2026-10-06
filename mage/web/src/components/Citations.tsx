// Code locations cited by causes and diagnoses: `path:L{a}-L{b}@sha`, linked to the GitHub
// permalink when the citation is in the ticket's repo.
import type { Citation, Repo } from "../gen/mage";
import { citationUrl } from "../lib/format";
import { ExternalLink, Sha } from "./common";

export function CitationList({ citations, repo, label = "Evidence" }: { citations: readonly Citation[]; repo: Repo | null; label?: string }) {
  if (citations.length === 0) return null;
  return (
    <ul className="citations" aria-label={label}>
      {citations.map((c, i) => {
        const where = `${c.path}:${c.lines.start}-${c.lines.end}`;
        const url = citationUrl(repo, c);
        return (
          <li key={`${c.path}:${c.lines.start}:${i}`}>
            {url === null ? <code>{where}</code> : <ExternalLink href={url}>{where}</ExternalLink>} <Sha sha={c.sha} />
            {c.note !== null && <span className="muted"> — {c.note}</span>}
          </li>
        );
      })}
    </ul>
  );
}
