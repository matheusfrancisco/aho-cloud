// A run's artifacts: video and GIF players, the trace viewer link, logs (virtualized), the repro
// script, the patch as a diff, and downloads. All artifact text is untrusted: rendered as text nodes.
import { FileViewer, formatBytes, Loading, useAction, useLoad } from "@aho/ui";
import { useMemo, useState } from "react";
import type { OrgApi } from "../api/client";
import type { ArtifactView } from "../gen/mage";
import { ARTIFACT_KIND_LABEL } from "../lib/labels";
import { splitLines } from "../lib/lineDiff";
import { MageError } from "./common";

const LOG_MAX_BYTES = 4 << 20;
const TEXT_MAX_BYTES = 1 << 20;
const LINE_PX = 18;
const LOG_VIEW_PX = 396;
const OVERSCAN = 30;

/** A long log, rendering only the lines in view. */
export function LogView({ text, label }: { text: string; label: string }) {
  const lines = useMemo(() => splitLines(text), [text]);
  const [scrollTop, setScrollTop] = useState(0);
  const first = Math.max(0, Math.floor(scrollTop / LINE_PX) - OVERSCAN);
  const end = Math.min(lines.length, Math.ceil((scrollTop + LOG_VIEW_PX) / LINE_PX) + OVERSCAN);
  const rows = [];
  for (let i = first; i < end; i++) {
    rows.push(
      <div key={i} className="log-line" style={{ top: i * LINE_PX, height: LINE_PX, lineHeight: `${LINE_PX}px` }}>
        {lines[i]}
      </div>,
    );
  }
  return (
    <div className="log-view" role="log" aria-label={label} style={{ height: LOG_VIEW_PX }} onScroll={(e) => setScrollTop(e.currentTarget.scrollTop)}>
      <div style={{ position: "relative", height: lines.length * LINE_PX }}>{rows}</div>
    </div>
  );
}

function lineClass(line: string): string {
  if (line.startsWith("diff --git") || line.startsWith("+++ ") || line.startsWith("--- ")) return "file";
  if (line.startsWith("@@")) return "hunk";
  if (line.startsWith("+")) return "add";
  if (line.startsWith("-")) return "del";
  return "";
}

/** A unified diff with added, removed, hunk and file lines marked. */
export function DiffView({ patch, label = "patch" }: { patch: string; label?: string }) {
  const lines = useMemo(() => splitLines(patch), [patch]);
  return (
    <pre className="diff" aria-label={label}>
      {lines.map((line, i) => (
        <div key={i} className={lineClass(line)}>
          {line === "" ? " " : line}
        </div>
      ))}
    </pre>
  );
}

function Truncated({ artifact, shown }: { artifact: ArtifactView; shown: number }) {
  return (
    <p className="note">
      Only the first {formatBytes(shown)} of {formatBytes(artifact.bytes)} are shown; download the file for the rest.
    </p>
  );
}

/** Fetches a text artifact (log, patch, script) and shows it the way its kind is read. */
export function TextArtifact({ api, artifact }: { api: OrgApi; artifact: ArtifactView }) {
  const max = artifact.kind === "log" ? LOG_MAX_BYTES : TEXT_MAX_BYTES;
  const loaded = useLoad(() => api.artifactText(artifact, max), [api, artifact.id]);
  if (loaded.data === undefined) return loaded.loading ? <Loading what={artifact.name} /> : <MageError error={loaded.error} />;
  const { text, truncated } = loaded.data;
  return (
    <>
      {truncated && <Truncated artifact={artifact} shown={max} />}
      {artifact.kind === "log" && <LogView text={text} label={artifact.name} />}
      {artifact.kind === "patch" && (
        <>
          <DiffView patch={text} label={artifact.name} />
          <p className="hint">
            Apply it from the repository root: <code>git apply {artifact.name}</code>
          </p>
        </>
      )}
      {artifact.kind === "script" && <FileViewer file={{ path: artifact.name, size: artifact.bytes, text, truncated }} />}
    </>
  );
}

/** Text shown on demand: the bytes are fetched when the section is first opened. */
function Expandable({ api, artifact }: { api: OrgApi; artifact: ArtifactView }) {
  const [open, setOpen] = useState(false);
  return (
    <details onToggle={(e) => setOpen(e.currentTarget.open)}>
      <summary>Show {ARTIFACT_KIND_LABEL[artifact.kind].toLowerCase()}</summary>
      {open && <TextArtifact api={api} artifact={artifact} />}
    </details>
  );
}

function TraceButton({ api, artifact }: { api: OrgApi; artifact: ArtifactView }) {
  const [url, setUrl] = useState<string | null>(null);
  const share = useAction(async () => {
    const link = await api.shareArtifact(artifact.id);
    setUrl(link.url);
    // The trace viewer lives on its own origin; it gets no handle on this window.
    window.open(link.url, "_blank", "noopener,noreferrer");
  });
  return (
    <>
      <button type="button" className="secondary small" onClick={() => void share.run()} disabled={share.busy}>
        {share.busy ? "Opening…" : "Open trace"}
      </button>
      {url !== null && (
        <a href={url} target="_blank" rel="noopener noreferrer">
          Trace viewer link (valid 10 minutes)
        </a>
      )}
      <MageError error={share.error} />
    </>
  );
}

/** Kinds the content route refuses (403): never offered. */
const HIDDEN: Record<ArtifactView["kind"], boolean> = {
  scip_index: true,
  source_bundle: true,
  video: false,
  gif: false,
  trace: false,
  screenshot: false,
  log: false,
  script: false,
  patch: false,
  transcript: false,
  fix_brief: false,
};

function Artifact({ api, artifact }: { api: OrgApi; artifact: ArtifactView }) {
  const url = api.artifactUrl(artifact.id);
  return (
    <li id={`artifact-${artifact.id}`} className="stack">
      <div className="row">
        <strong>{ARTIFACT_KIND_LABEL[artifact.kind]}</strong>
        <code>{artifact.name}</code>
        <span className="muted small">{formatBytes(artifact.bytes)}</span>
        <span className="grow" />
        {artifact.kind === "trace" && <TraceButton api={api} artifact={artifact} />}
        <a href={url} download={artifact.name}>
          {artifact.kind === "patch" ? "Download .patch" : "Download"}
        </a>
      </div>
      {artifact.kind === "video" && (
        <div className="artifact-media">
          <video controls preload="metadata" src={url} aria-label={artifact.name} />
        </div>
      )}
      {(artifact.kind === "gif" || artifact.kind === "screenshot") && (
        <div className="artifact-media">
          <img src={url} alt={artifact.name} />
        </div>
      )}
      {(artifact.kind === "log" || artifact.kind === "script" || artifact.kind === "patch") && <Expandable api={api} artifact={artifact} />}
    </li>
  );
}

export function ArtifactsPanel({ api, artifacts }: { api: OrgApi; artifacts: readonly ArtifactView[] }) {
  const shown = artifacts.filter((a) => a.complete && !HIDDEN[a.kind]);
  if (shown.length === 0) return <p className="muted">No artifacts yet.</p>;
  return (
    <ul className="list stack" aria-label="Artifacts">
      {shown.map((a) => (
        <Artifact key={a.id} api={api} artifact={a} />
      ))}
    </ul>
  );
}
