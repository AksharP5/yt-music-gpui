#![expect(
    clippy::expect_used,
    reason = "a build script reports failure by panicking"
)]

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
    let target = std::env::var("CARGO_CFG_TARGET_OS");
    // Windows has no video yet. player.rs binds to libmpv-2.dll at run time
    // there, so it needs no import library.
    if target.as_deref() == Ok("windows") {
        return;
    }
    if target.as_deref() == Ok("macos") {
        // Apple Silicon Homebrew keeps libmpv outside the compiler's and
        // linker's default search paths. Release builds link the LGPL libmpv
        // that packaging/macos-libmpv.sh builds instead.
        let prefix = std::env::var("LIBMPV_PREFIX")
            .or_else(|_| std::env::var("HOMEBREW_PREFIX"))
            .unwrap_or("/opt/homebrew".into());
        cc::Build::new()
            .file("native/video_macos.m")
            .include(format!("{prefix}/include"))
            .flag("-fno-objc-arc")
            .warnings(true)
            .compile("native_video");
        println!("cargo:rustc-link-search=native={prefix}/lib");
        println!("cargo:rustc-link-lib=mpv");
        for framework in ["AppKit", "QuartzCore", "IOSurface", "OpenGL"] {
            println!("cargo:rustc-link-lib=framework={framework}");
        }
        println!("cargo:rerun-if-env-changed=HOMEBREW_PREFIX");
        println!("cargo:rerun-if-env-changed=LIBMPV_PREFIX");
        println!("cargo:rerun-if-changed=native/video_macos.m");
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
