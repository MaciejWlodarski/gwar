use std::{fs, path::Path};

fn main() {
    // The bundle's third-party licenses are written by scripts/licenses/build.mjs, which
    // `tauri build` runs first; a plain cargo build gets a note in their place, which the check
    // before bundling refuses.
    let licenses = Path::new("gen/licenses/THIRD_PARTY_LICENSES.md");
    if !licenses.exists() {
        fs::create_dir_all(licenses.parent().unwrap()).unwrap();
        fs::write(
            licenses,
            "# Third-party licenses\n\nNot generated for this build: scripts/licenses/build.mjs writes them.\n",
        )
        .unwrap();
    }
    // The app still starts on macOS 11, which has no ScreenCaptureKit; share sound checks first.
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "macos") {
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,ScreenCaptureKit");
    }
    tauri_build::build()
}
