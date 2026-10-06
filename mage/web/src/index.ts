// @mage/web: Mage's pages, typed API client and generated wire types. A host (mage/web-app, or the
// aho cloud console) renders `mageRoutes` / `<MageRoutes />` under `<MageProvider host={…}>`, puts
// `mageNav` in its sidebar, and imports `@aho/ui/styles.css` then `@mage/web/styles.css`.
export { decodeError, INTEGRATION_SLUG, MageApi, MageApiError, type MageApiOptions, type MageErrorCode, OrgApi, withQuery } from "./api/client";
export { browserEventSource, type EventSourceFactory, type EventSourceLike, FeedStream, RunStream } from "./api/sse";
export { MageError } from "./components/common";
export { type MageHost, MageProvider, type MageState, useFeed, useMage } from "./context";
export * from "./gen/mage";
export { atLeast, ROLE_LABEL } from "./lib/labels";
export { Markdown, safeHref } from "./lib/markdown";
export { type MageNavEntry, mageNav, mageRoutes, MageRoutes, RepoIcon, SettingsIcon, TicketIcon } from "./routes";
