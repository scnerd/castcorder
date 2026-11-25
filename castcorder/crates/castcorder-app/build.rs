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
    // The commands the call window may call from the project's pages
    // (cast-40e.10); main.rs grants them to that window and origin only.
    #[cfg(feature = "desktop")]
    tauri_build::try_build(tauri_build::Attributes::new().app_manifest(
        tauri_build::AppManifest::new().commands(&[
            "contact_code",
            "contact_add",
            "joined_rooms_save",
        ]),
    ))
    .expect("tauri build step");
}
