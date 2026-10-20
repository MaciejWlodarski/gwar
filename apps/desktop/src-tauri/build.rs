use std::{fs, path::Path};

fn main() {
    // The bundle's third-party licenses are written by scripts/licenses/build.mjs, which
    // `tauri build` runs first; a plain cargo build gets a note in their place.
    let licenses = Path::new("gen/licenses/THIRD_PARTY_LICENSES.md");
    if !licenses.exists() {
        fs::create_dir_all(licenses.parent().unwrap()).unwrap();
        fs::write(
            licenses,
            "# Third-party licenses\n\nNot generated for this build: scripts/licenses/build.mjs writes them.\n",
        )
        .unwrap();
    }
    tauri_build::build()
}
