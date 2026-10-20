// Writes the licenses of every third-party component the desktop app ships, for its bundle
// (tauri.conf.json's beforeBuildCommand runs it after the web build): the Rust crates' through
// cargo-about, then the web build's list of its JavaScript packages.
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

const root = join(import.meta.dirname, "../..");
const out = join(root, "apps/desktop/src-tauri/gen/licenses/THIRD_PARTY_LICENSES.md");
const web = join(root, "apps/web/dist/licenses.md");

if (!existsSync(web)) {
  console.error(`${web} is missing: build the web app first`);
  process.exit(1);
}
let rust;
try {
  rust = execFileSync("cargo", [
    "about", "generate", "--fail", "--locked",
    "-c", "scripts/licenses/about.toml",
    "-m", "apps/desktop/src-tauri/Cargo.toml",
    "scripts/licenses/about.hbs",
  ], { cwd: root, encoding: "utf8", maxBuffer: 64 << 20, stdio: ["ignore", "pipe", "inherit"] });
} catch (error) {
  console.error("cargo about failed; it installs with: cargo install --locked cargo-about --version 0.9.2 --features cli");
  throw error;
}
// The web build's own heading goes; its packages' headings move one level down.
const javascript = readFileSync(web, "utf8").replace(/^# .*\n/, "").replace(/^## /gm, "### ");

mkdirSync(dirname(out), { recursive: true });
writeFileSync(`${out}.tmp`, `${rust}\n## JavaScript packages\n${javascript}`);
renameSync(`${out}.tmp`, out);
console.log(`wrote ${out}`);
