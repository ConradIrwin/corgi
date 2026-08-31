use anyhow::{bail, Context, Result};
use semver::Version;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const RELEASE_ROOT: &str = "https://github.com/ConradIrwin/corgi/releases/download";
const SWITCH_VERSION_ENV: &str = "CORGI_INTERNAL_SWITCH_VERSION";
static NEXT_INSTALL: AtomicU64 = AtomicU64::new(0);

/// Install and hand the invocation to the exact corgi named by the nearest
/// `corgi.toml` when this executable is older than that version.
pub fn update_if_required(start: &Path, argv: &[OsString]) -> Result<()> {
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("corgi was built with an invalid package version")?;
    if let Some(expected) = std::env::var_os(SWITCH_VERSION_ENV) {
        std::env::remove_var(SWITCH_VERSION_ENV);
        let expected = parse_version(&expected.to_string_lossy())?;
        if current != expected {
            bail!("invoked corgi {current} to provide corgi {expected}");
        }
    }

    let Some(required) = crate::build::configured_corgi_version(start)? else {
        return Ok(());
    };
    let required = parse_version(&required)?;
    if required <= current {
        return Ok(());
    }

    let installed = install(&crate::store::default_root()?, &required)?;
    let mut replacement = Command::new(&installed);
    replacement
        .args(argv.iter().skip(1))
        .env(SWITCH_VERSION_ENV, required.to_string());
    replace_process(replacement)
        .with_context(|| format!("running corgi {required} at {}", installed.display()))
}

fn parse_version(value: &str) -> Result<Version> {
    Version::parse(value).with_context(|| format!("invalid corgi version `{value}`"))
}

fn install(store_root: &Path, version: &Version) -> Result<PathBuf> {
    let store_root = if store_root.is_absolute() {
        store_root.to_path_buf()
    } else {
        std::env::current_dir()
            .context("reading the current directory")?
            .join(store_root)
    };
    fs::create_dir_all(&store_root)
        .with_context(|| format!("creating {}", store_root.display()))?;
    let store_root = store_root
        .canonicalize()
        .with_context(|| format!("resolving {}", store_root.display()))?;
    let target = release_target()?;
    install_from_release(&store_root, version, target)
}

fn release_target() -> Result<&'static str> {
    release_target_for(std::env::consts::OS, std::env::consts::ARCH).with_context(|| {
        format!(
            "automatic corgi installation is not available for {}-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        )
    })
}

fn release_target_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

fn release_asset(version: &Version, target: &str) -> (String, String, String) {
    let asset = format!("corgi-{target}.tar.gz");
    let root = format!("{RELEASE_ROOT}/v{version}");
    (
        asset.clone(),
        format!("{root}/{asset}"),
        format!("{root}/{asset}.sha256"),
    )
}

fn download(url: &str, destination: &Path) -> Result<()> {
    let status = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--output",
        ])
        .arg(destination)
        .arg(url)
        .status()
        .with_context(|| format!("starting curl to download {url}"))?;
    if !status.success() {
        bail!("downloading {url} failed with {status}");
    }
    Ok(())
}

fn parse_checksum(contents: &str, asset: &str) -> Result<String> {
    let mut fields = contents.split_whitespace();
    let checksum = fields.next().context("checksum file is empty")?;
    let filename = fields.next().context("checksum file has no asset name")?;
    if fields.next().is_some() {
        bail!("checksum file has unexpected trailing fields");
    }
    if filename != asset {
        bail!("checksum names `{filename}`, expected `{asset}`");
    }
    if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("checksum for {asset} is not a SHA-256 digest");
    }
    Ok(checksum.to_ascii_lowercase())
}

fn create_staging_directory(store_root: &Path) -> Result<PathBuf> {
    let parent = store_root.join("tmp");
    fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
    loop {
        let staging = parent.join(format!(
            "corgi-install-{}-{}",
            std::process::id(),
            NEXT_INSTALL.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&staging) {
            Ok(()) => return Ok(staging),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", staging.display()));
            }
        }
    }
}

