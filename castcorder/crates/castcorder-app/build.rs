//! The Tauri shell's build step (feature `desktop`); the library needs none.

fn main() {
    #[cfg(feature = "desktop")]
    tauri_build::build();
}
