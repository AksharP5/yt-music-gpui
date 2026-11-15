fn main() {
    let source = std::fs::read_to_string("src/ytdlp_account.py").expect("read yt-dlp pin");
    let version = source
        .lines()
        .find_map(|line| {
            line.strip_prefix("REVIEWED_VERSION = \"")?
                .strip_suffix('"')
        })
        .expect("reviewed yt-dlp version");
    println!("cargo:rustc-env=REVIEWED_YTDLP={version}");
    println!("cargo:rerun-if-changed=src/ytdlp_account.py");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Video is Linux-only so far. Apple Silicon Homebrew keeps libmpv outside
        // the linker's default search path.
        let homebrew = std::env::var("HOMEBREW_PREFIX").unwrap_or("/opt/homebrew".into());
        println!("cargo:rustc-link-search=native={homebrew}/lib");
        println!("cargo:rustc-link-lib=mpv");
        println!("cargo:rerun-if-env-changed=HOMEBREW_PREFIX");
        return;
    }
    cc::Build::new()
        .file("native/video.c")
        .file("native/viewporter-protocol.c")
        .include("native")
        .warnings(true)
        .compile("native_video");
    for library in [
        "EGL",
        "X11",
        "Xext",
        "GLESv2",
        "wayland-client",
        "wayland-egl",
        "mpv",
        "pthread",
    ] {
        println!("cargo:rustc-link-lib={library}");
    }
    println!("cargo:rerun-if-changed=native/video.c");
    println!("cargo:rerun-if-changed=native/viewporter-protocol.c");
    println!("cargo:rerun-if-changed=native/viewporter-client-protocol.h");
}
