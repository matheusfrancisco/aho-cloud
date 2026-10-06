// Knowledge layer states of one indexed commit.
import { Badge } from "@aho/ui";
import type { LayerState, RecipeStatus, VersionView } from "../gen/mage";
import { LAYER_LABEL, LAYER_TONE, LAYERS, RECIPE_STATUS_LABEL, RECIPE_STATUS_TONE } from "../lib/labels";

export function LayerBadge({ state }: { state: LayerState | undefined }) {
  if (state === undefined) return <span className="muted">—</span>;
  const reason = state.type === "failed" || state.type === "skipped" ? state.reason : undefined;
  return (
    <span title={reason}>
      <Badge tone={LAYER_TONE[state.type]}>{state.type}</Badge>
    </span>
  );
}

/** Every layer of `version` as a labeled badge. */
export function LayerSummary({ version }: { version: VersionView | null }) {
  if (version === null) return <span className="muted">not indexed yet</span>;
  return (
    <span className="tags">
      {LAYERS.filter((l) => version.layers[l] !== undefined).map((l) => (
        <span key={l} className="small">
          {LAYER_LABEL[l]} <LayerBadge state={version.layers[l]} />
        </span>
      ))}
    </span>
  );
}

export function RecipeBadge({ status }: { status: RecipeStatus }) {
  return (
    <span title={status.type === "broken" ? status.reason : undefined}>
      <Badge tone={RECIPE_STATUS_TONE[status.type]}>{RECIPE_STATUS_LABEL[status.type]}</Badge>
    </span>
  );
}
