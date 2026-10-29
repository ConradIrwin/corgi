//! Pinned native libraries assembled into a per-target sysroot.
//!
//! Build scripts and links cannot see host libraries inside the sandbox, so a
//! project declares the shared libraries it links against in `corgi.toml`:
//!
//! ```toml
//! [libraries.libxcb1]
//! url = "https://snapshot.debian.org/.../libxcb1_1.14-3_amd64.deb"
//! sha256 = "..."
//! targets = ["x86_64-unknown-linux-gnu"]
//! ```
//!
//! Each entry is a `.deb` package or a tarball. Every entry serving a target is
//! unpacked into one sysroot in the store, because libraries refer to each
//! other (`fontconfig.pc` requires `freetype2`). The sysroot's identity is the
//! hash of its pins; the Zig wrappers carry it, so it reaches the toolchain
//! identity that build scripts and links already key on. Corgi's `pkg-config`
//! reads the sysroot's `.pc` files, and the link driver adds its library
//! directories to the search path.

use anyhow::{bail, ensure, Context, Result};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::store::{sha256_hex, Store};

/// Bumped when unpacking changes what a given set of pins produces.
const LAYOUT_VERSION: &str = "corgi-sysroot-1";

/// Written last into a finished sysroot.
const COMPLETE_MARKER: &str = ".corgi-sysroot";

/// One `[libraries.<name>]` entry in `corgi.toml`.
#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct LibrarySpec {
    #[serde(skip)]
    pub name: String,
    pub url: String,
    pub sha256: String,
    /// Compilation targets whose sysroot includes this archive; empty = all.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Download transport, as for `[tools]`: empty or "github".
    #[serde(default)]
    pub auth: String,
}

impl LibrarySpec {
    pub fn serves_platform(&self, platform: &str) -> bool {
        self.targets.is_empty() || self.targets.iter().any(|target| target == platform)
    }
}

/// An unpacked library sysroot in the store.
pub struct Sysroot {
    pub root: PathBuf,
    /// Hash of the pins that produced `root`.
    pub identity: String,
}

/// Assemble the sysroot for `platform` from the libraries serving it.
///
/// Returns `None` when no library serves `platform`.
///
/// # Errors
///
/// Fails when a download, checksum, or unpack fails.
pub fn ensure_sysroot<'a>(
    store: &Store,
    libraries: impl IntoIterator<Item = &'a LibrarySpec>,
    platform: &str,
) -> Result<Option<Sysroot>> {
    let mut selected = libraries
        .into_iter()
        .filter(|library| library.serves_platform(platform))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Ok(None);
    }
    selected.sort_by(|left, right| left.name.cmp(&right.name));
    let pins = selected
        .iter()
        .map(|library| (&library.name, &library.url, &library.sha256))
        .collect::<Vec<_>>();
    let identity = sha256_hex(&serde_json::to_vec(&(LAYOUT_VERSION, pins))?);
    let root = store.root.join("sysroots").join(&identity[..16]);
    if root.join(COMPLETE_MARKER).is_file() {
        Store::touch_used(&root);
        return Ok(Some(Sysroot { root, identity }));
    }

    crate::build::status!(
        "Installing",
        "{} libraries for {platform} (sha256-pinned)",
        selected.len()
    );
    let work = store.tmp_path("sysroot");
    let staging = work.join("root");
    fs::create_dir_all(&staging)?;
    for library in &selected {
        let archive = work.join(format!("{}.archive", library.name));
        crate::build::download_pinned(
            "library",
            &library.name,
            &library.url,
            &library.auth,
            &library.sha256,
            &archive,
        )?;
        unpack_archive(&archive, &staging, &work)
            .with_context(|| format!("unpacking library {}", library.name))?;
    }
    relativize_symlinks(&staging, &staging)?;
    fs::write(staging.join(COMPLETE_MARKER), &identity)?;
    fs::create_dir_all(root.parent().context("sysroot has no parent")?)?;
    match fs::rename(&staging, &root) {
        Ok(()) => {}
        Err(_) if root.join(COMPLETE_MARKER).is_file() => {}
        Err(error) => return Err(error).context("publishing library sysroot"),
    }
    fs::remove_dir_all(&work).ok();
    Ok(Some(Sysroot { root, identity }))
}

/// The Debian multiarch directory name for a Zig target such as
/// `x86_64-linux-gnu` or `x86_64-linux-gnu.2.31`.
pub fn multiarch(zig_target: &str) -> &str {
    zig_target
        .split_once('.')
        .map_or(zig_target, |(triple, _)| triple)
}

/// Directories holding the sysroot's libraries, most specific first.
pub fn library_directories(root: &Path, multiarch: &str) -> Vec<PathBuf> {
    [
        format!("usr/lib/{multiarch}"),
        format!("lib/{multiarch}"),
        "usr/lib".to_string(),
        "lib".to_string(),
    ]
    .into_iter()
    .map(|relative| root.join(relative))
    .filter(|directory| directory.is_dir())
    .collect()
}

/// Directories holding the sysroot's `.pc` files, in search order.
pub fn pkg_config_directories(root: &Path, multiarch: &str) -> Vec<PathBuf> {
    library_directories(root, multiarch)
        .into_iter()
        .map(|directory| directory.join("pkgconfig"))
        .chain([root.join("usr/share/pkgconfig")])
        .filter(|directory| directory.is_dir())
        .collect()
}

