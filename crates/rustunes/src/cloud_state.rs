//! Cloud portability boundary. Only durable verified mappings are exported;
//! pairing, caches, temporary audio, and executable operation intents stay local.
pub use crate::engine::state::journal::{export_cloud_snapshot, import_cloud_snapshot};
use anyhow::Result;
use std::path::Path;
pub fn export_playlists(state: &Path, root: &Path) -> Result<String> {
    crate::cli::playlist_push::managed::export_cloud_baselines(state, root)
}
pub fn import_playlists(state: &Path, root: &Path, body: &str) -> Result<()> {
    crate::cli::playlist_push::managed::import_cloud_baselines(state, root, body)
}
