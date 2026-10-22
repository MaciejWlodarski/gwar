//! App updates: tauri-plugin-updater checks the feed in tauri.conf.json, downloads and verifies
//! the update and installs it, driven by the web UI (platform/updater.ts). The release workflow
//! signs the update files with the release key using the `minisign` CLI and lists them in
//! `latest.json` (.github/workflows/release.yml, docs/releasing.md). These tests hold the two
//! together, through the plugin's own verification.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tauri::test::{MockRuntime, mock_builder, mock_context, noop_assets};
use tauri_plugin_updater::UpdaterExt;

const CONFIG: &str = include_str!("../tauri.conf.json");
const RELEASE_KEY: &str = include_str!("../../../../docs/release-key.pub");

/// A throwaway key pair (its secret half was deleted) and a signature made the way the release
/// workflow makes them: `minisign -G -W`, then `minisign -S -s <key> -m <file> -t <comment>`,
/// prehashed (minisign's default) and with the version in the trusted comment.
const TEST_KEY: &str = "untrusted comment: minisign public key F88FD0358F93C9C7\nRWTHyZOPNdCP+GAVLIDCkQ/2lNGfq/r4YLUQCtpK1ZbyUfovWVUpJtrU\n";
const TEST_SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RUTHyZOPNdCP+Ca+prOR7mfuL2j186TzH5C8lH6LUo9Ss0HtJ9L+ZFd9NUCSZOWJVQDLb8Y4PbXaPxxCOVz4AMPOxJR9yG4G9Aw=
trusted comment: timestamp:1792000000\tfile:Gwar_9.9.9_universal.app.tar.gz\tversion:9.9.9
On0ok5l0IMv29O8ePQoV6KeCHCMT03fMB/VSwj2Mf9/8Z7jNYDcJiKbEunVGE0bGwtW9MHacnRybqqESeyKADQ==
";
const TEST_PAYLOAD: &[u8] = b"Gwar updater test payload\n";

/// Every key the workflow writes into `latest.json`'s `platforms`.
const TARGETS: [&str; 6] =
    ["darwin-aarch64", "darwin-x86_64", "windows-x86_64", "windows-x86_64-msi", "linux-x86_64", "linux-x86_64-deb"];

fn updater_config() -> Value {
    let config: Value = serde_json::from_str(CONFIG).unwrap();
    config["plugins"]["updater"].clone()
}

/// A mock app running the updater plugin with this app's configuration, except for the key.
fn app(pubkey: &str) -> tauri::App<MockRuntime> {
    let mut updater = updater_config();
    updater["pubkey"] = json!(STANDARD.encode(pubkey));
    let mut context = mock_context(noop_assets());
    context.config_mut().plugins.0.insert("updater".into(), updater);
    mock_builder().plugin(tauri_plugin_updater::Builder::new().build()).build(context).unwrap()
}

/// Whether the plugin, reading `manifest` for `target`, accepts `bytes` as the update.
fn verify(app: &tauri::App<MockRuntime>, manifest: &Value, target: &str, bytes: &[u8]) -> Result<(), String> {
    let updater = app.updater_builder().target(target).build().map_err(|e| e.to_string())?;
    let update = updater.restore_update(manifest.clone()).map_err(|e| e.to_string())?.ok_or("not an update")?;
    update.verify(bytes).map_err(|e| e.to_string())
}

#[test]
fn the_feed_is_checked_with_the_release_key() {
    let config: tauri_plugin_updater::Config = serde_json::from_value(updater_config()).unwrap();
    let pubkey = String::from_utf8(STANDARD.decode(&config.pubkey).unwrap()).unwrap();
    assert_eq!(pubkey, RELEASE_KEY, "plugins.updater.pubkey must be docs/release-key.pub in base64");
    assert!(config.require_signed_version);
    let endpoints: Vec<&str> = config.endpoints.iter().map(|u| u.as_str()).collect();
    assert_eq!(endpoints, ["https://github.com/MaciejWlodarski/gwar/releases/latest/download/latest.json"]);
}

#[test]
fn minisign_cli_signatures_verify_for_every_target() {
    let app = app(TEST_KEY);
    let entry = json!({ "url": "https://example.invalid/Gwar_9.9.9_universal.app.tar.gz", "signature": STANDARD.encode(TEST_SIGNATURE) });
    let platforms: serde_json::Map<String, Value> = TARGETS.iter().map(|t| (t.to_string(), entry.clone())).collect();
    let manifest =
        json!({ "version": "9.9.9", "notes": "", "pub_date": "2026-10-22T12:00:00Z", "platforms": platforms });
    for target in TARGETS {
        verify(&app, &manifest, target, TEST_PAYLOAD).unwrap_or_else(|e| panic!("{target}: {e}"));
        assert!(verify(&app, &manifest, target, b"tampered").is_err(), "{target}");
    }
    // The signed version binds the file: the same signature cannot be announced as another version.
    let mut newer = manifest.clone();
    newer["version"] = json!("9.9.10");
    assert!(verify(&app, &newer, "linux-x86_64", TEST_PAYLOAD).unwrap_err().contains("9.9.9"));
    // Nor checked with another key, such as the release key.
    assert!(verify(&self::app(RELEASE_KEY), &manifest, "linux-x86_64", TEST_PAYLOAD).is_err());
}

/// Checks a release's update files the way the app will: `GWAR_UPDATER_DIR` holds `latest.json`,
/// the files it names and `key.pub`, the public key they were signed with. For trying the release
/// workflow's "Sign the updates" step: run its script with a throwaway key on stand-in files, then
/// `GWAR_UPDATER_DIR=<dir> cargo test -p gwar-desktop a_signed_release -- --ignored`.
#[test]
#[ignore = "needs GWAR_UPDATER_DIR"]
fn a_signed_release_verifies() {
    let dir = std::path::PathBuf::from(std::env::var("GWAR_UPDATER_DIR").expect("GWAR_UPDATER_DIR"));
    let key = std::fs::read_to_string(dir.join("key.pub")).unwrap();
    let manifest: Value = serde_json::from_slice(&std::fs::read(dir.join("latest.json")).unwrap()).unwrap();
    let app = app(&key);
    let platforms = manifest["platforms"].as_object().unwrap();
    assert_eq!(platforms.len(), TARGETS.len());
    for target in TARGETS {
        let url = platforms[target]["url"].as_str().unwrap();
        let file = std::fs::read(dir.join(url.rsplit('/').next().unwrap())).unwrap();
        verify(&app, &manifest, target, &file).unwrap_or_else(|e| panic!("{target}: {e}"));
        println!("{target}: {url} verified");
    }
}
