// Markdown from untrusted sources (ticket bodies, cause explanations, fix briefs, agent summaries),
// per server.md "Untrusted content": no raw HTML, links to absolute http(s) URLs only and with
// `rel="noopener noreferrer nofollow"`, images shown as links (nothing remote is loaded). The tree
// is rendered as React elements, so text is never parsed as HTML.
import type { ComponentProps } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";

export const LINK_REL = "noopener noreferrer nofollow";

/** `url` if it is an absolute `http:` or `https:` URL, else `null`. */
export function safeHref(url: string): string | null {
  let parsed: URL;
  try {
    parsed = new URL(url);
  } catch {
    return null;
  }
  return parsed.protocol === "http:" || parsed.protocol === "https:" ? url : null;
}

function Link({ href, children }: ComponentProps<"a">) {
  if (href === undefined) return <span className="md-link-removed">{children}</span>;
  return (
    <a href={href} rel={LINK_REL} target="_blank">
      {children}
    </a>
  );
}

function ImageLink({ src, alt }: ComponentProps<"img">) {
  const label = alt !== undefined && alt !== "" ? alt : "image";
  if (typeof src !== "string") return <span className="md-image">[{label}]</span>;
  return (
    <a className="md-image" href={src} rel={LINK_REL} target="_blank">
      [image: {label}]
    </a>
  );
}

const COMPONENTS: Components = { a: Link, img: ImageLink };

const PLUGINS = [remarkGfm];

/** Renders untrusted markdown safely. */
export function Markdown({ source, className = "markdown" }: { source: string; className?: string }) {
  return (
    <div className={className}>
      <ReactMarkdown remarkPlugins={PLUGINS} skipHtml urlTransform={(url) => safeHref(url) ?? undefined} components={COMPONENTS}>
        {source}
      </ReactMarkdown>
    </div>
  );
}
