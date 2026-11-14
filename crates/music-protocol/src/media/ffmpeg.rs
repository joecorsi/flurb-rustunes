//! A seekable child-stdin locator for the already-opened regular source file.
//! Sources still enter FFmpeg through the retained descriptor, not a reopened path.
pub const SEEKABLE_STDIN: &str = if cfg!(target_os = "macos") {
    "/dev/fd/0"
} else {
    "/proc/self/fd/0"
};