fn install_from_release(store_root: &Path, version: &Version, target: &str) -> Result<PathBuf> {
    let destination = installation_root(store_root, version);
    let executable = installation_binary(store_root, version);
    if executable.is_file() {
        touch_used(&destination);
        return Ok(executable);
    }
    if destination.exists() {
        bail!("incomplete corgi installation at {}", destination.display());
    }

    fs::create_dir_all(destination.parent().unwrap())
        .with_context(|| format!("creating {}", destination.parent().unwrap().display()))?;
    let staging = create_staging_directory(store_root)?;

    let result = (|| {
        eprintln!("{:>12} corgi {version} ({target})", "Installing");
        let (asset, archive_url, checksum_url) = release_asset(version, target);
        let archive = staging.join(&asset);
        let checksum_file = staging.join(format!("{asset}.sha256"));
        download(&archive_url, &archive)?;
        download(&checksum_url, &checksum_file)?;

        let expected = parse_checksum(
            &fs::read_to_string(&checksum_file)
                .with_context(|| format!("reading {}", checksum_file.display()))?,
            &asset,
        )?;
        let actual = crate::store::sha256_file(&archive)?;
        if actual != expected {
            bail!("checksum mismatch for corgi {version}: expected {expected}, got {actual}");
        }

        let bin = staging.join("bin");
        fs::create_dir(&bin).with_context(|| format!("creating {}", bin.display()))?;
        let executable_filename = executable_name("corgi");
        let status = Command::new("tar")
            .args(["-xzf"])
            .arg(&archive)
            .arg("-C")
            .arg(&bin)
            // Extract only the expected root entry, not arbitrary archive paths.
            .arg(&executable_filename)
            .status()
            .with_context(|| format!("starting tar to unpack {archive_url}"))?;
        if !status.success() {
            bail!("unpacking corgi {version} failed with {status}");
        }

        let staged_executable = bin.join(executable_filename);
        let metadata = fs::symlink_metadata(&staged_executable).with_context(|| {
            format!(
                "corgi {version} archive did not contain {}",
                staged_executable.display()
            )
        })?;
        if !metadata.file_type().is_file() {
            bail!(
                "corgi {version} archive entry {} is not a regular file",
                staged_executable.display()
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&staged_executable, permissions)
                .with_context(|| format!("making {} executable", staged_executable.display()))?;
        }

        let validation_dir = staging.join("validate");
        fs::create_dir(&validation_dir)
            .with_context(|| format!("creating {}", validation_dir.display()))?;
        fs::write(validation_dir.join("corgi.toml"), "")
            .with_context(|| format!("creating validation corgi.toml for corgi {version}"))?;
        let output = Command::new(&staged_executable)
            .arg("--version")
            .current_dir(&validation_dir)
            .env(SWITCH_VERSION_ENV, version.to_string())
            .output()
            .with_context(|| format!("checking downloaded corgi {version}"))?;
        if !output.status.success() {
            bail!(
                "downloaded corgi {version} failed its version check with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let reported = String::from_utf8(output.stdout)
            .with_context(|| format!("downloaded corgi {version} printed a non-UTF-8 version"))?;
        if reported.trim() != format!("corgi {version}") {
            bail!(
                "downloaded corgi {version} reported unexpected version `{}`",
                reported.trim()
            );
        }

        fs::remove_file(&archive).with_context(|| format!("removing {}", archive.display()))?;
        fs::remove_file(&checksum_file)
            .with_context(|| format!("removing {}", checksum_file.display()))?;
        fs::remove_dir_all(&validation_dir)
            .with_context(|| format!("removing {}", validation_dir.display()))?;
        touch_used(&staging);

        match fs::rename(&staging, &destination) {
            Ok(()) => {}
            Err(_) if executable.is_file() => {
                // Another process won the race to publish the same immutable version.
                fs::remove_dir_all(&staging).ok();
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("publishing corgi {}", destination.display()));
            }
        }
        Ok(executable)
    })();
    if result.is_err() {
        fs::remove_dir_all(&staging).ok();
    }
    result
}

fn installation_root(store_root: &Path, version: &Version) -> PathBuf {
    store_root.join("tools").join(format!("corgi-{version}"))
}

fn installation_binary(store_root: &Path, version: &Version) -> PathBuf {
    installation_root(store_root, version)
        .join("bin")
        .join(executable_name("corgi"))
}

fn touch_used(directory: &Path) {
    let _ = fs::write(directory.join(".corgi-used"), b"used\n");
}

fn executable_name(name: &str) -> OsString {
    if cfg!(windows) {
        OsString::from(format!("{name}.exe"))
    } else {
        OsString::from(name)
    }
}

#[cfg(unix)]
fn replace_process(mut command: Command) -> Result<()> {
    use std::os::unix::process::CommandExt;
    Err(command.exec().into())
}

#[cfg(not(unix))]
fn replace_process(mut command: Command) -> Result<()> {
    let status = command.status()?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn temporary_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "corgi-self-update-{}-{}",
            std::process::id(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn release_target_uses_the_full_rust_triple() {
        assert_eq!(
            release_target_for("macos", "aarch64"),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(release_target_for("macos", "x86_64"), None);
        assert_eq!(release_target_for("linux", "aarch64"), None);
    }

    #[test]
    fn release_urls_name_the_version_and_target() {
        let version = Version::parse("1.2.3").unwrap();

        let (asset, archive_url, checksum_url) = release_asset(&version, "aarch64-apple-darwin");

        assert_eq!(asset, "corgi-aarch64-apple-darwin.tar.gz");
        assert_eq!(
            archive_url,
            "https://github.com/ConradIrwin/corgi/releases/download/v1.2.3/corgi-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(checksum_url, format!("{archive_url}.sha256"));
    }

    #[test]
    fn release_checksums_are_strictly_parsed() {
        let asset = "corgi-aarch64-apple-darwin.tar.gz";
        let checksum = "0123456789abcdef".repeat(4);

        assert_eq!(
            parse_checksum(&format!("{checksum}  {asset}\n"), asset).unwrap(),
            checksum
        );
        assert!(parse_checksum("", asset).is_err());
        assert!(parse_checksum(&format!("{checksum}  another-file\n"), asset).is_err());
        assert!(parse_checksum(&format!("not-a-digest  {asset}\n"), asset).is_err());
        assert!(parse_checksum(&format!("{checksum}  {asset} extra\n"), asset).is_err());
    }

    #[test]
    fn discovers_version_from_nearest_parent_manifest() {
        let root = temporary_directory();
        let member = root.join("nested/member");
        fs::create_dir_all(&member).unwrap();
        fs::write(
            root.join("corgi.toml"),
            "corgi_version = \"12.3.4\"\n[future]\nsetting = true\n",
        )
        .unwrap();

        let version = crate::build::configured_corgi_version(&member)
            .unwrap()
            .unwrap();

        assert_eq!(version, "12.3.4");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pin_creates_a_manifest_and_preserves_existing_content() {
        let root = temporary_directory();
        let member = root.join("member");
        fs::create_dir_all(&member).unwrap();

        let created = crate::build::pin_corgi_version(&member, "1.2.3").unwrap();
        assert_eq!(created, member.join("corgi.toml"));
        assert_eq!(
            fs::read_to_string(&created).unwrap(),
            "corgi_version = \"1.2.3\"\n"
        );

        fs::write(
            &created,
            "# keep this comment\ncorgi_version = \"1.2.3\"\n[future]\nsetting = true\n",
        )
        .unwrap();
        crate::build::pin_corgi_version(&member, "2.0.0").unwrap();
        let updated = fs::read_to_string(&created).unwrap();
        assert!(updated.contains("# keep this comment"));
        assert!(updated.contains("corgi_version = \"2.0.0\""));
        assert!(updated.contains("[future]\nsetting = true"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn nearest_manifest_without_version_disables_parent_declaration() {
        let root = temporary_directory();
        let member = root.join("member");
        fs::create_dir_all(&member).unwrap();
        fs::write(root.join("corgi.toml"), "corgi_version = \"12.3.4\"\n").unwrap();
        fs::write(member.join("corgi.toml"), "[roots]\n").unwrap();

        assert!(crate::build::configured_corgi_version(&member)
            .unwrap()
            .is_none());
        fs::remove_dir_all(root).unwrap();
    }
}
