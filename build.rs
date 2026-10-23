fn main() {
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
