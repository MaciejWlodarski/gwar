//! Download, verify and unpack the official TeamSpeak 3 server.

use std::{
    fs::{self, File},
    io::{self, BufReader},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Pinned release; URLs and checksums from https://www.teamspeak.com/versions/server.json.
pub const VERSION: &str = "3.13.8";

/// Marker written last inside the install dir; holds the archive's SHA-256.
const MARKER: &str = ".installed";

#[derive(Clone, Debug)]
pub struct Installed {
    /// Directory containing `ts3server` (`ts3server.exe` on Windows).
    pub dir: PathBuf,
    /// The x86-64 build runs on this machine only through `box64` or `qemu-user`.
    pub emulated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    TarBz2,
    Zip,
}

#[derive(Clone, Copy, Debug)]
struct Build {
    url: &'static str,
    sha256: &'static str,
    format: Format,
    emulated: bool,
}

const LINUX_AMD64: Build = Build {
    url: "https://files.teamspeak-services.com/releases/server/3.13.8/teamspeak3-server_linux_amd64-3.13.8.tar.bz2",
    sha256: "a3c4658e09892d3dbd8ea752d0de42dc7d111bf44d09721927f0f4782496eb2d",
    format: Format::TarBz2,
    emulated: false,
};
const WINDOWS_X64: Build = Build {
    url: "https://files.teamspeak-services.com/releases/server/3.13.8/teamspeak3-server_win64-3.13.8.zip",
    sha256: "1dbde314c4895af0c8aae73755b2a920e564fabcb2c651e56d0532a473cdc5cb",
    format: Format::Zip,
    emulated: false,
};
// The macOS build is x86-64 only; Apple silicon runs it through Rosetta transparently.
const MACOS: Build = Build {
    url: "https://files.teamspeak-services.com/releases/server/3.13.8/teamspeak3-server_mac-3.13.8.zip",
    sha256: "b54095da569eabb0c01b777815d48c954871c028bec54caeb3529f36230a467b",
    format: Format::Zip,
    emulated: false,
};

fn build_for(os: &str, arch: &str) -> Result<Build> {
    Ok(match (os, arch) {
        ("linux", "x86_64") => LINUX_AMD64,
        ("linux", "aarch64") => Build { emulated: true, ..LINUX_AMD64 },
        ("windows", "x86_64") => WINDOWS_X64,
        ("macos", "x86_64" | "aarch64") => MACOS,
        _ => bail!("the official TeamSpeak server has no build for {os}/{arch}"),
    })
}

/// Make sure the official server is unpacked under `root/server-<VERSION>/`, downloading it if needed.
pub async fn ensure_installed(root: &Path) -> Result<Installed> {
    let build = build_for(std::env::consts::OS, std::env::consts::ARCH)?;
    let dir = root.join(format!("server-{VERSION}"));
    let installed = Installed { dir: dir.clone(), emulated: build.emulated };
    if dir.join(MARKER).is_file() {
        return Ok(installed);
    }
    tokio::fs::create_dir_all(root).await.with_context(|| format!("create {}", root.display()))?;

    let id = std::process::id();
    let archive = root.join(format!(".download-{id}.tmp"));
    let staging = root.join(format!(".extract-{id}.tmp"));
    let result = install(&build, &archive, &staging, &dir).await;
    let _ = tokio::fs::remove_file(&archive).await;
    let _ = tokio::fs::remove_dir_all(&staging).await;
    if let Err(e) = result {
        // A concurrent installer may have won the race.
        if dir.join(MARKER).is_file() {
            return Ok(installed);
        }
        return Err(e);
    }
    Ok(installed)
}

async fn install(build: &Build, archive: &Path, staging: &Path, dir: &Path) -> Result<()> {
    tracing::info!("downloading the official TeamSpeak server {VERSION} from {}", build.url);
    let digest = download(build.url, archive).await?;
    verify_digest(&digest, build.sha256).with_context(|| format!("downloaded {}", build.url))?;

    let (archive_path, staging_path, format) = (archive.to_owned(), staging.to_owned(), build.format);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let _ = fs::remove_dir_all(&staging_path);
        fs::create_dir_all(&staging_path)?;
        match format {
            Format::TarBz2 => extract_tar_bz2(&archive_path, &staging_path)?,
            Format::Zip => extract_zip(&archive_path, &staging_path)?,
        }
        let exe = if cfg!(windows) { "ts3server.exe" } else { "ts3server" };
        ensure!(staging_path.join(exe).is_file(), "the archive does not contain {exe}");
        fs::write(staging_path.join(MARKER), &digest)?;
        Ok(())
    })
    .await??;

    if dir.exists() {
        // Left over from an interrupted or foreign install; the marker was checked by the caller.
        tokio::fs::remove_dir_all(dir).await.with_context(|| format!("remove stale {}", dir.display()))?;
    }
    tokio::fs::rename(staging, dir).await.with_context(|| format!("move the server into {}", dir.display()))?;
    tracing::info!("installed the official TeamSpeak server into {}", dir.display());
    Ok(())
}

