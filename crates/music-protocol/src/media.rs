//! Bounded local media observations and stable fingerprints.
//! These facts do not confer device execution or application ownership authority.

pub(crate) mod ape;
pub mod artwork;
pub mod artwork_inventory;
pub mod audio_source;
pub mod bitmap;
pub mod container;
pub mod ffmpeg;
pub mod fingerprint;
pub mod format;
pub mod metadata;
pub mod pcm;
pub mod renditions;
pub mod timing;

// Held inputs and codec-specific facts share the same bounded media boundary.
pub mod held_source;
pub mod jpeg;
pub mod mpeg;
pub(crate) mod mpeg_layout;
pub mod other_audio;
pub mod png;
