use anyhow::{bail, Context, Result};
use semver::Version;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

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
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    install_with_cargo(&store_root, version, &cargo)
}

fn install_with_cargo(
    store_root: &Path,
    version: &Version,
    cargo: &std::ffi::OsStr,
) -> Result<PathBuf> {
    let destination = installation_root(store_root, version);
    let executable = installation_binary(store_root, version);
    if executable.is_file() {
        touch_used(&destination);
        return Ok(executable);
    }
    if destination.exists() {
        bail!("incomplete corgi installation at {}", destination.display());
    }

    let staging = store_root.join("tmp").join(format!(
        "corgi-install-{}-{}",
        std::process::id(),
        NEXT_INSTALL.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(staging.parent().unwrap())
        .with_context(|| format!("creating {}", staging.parent().unwrap().display()))?;
    fs::create_dir_all(destination.parent().unwrap())
        .with_context(|| format!("creating {}", destination.parent().unwrap().display()))?;
    fs::remove_dir_all(&staging).ok();
    fs::create_dir(&staging).with_context(|| format!("creating {}", staging.display()))?;

    eprintln!("{:>12} corgi {version}", "Installing");
    let status = Command::new(cargo)
        .args(["install", "corgi-build", "--locked", "--version"])
        .arg(format!("={version}"))
        .arg("--root")
        .arg(&staging)
        // rustup selects cargo's toolchain from the parent process's working
        // directory, not from the downloaded package. A project may pin a
        // Rust version too old to compile the corgi it requires.
        .current_dir(&staging)
        .status();
    let status = match status {
        Ok(status) => status,
        Err(error) => {
            fs::remove_dir_all(&staging).ok();
            return Err(error)
                .with_context(|| format!("starting cargo to install corgi {version}"));
        }
    };
    if !status.success() {
        fs::remove_dir_all(&staging).ok();
        bail!("installing corgi {version} failed with {status}");
    }
    let staged_executable = staging.join("bin").join(executable_name("corgi"));
    if !staged_executable.is_file() {
        fs::remove_dir_all(&staging).ok();
        bail!(
            "installing corgi {version} did not produce {}",
            staged_executable.display()
        );
    }
    touch_used(&staging);

    match fs::rename(&staging, &destination) {
        Ok(()) => {}
        Err(_) if executable.is_file() => {
            // Another process won the race to publish the same immutable version.
            fs::remove_dir_all(&staging).ok();
        }
        Err(error) => {
            fs::remove_dir_all(&staging).ok();
            return Err(error)
                .with_context(|| format!("publishing corgi {}", destination.display()));
        }
    }
    Ok(executable)
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
