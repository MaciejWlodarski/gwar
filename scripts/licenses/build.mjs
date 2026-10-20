// Writes the licenses of every third-party component the desktop app ships, for its bundle
// (tauri.conf.json's beforeBuildCommand runs it after the web build): the Rust crates', native
// and in the MLS WebAssembly module, through cargo-about; the C libraries built into them; then
// the web build's list of its JavaScript packages and font. `--check` (before bundling) refuses
// the placeholder a plain cargo build leaves.
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const root = join(import.meta.dirname, "../..");
const out = join(root, "apps/desktop/src-tauri/gen/licenses/THIRD_PARTY_LICENSES.md");
const web = join(root, "apps/web/dist/licenses.md");
const desktop = "apps/desktop/src-tauri/Cargo.toml";

if (process.argv.includes("--check")) {
  if (!existsSync(out) || readFileSync(out, "utf8").includes("Not generated for this build")) {
    console.error(`${out} is not generated: tauri build writes it (scripts/licenses/build.mjs)`);
    process.exit(1);
  }
  process.exit(0);
}
if (!existsSync(web)) {
  console.error(`${web} is missing: build the web app first`);
  process.exit(1);
}
mkdirSync(dirname(out), { recursive: true });

/** cargo-about's listing for one build, as JSON. Written to a file: in PowerShell it refuses stdout. */
function about(manifest, target) {
  const file = `${out}.json`;
  try {
    execFileSync("cargo", [
      "about", "generate", "--fail", "--locked", "--format", "json", "-c", "scripts/licenses/about.toml",
      "-m", manifest, ...(target ? ["--target", target] : []), "-o", file,
    ], { cwd: root, stdio: ["ignore", "inherit", "inherit"] });
  } catch (error) {
    console.error("cargo about failed; it installs with: cargo install --locked cargo-about --version 0.9.2 --features cli");
    throw error;
  }
  const listing = JSON.parse(readFileSync(file, "utf8"));
  rmSync(file);
  return listing;
}

// One section per license text, listing every crate under it from either build.
const sections = new Map();
for (const { licenses } of [about(desktop), about("crates/gwar-mls-wasm/Cargo.toml", "wasm32-unknown-unknown")]) {
  for (const license of licenses) {
    const section = sections.get(license.text) ?? { name: license.name, crates: new Map() };
    for (const { crate } of license.used_by) section.crates.set(`${crate.name} ${crate.version}`, crate.repository);
    sections.set(license.text, section);
  }
}
const rust = [...sections].map(([text, { name, crates }]) => [
  `### ${name}`, "", "Used by:",
  ...[...crates].sort(([a], [b]) => a.localeCompare(b)).map(([crate, repository]) => `- ${crate}${repository ? ` (${repository})` : ""}`),
  "", "```text", text.trim(), "```", "",
].join("\n"));

// Opus, built from source by audiopus_sys and linked in: its own notice, beside the wrapper's.
const metadata = JSON.parse(execFileSync("cargo", ["metadata", "--locked", "--format-version", "1", "--manifest-path", desktop], { cwd: root, encoding: "utf8", maxBuffer: 256 << 20 }));
const opus = metadata.packages.find((p) => p.name === "audiopus_sys");
const opusNotice = readFileSync(join(dirname(opus.manifest_path), "opus/COPYING"), "utf8").trim();

// The web build's own heading goes; its entries' headings move one level down.
const javascript = readFileSync(web, "utf8").replace(/^# .*\n/, "").replace(/^## /gm, "### ");

writeFileSync(`${out}.tmp`, [
  "# Third-party licenses", "",
  "The licenses of the third-party components in this build of the Gwar desktop app: its Rust",
  "crates (native, and in its MLS WebAssembly module), the C libraries built into them, then the",
  "JavaScript packages and the font of its interface. Written by `scripts/licenses/build.mjs`;",
  "THIRD_PARTY_NOTICES.md has the notes on them.", "",
  "## Rust crates", "", ...rust,
  "## C libraries", "", `### Opus (built into audiopus_sys ${opus.version})`, "", "```text", opusNotice, "```", "",
  "## JavaScript packages and font", javascript,
].join("\n"));
renameSync(`${out}.tmp`, out);
console.log(`wrote ${out}`);
