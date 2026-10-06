//! The `mage/web-app` bundle embedded at build time (`build.rs`), handed to the server.

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

/// The embedded bundle, or API only when the binary was built without `web-app/dist`.
pub fn bundle() -> mage_server::Assets {
    if WEB_ASSETS.is_empty() {
        tracing::warn!("this binary embeds no web-app bundle (web-app/dist was absent at build time): serving the API only");
        return mage_server::Assets::None;
    }
    tracing::info!(files = WEB_ASSETS.len(), "serving the embedded web-app bundle");
    mage_server::Assets::Embedded(WEB_ASSETS.iter().map(|(path, bytes)| ((*path).to_owned(), *bytes)).collect())
}
