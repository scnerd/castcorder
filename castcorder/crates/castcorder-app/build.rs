//! The Tauri shell's build step (feature `desktop`); the library needs none.

fn main() {
    // The macOS GStreamer.framework links every library as @rpath/<name>. The
    // .app ships the trimmed copy in Contents/Frameworks (task app:gst:stage);
    // a development build finds the framework install.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos")
        && std::env::var_os("CARGO_FEATURE_NATIVE_CAPTURE").is_some()
    {
        for rpath in [
            "@executable_path/../Frameworks",
            "/Library/Frameworks/GStreamer.framework/Versions/1.0/lib",
        ] {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{rpath}");
        }
    }
    #[cfg(feature = "desktop")]
    tauri_build::build();
}
