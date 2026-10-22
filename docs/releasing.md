# Releasing

Pushing a tag `vX.Y.Z` builds the desktop app and the server binaries on
GitHub Actions (`.github/workflows/release.yml`) and puts them in a **draft**
GitHub release. Nothing is public until you publish the draft by hand.

## Cut a release

1. Make sure `main` is green and contains what you want to ship.
2. Set the version everywhere:

   ```bash
   scripts/bump-version.sh 0.2.0
   ```

   It edits the Cargo manifests, `tauri.conf.json` and the `package.json`
   files, refreshes `Cargo.lock` and checks the result. It does not commit or
   tag. Running it again with the same version changes nothing.
3. Commit, tag and push:

   ```bash
   git commit -am "Release v0.2.0"
   git tag v0.2.0
   git push origin main v0.2.0
   ```

4. Watch the **release** workflow in the Actions tab. It first runs
   `scripts/check-version.sh`, which fails the whole run if the tag differs
   from any version (manifests, `tauri.conf.json`, `package.json` files or
   `Cargo.lock`). Then it builds everything in parallel (macOS, Windows, Linux
   desktop; Linux x86-64 and ARM64 server) and, if all builds pass, creates the
   draft release with all files and `SHA256SUMS`, with a GitHub provenance
   attestation for each file.