/// Unpack a `.deb` (its `data.tar.*` member) or a tarball into `destination`.
fn unpack_archive(archive: &Path, destination: &Path, work: &Path) -> Result<()> {
    let mut file = File::open(archive)?;
    let mut magic = [0u8; 8];
    let is_deb = file.read_exact(&mut magic).is_ok() && &magic == b"!<arch>\n";
    let tarball = if is_deb {
        extract_deb_data(&mut file, work)?
    } else {
        archive.to_path_buf()
    };
    // GNU tar and bsdtar both detect the compression from the content.
    let status = Command::new("tar")
        .arg("-xf")
        .arg(&tarball)
        .arg("-C")
        .arg(destination)
        .status()
        .context("running tar")?;
    ensure!(status.success(), "tar failed on {}", tarball.display());
    Ok(())
}

/// Copy the `data.tar.*` member out of a `.deb` (an `ar` archive) into `work`.
///
/// `file` must be positioned just after the `!<arch>\n` magic.
fn extract_deb_data(file: &mut File, work: &Path) -> Result<PathBuf> {
    let mut header = [0u8; 60];
    while file.read_exact(&mut header).is_ok() {
        let name = std::str::from_utf8(&header[..16])?.trim_end();
        let name = name.trim_end_matches('/');
        let size: u64 = std::str::from_utf8(&header[48..58])?
            .trim()
            .parse()
            .context("malformed ar member size")?;
        if name.starts_with("data.tar") {
            ensure!(
                !name.contains('/') && !name.contains(".."),
                "unexpected .deb member name `{name}`"
            );
            let output = work.join(name);
            let mut writer = File::create(&output)?;
            let copied = std::io::copy(&mut file.take(size), &mut writer)?;
            ensure!(copied == size, "truncated .deb member `{name}`");
            return Ok(output);
        }
        // Members are padded to an even length.
        file.seek(SeekFrom::Current((size + size % 2) as i64))?;
    }
    bail!("the .deb has no data.tar member")
}

/// Rewrite absolute symlinks under `directory` to point inside `root`.
///
/// Packages use absolute targets such as `/lib/x86_64-linux-gnu/libz.so.1`,
/// which would otherwise resolve against the host (or nothing, in the sandbox).
fn relativize_symlinks(root: &Path, directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.is_dir() {
            relativize_symlinks(root, &path)?;
        } else if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            if target.is_absolute() {
                let depth = path
                    .parent()
                    .context("symlink has no parent")?
                    .strip_prefix(root)?
                    .components()
                    .filter(|component| matches!(component, Component::Normal(_)))
                    .count();
                let mut relative = PathBuf::new();
                for _ in 0..depth {
                    relative.push("..");
                }
                relative.push(target.strip_prefix("/")?);
                fs::remove_file(&path)?;
                std::os::unix::fs::symlink(&relative, &path)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn deb_data_unpacks_with_absolute_symlinks_kept_inside_the_root() {
        let work =
            std::env::temp_dir().join(format!("corgi-libraries-test-{}", std::process::id()));
        let payload = work.join("payload");
        let library = payload.join("usr/lib/x86_64-linux-gnu");
        fs::create_dir_all(&library).unwrap();
        fs::write(library.join("libz.so.1.2.11"), "elf").unwrap();
        // zlib1g-dev links its .so to the runtime file by absolute path.
        std::os::unix::fs::symlink(
            "/usr/lib/x86_64-linux-gnu/libz.so.1.2.11",
            library.join("libz.so"),
        )
        .unwrap();
        let data = work.join("data.tar.gz");
        assert!(Command::new("tar")
            .arg("-czf")
            .arg(&data)
            .arg("-C")
            .arg(&payload)
            .arg(".")
            .status()
            .unwrap()
            .success());
        let deb = work.join("zlib.deb");
        let mut writer = File::create(&deb).unwrap();
        writer.write_all(b"!<arch>\n").unwrap();
        // An odd-length member exercises ar's even padding.
        for (name, contents) in [
            ("debian-binary", b"2.0\n\n".to_vec()),
            ("data.tar.gz", fs::read(&data).unwrap()),
        ] {
            write!(
                writer,
                "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                0,
                0,
                0,
                100644,
                contents.len()
            )
            .unwrap();
            writer.write_all(&contents).unwrap();
            if contents.len() % 2 == 1 {
                writer.write_all(b"\n").unwrap();
            }
        }
        drop(writer);

        let root = work.join("root");
        fs::create_dir_all(&root).unwrap();
        unpack_archive(&deb, &root, &work).unwrap();
        relativize_symlinks(&root, &root).unwrap();

        let link = root.join("usr/lib/x86_64-linux-gnu/libz.so");
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("../../../usr/lib/x86_64-linux-gnu/libz.so.1.2.11")
        );
        assert_eq!(fs::read_to_string(&link).unwrap(), "elf");
        assert_eq!(
            library_directories(&root, multiarch("x86_64-linux-gnu.2.31")),
            vec![root.join("usr/lib/x86_64-linux-gnu"), root.join("usr/lib")]
        );
        fs::remove_dir_all(&work).unwrap();
    }
}