/// Stream `url` into `dest`, returning the lowercase hex SHA-256 of the body.
async fn download(url: &str, dest: &Path) -> Result<String> {
    let response = reqwest::get(url).await?.error_for_status()?;
    let mut file = tokio::fs::File::create(dest).await.with_context(|| format!("create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("download interrupted")?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn verify_digest(actual: &str, expected: &str) -> Result<()> {
    ensure!(actual.eq_ignore_ascii_case(expected), "SHA-256 mismatch: expected {expected}, got {actual}");
    Ok(())
}

/// Destination for an archive member after dropping its top-level folder; `None` for the folder itself.
fn stripped_path(dest: &Path, name: &Path) -> Result<Option<PathBuf>> {
    let mut parts = name.components().filter(|c| !matches!(c, Component::CurDir));
    if !matches!(parts.next(), Some(Component::Normal(_)) | None) {
        bail!("unsafe path in archive: {}", name.display());
    }
    let mut out = dest.to_owned();
    let mut any = false;
    for part in parts {
        let Component::Normal(part) = part else { bail!("unsafe path in archive: {}", name.display()) };
        out.push(part);
        any = true;
    }
    Ok(any.then_some(out))
}

fn extract_tar_bz2(archive: &Path, dest: &Path) -> Result<()> {
    let file = BufReader::new(File::open(archive)?);
    let mut tar = tar::Archive::new(bzip2::read::BzDecoder::new(file));
    tar.set_preserve_permissions(true);
    for entry in tar.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.into_owned();
        let Some(target) = stripped_path(dest, &name)? else { continue };
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        entry.unpack(&target).with_context(|| format!("unpack {}", name.display()))?;
    }
    Ok(())
}

fn extract_zip(archive: &Path, dest: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(BufReader::new(File::open(archive)?))?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let Some(name) = entry.enclosed_name() else { bail!("unsafe path in archive: {:?}", entry.name()) };
        let Some(target) = stripped_path(dest, &name)? else { continue };
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        io::copy(&mut entry, &mut File::create(&target)?)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn sample_tar_bz2(path: &Path) {
        let enc = bzip2::write::BzEncoder::new(File::create(path).unwrap(), bzip2::Compression::fast());
        let mut tar = tar::Builder::new(enc);
        let mut dir = tar::Header::new_gnu();
        dir.set_entry_type(tar::EntryType::Directory);
        dir.set_mode(0o755);
        dir.set_size(0);
        tar.append_data(&mut dir, "top/", io::empty()).unwrap();
        for (name, body, mode) in
            [("top/ts3server", &b"#!/bin/sh\n"[..], 0o755), ("top/sql/a.sql", b"select 1;", 0o644)]
        {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(mode);
            tar.append_data(&mut h, name, body).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
    }

    #[test]
    fn tar_bz2_strips_top_level_and_keeps_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("a.tar.bz2");
        sample_tar_bz2(&archive);
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        extract_tar_bz2(&archive, &out).unwrap();
        assert_eq!(fs::read(out.join("sql/a.sql")).unwrap(), b"select 1;");
        assert!(!out.join("top").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(out.join("ts3server")).unwrap().permissions().mode() & 0o111, 0o111);
            assert_eq!(fs::metadata(out.join("sql/a.sql")).unwrap().permissions().mode() & 0o111, 0);
        }
    }

    #[test]
    fn zip_strips_top_level_and_keeps_modes() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("a.zip");
        let mut zip = zip::ZipWriter::new(File::create(&archive).unwrap());
        let opts = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
        zip.add_directory("top/", opts).unwrap();
        zip.start_file("top/ts3server", opts).unwrap();
        zip.write_all(b"bin").unwrap();
        zip.start_file("top/doc/readme.txt", opts.unix_permissions(0o644)).unwrap();
        zip.write_all(b"hi").unwrap();
        zip.finish().unwrap();
        let out = tmp.path().join("out");
        fs::create_dir(&out).unwrap();
        extract_zip(&archive, &out).unwrap();
        assert_eq!(fs::read(out.join("doc/readme.txt")).unwrap(), b"hi");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(out.join("ts3server")).unwrap().permissions().mode() & 0o777, 0o755);
            assert_eq!(fs::metadata(out.join("doc/readme.txt")).unwrap().permissions().mode() & 0o111, 0);
        }
    }

    #[test]
    fn rejects_path_traversal() {
        let dest = Path::new("/x");
        assert!(stripped_path(dest, Path::new("top/../../etc/passwd")).is_err());
        assert!(stripped_path(dest, Path::new("/abs/path")).is_err());
        assert_eq!(stripped_path(dest, Path::new("top/")).unwrap(), None);
        assert_eq!(stripped_path(dest, Path::new("./top/a/b")).unwrap(), Some(PathBuf::from("/x/a/b")));
    }

    #[test]
    fn digest_check() {
        let d = hex(&Sha256::digest(b"abc"));
        assert_eq!(d, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert!(verify_digest(&d, &d.to_uppercase()).is_ok());
        assert!(verify_digest(&d, &d.replace('b', "c")).is_err());
    }

    #[test]
    fn platform_table() {
        assert!(!build_for("linux", "x86_64").unwrap().emulated);
        let arm = build_for("linux", "aarch64").unwrap();
        assert!(arm.emulated && arm.url == LINUX_AMD64.url);
        assert_eq!(build_for("windows", "x86_64").unwrap().format, Format::Zip);
        assert!(build_for("freebsd", "x86_64").is_err());
        assert!(build_for("linux", "riscv64").is_err());
    }
}
