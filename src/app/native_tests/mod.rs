//! Native window checks, run individually in a fresh private guest environment.

mod account;
mod playback;
pub(in crate::app) mod render;
mod restart;
mod sync;

use anyhow::{Context as _, Result, ensure};
use std::{fs, os::unix::fs::MetadataExt, path::PathBuf};

pub(in crate::app) fn isolate() -> Result<()> {
    ensure!(
        std::env::var("YT_MUSIC_NATIVE_TESTS").as_deref() == Ok("1"),
        "Set YT_MUSIC_NATIVE_TESTS=1 in a fresh isolated environment"
    );
    let runtime =
        PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("Set private runtime")?);
    let resolved = runtime.canonicalize()?;
    let metadata = fs::metadata(&runtime)?;
    ensure!(
        runtime == resolved
            && runtime.parent() == Some(std::path::Path::new("/tmp"))
            && metadata.is_dir()
            && metadata.mode() & 0o777 == 0o700
            && metadata.uid() == unsafe { libc::geteuid() },
        "Use a fresh owned 0700 runtime directory directly under /tmp"
    );
    let mut homes = Vec::new();
    for key in ["XDG_DATA_HOME", "XDG_CACHE_HOME"] {
        let home = PathBuf::from(std::env::var_os(key).with_context(|| format!("Set {key}"))?);
        ensure!(
            home.starts_with(&runtime) && home != runtime,
            "{key} must be inside the private runtime"
        );
        fs::create_dir_all(&home)?;
        let home = home.canonicalize()?;
        ensure!(
            home.starts_with(&runtime) && home != runtime,
            "{key} must be inside the private runtime"
        );
        homes.push(home);
    }
    ensure!(
        homes[0] != homes[1],
        "Use separate data and cache directories"
    );
    ensure!(
        std::env::var("DBUS_SESSION_BUS_ADDRESS")?.contains("/tmp/"),
        "Run under dbus-run-session"
    );
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    ensure!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } == 0);
    crate::video::initialize()
}