5. The **sign** job then waits for your approval (the `release` environment).
   Approving hands the release key to that job, so first check that the run
   is for your tag, on the commit you pushed, and that nobody changed
   `.github/workflows/release.yml` there. Then approve it, and it adds
   `SHA256SUMS.minisig`, the signatures of the update files and `latest.json`
   (see [Updates](#updates)) to the draft. Approve before publishing; it
   refuses to change a published release.
6. Once the sign job has finished and `SHA256SUMS.minisig` and `latest.json`
   are on the draft, open the draft under Releases. Edit the generated notes,
   optionally download and try a few assets, then press **Publish release**.
   Publish it as a normal release, with **Set as the latest release** checked:
   the desktop apps are only offered the latest release, so a pre-release is
   not offered to them (use that on purpose for a release you do not want
   installed automatically).

Tags must look like `v1.2.3` (no `-rc1` suffixes: the Windows MSI format does
not accept them). If a build fails, fix the problem, delete the tag
(`git push --delete origin v0.2.0 && git tag -d v0.2.0`), and tag again. If
only a flaky job failed, "Re-run failed jobs" is enough: re-running refreshes
the files of an existing draft. A published release is never modified by the
workflow.

### Dry run

Actions > release > **Run workflow** builds everything without a tag and
without creating a release. The files are kept for 14 days as the workflow
artifact `release-assets` (plus per-platform artifacts). In this mode the check
job only verifies that all versions agree with each other.

## Assets

| File | What |
| --- | --- |
| `Gwar_X.Y.Z_universal.dmg` | macOS desktop app, Apple Silicon and Intel, macOS 11+ |
| `Gwar_X.Y.Z_universal.app.tar.gz` | the same app for the updater (installing by hand: use the `.dmg`) |
| `Gwar_X.Y.Z_x64-setup.exe` | Windows installer (NSIS) |
| `Gwar_X.Y.Z_x64_en-US.msi` | Windows installer (MSI) |
| `Gwar_X.Y.Z_amd64.AppImage` | Linux desktop app, runs anywhere with a recent glibc |
| `Gwar_X.Y.Z_amd64.deb` | Linux desktop app for Debian and Ubuntu |
| `gwar-server-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` | `gwar-server` and `gwar-connect`, Linux x86-64 |
| `gwar-server-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` | the same for Linux ARM64 (Oracle Ampere, Raspberry Pi 4/5 with a 64-bit OS) |
| `SHA256SUMS` | SHA-256 of every file above |
| `SHA256SUMS.minisig` | minisign signature of `SHA256SUMS` with the release key |
| `*.minisig` (the others) | signatures of the files the updater installs (`.app.tar.gz`, `-setup.exe`, `.msi`, `.AppImage`, `.deb`) |
| `latest.json` | the update feed: version, and the URL and signature of each update file |

The desktop file names come from Tauri, so they can differ slightly from this
table. The server archives hold a folder with `gwar-server`, `gwar-connect`,
`LICENSE` and `THIRD_PARTY_NOTICES.md`. They include TeamSpeak support (the
default `teamspeak` feature), link Opus statically, and are built on Ubuntu
22.04, so they need glibc 2.35 or newer (Ubuntu 22.04+, Debian 12+).

## Verify a download

**1. The signature** proves `SHA256SUMS` comes from the Gwar release key, not
just from whoever served you the files. Install
[minisign](https://jedisct1.github.io/minisign/) (`brew install minisign`,
`apt install minisign`, `scoop install minisign`) and run, next to
`SHA256SUMS` and `SHA256SUMS.minisig`:

```bash
minisign -Vm SHA256SUMS -P RWQ7B0C3dfQMu/i6pOK6gtlo5MBdyDidCO7Iec4mv6zBuOjwL5+p446A
```

It must print `Signature and comment signature verified` and a trusted comment
`gwar vX.Y.Z SHA256SUMS` with the version you downloaded. The key is also in
[release-key.pub](release-key.pub); take it from this repository, not from a
download page.

**2. The checksum** ties your file to the signed list. Put the file next to
`SHA256SUMS` and check just that file:

```bash
# Linux
sha256sum --check --ignore-missing SHA256SUMS
# macOS
grep ' Gwar_0.2.0_universal.dmg$' SHA256SUMS | shasum -a 256 -c
```

Windows (PowerShell): compare the output of
`(Get-FileHash .\Gwar_0.2.0_x64-setup.exe -Algorithm SHA256).Hash` with the
line for that file in `SHA256SUMS` (case does not matter).

**3. Provenance (optional)** shows which workflow and commit built the file,
with the [GitHub CLI](https://cli.github.com/):

```bash
gh attestation verify Gwar_0.2.0_universal.dmg --repo MaciejWlodarski/gwar \
  --signer-workflow MaciejWlodarski/gwar/.github/workflows/release.yml \
  --source-ref refs/tags/v0.2.0
```

A valid attestation
proves where a file was built, not that the code was good.

None of this removes the operating system warnings below: those need paid
certificates.

## Updates

The desktop app updates itself from the latest published release
(`tauri-plugin-updater`). Shortly after it starts, and then every six hours, it
reads
`https://github.com/MaciejWlodarski/gwar/releases/latest/download/latest.json`.
If that names a newer version (compared as semver with the version in
`tauri.conf.json`), it downloads the update in the background and shows an icon
at the bottom of the server rail, with the progress in its tooltip. Nothing is
installed until the user clicks the icon; the app then installs the update and
restarts, asking first during a call or a screen share. An up-to-date app shows
no icon, and a failed check (offline, or no published release yet) is retried
quietly at the next one. A failed download or install shows a warning icon;
clicking it starts again. The version and the commit an app was built from
(`v0.1.2 · 31b65bd`, `-dirty` for uncommitted changes) are at the very bottom
of the rail; clicking copies them, for bug reports.

GitHub's `latest` is the newest published release that is not a pre-release,
so drafts and pre-releases are never offered. The releases up to v0.1.2 are
pre-releases without a `latest.json`: until a normal release with one is
published, the check finds no feed and the apps show nothing.

The app installs these files, listed per platform in `latest.json` (it looks
up `<os>-<arch>-<installer>` first, then `<os>-<arch>`):

| Feed key | File | Installed app |
| --- | --- | --- |
| `darwin-aarch64`, `darwin-x86_64` | `Gwar_X.Y.Z_universal.app.tar.gz` | macOS (replaces `Gwar.app`; asks for an administrator password when the user cannot write there) |
| `windows-x86_64` | `Gwar_X.Y.Z_x64-setup.exe` | Windows, installed with the setup program (runs it with a progress bar) |
| `windows-x86_64-msi` | `Gwar_X.Y.Z_x64_en-US.msi` | Windows, installed with the MSI |
| `linux-x86_64` | `Gwar_X.Y.Z_amd64.AppImage` | the AppImage (replaces the file) |
| `linux-x86_64-deb` | `Gwar_X.Y.Z_amd64.deb` | the Debian package (`dpkg -i`, asks for a password with `pkexec`) |

The app checks every update file against the release key before installing
it: the sign job signs each one with `minisign` (the public key is in
`plugins > updater > pubkey` of `tauri.conf.json`, base64 of
[release-key.pub](release-key.pub)). Each signature's trusted comment carries
`version:X.Y.Z`, and the app (`requireSignedVersion`) refuses a file signed for
another version than `latest.json` announces, so a tampered feed cannot pass
off an older release as a newer one. `latest.json` itself is not signed.

The build jobs need no key: Tauri's own updater files
(`bundle > createUpdaterArtifacts`) would need its private key at build time,
so the workflow packs the macOS app itself and serves the installers as they
are. Locally built apps (`pnpm tauri build`) check for updates the same way, so
a local 0.1.2 build is offered 0.1.3 once it is published. `tauri dev` and the
web app never check.

## The release key

`SHA256SUMS` and the update files are signed with a dedicated minisign key,
used for nothing else.
Its public half is [release-key.pub](release-key.pub); the secret half exists
only as the `MINISIGN_SECRET_KEY` secret of the `release` environment
(Settings > Environments). That environment accepts only `v*` tags and needs
the owner's approval for every run, so a workflow on another branch or a pull
request (forks included) cannot use the key. GitHub never shows a secret again,
in the interface or the API, but a job you approve receives it in plain text:
a tag whose workflow was changed could send the key anywhere. That is why the
approval step above asks you to check the run first. With another repository
administrator, approval would no longer be only yours.

If the key is lost or may have leaked, make a new one in a private folder,
store it, and only then replace the public half:

```bash
dir=$(mktemp -d)
minisign -G -W -p "$dir/release-key.pub" -s "$dir/release.key"
gh secret set MINISIGN_SECRET_KEY --env release < "$dir/release.key" &&
  cp "$dir/release-key.pub" docs/release-key.pub &&
  rm -r "$dir"
```

If `gh secret set` fails, the new key stays in `$dir`; try again from there.
Then update the key in "Verify a download" above and the updater's key in
`tauri.conf.json` (`base64 < docs/release-key.pub | tr -d '\n'`; a test of the
desktop app checks that they match), commit, and say in the next release notes
that the key changed. Releases signed with the old key stay checkable with the
old public key from the repository history. Installed apps only accept updates
signed with the key they were built with, so after a change everyone has to
install the next version by hand once.

## Code signing and the secrets

Code signing is optional. Without secrets the workflow still builds everything,
just unsigned, and prints a warning. Add secrets under Settings > Secrets and
variables > Actions.

### macOS (signing and notarization)

Needs a paid Apple Developer Program membership and a **Developer ID
Application** certificate.

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE` | the certificate exported from Keychain Access as `.p12`, base64: `base64 -i cert.p12 \| pbcopy` |
| `APPLE_CERTIFICATE_PASSWORD` | the password chosen when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | for example `Developer ID Application: Your Name (TEAMID)` (`security find-identity -v -p codesigning`) |
| `APPLE_ID` | the Apple ID e-mail used for notarization |
| `APPLE_PASSWORD` | an app-specific password for that Apple ID (appleid.apple.com) |
| `APPLE_TEAM_ID` | the 10-character team ID |

The first three sign the app; with all six it is also notarized. With only the
first three you get a signed but not notarized app, which Gatekeeper still
warns about.

Without any of them the app is only ad-hoc signed (`signingIdentity: "-"` in
`tauri.conf.json`). macOS then refuses to open it at first: right-click the app
and choose **Open**, then **Open** again. On recent macOS versions that button
may not appear; instead try to open it once, then go to System Settings >
Privacy & Security and press **Open Anyway** near the bottom. Alternatively run
`xattr -dr com.apple.quarantine /Applications/Gwar.app`.

### Windows

Windows code signing is **not wired up**: the installers are unsigned, and
Microsoft Defender SmartScreen shows "Windows protected your PC". Click
**More info**, then **Run anyway**. Tauri can sign on Windows (a certificate
thumbprint or a custom `signCommand` in `bundle > windows`, for example with
Azure Trusted Signing), but that needs a certificate setup that depends on the
provider, so it is left out until there is one.

### Linux and the server binaries

No code signing; check them with the signed `SHA256SUMS` (see "Verify a download").
