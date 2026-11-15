//! Where the app keeps its files on each platform.

use std::path::PathBuf;

const APP: &str = "yt-music-gpui";

/// Saved music, preferences and the account marker.
pub(crate) fn data() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        home("Library/Application Support")
    } else if cfg!(windows) {
        absolute("LOCALAPPDATA")
    } else {
        xdg("XDG_DATA_HOME", ".local/share")
    };
    base.map(|base| base.join(APP))
}

/// Files the app can download again, such as artwork.
pub(crate) fn cache() -> Option<PathBuf> {
    if cfg!(windows) {
        return data().map(|app| app.join("cache"));
    }
    let base = if cfg!(target_os = "macos") {
        home("Library/Caches")
    } else {
        xdg("XDG_CACHE_HOME", ".cache")
    };
    base.map(|base| base.join(APP))
}

/// The XDG spec says to ignore empty or relative values, which would
/// otherwise put user data under the launch directory.
fn xdg(variable: &str, fallback: &str) -> Option<PathBuf> {
    absolute(variable).or_else(|| home(fallback))
}

fn home(relative: &str) -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(relative))
}

fn absolute(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}
