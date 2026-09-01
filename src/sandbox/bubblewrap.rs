//! Linux confinement, via `bwrap` (bubblewrap) mount namespaces.
//!
//! Linux has no path-rule filter equivalent to seatbelt, so the policy is
//! enforced by construction instead: the action runs in a fresh mount
//! namespace containing *only* the paths it is allowed to touch, bound
//! read-only or read-write as appropriate. Everything else is simply absent,
//! which also serves as the exec allowlist — an unbound binary cannot be run
//! because there is no file to run. The network namespace is unshared, so no
//! socket can reach off-machine.

use super::{Environment, Sandbox, UNHASHED_ENTRIES};
use anyhow::{bail, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

const BWRAP: &str = "bwrap";

/// System directories an action may read. These hold the dynamic loader and
/// the C library that the toolchain's own binaries need, plus the machine
/// description tools consult (`/proc`, `/sys`, `/etc`). Deliberately absent:
/// `/usr/bin` and friends, so no ambient tool is runnable.
const SYSTEM_READS: [&str; 6] = ["/etc", "/sys", "/usr/lib", "/usr/lib64", "/lib", "/lib64"];

pub fn ensure_available() -> Result<()> {
    let found = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join(BWRAP).is_file()))
        .unwrap_or(false);
    if !found {
        bail!(
            "corgi builds require `{BWRAP}` on PATH (install the bubblewrap package, \
             e.g. `apt install bubblewrap` or `dnf install bubblewrap`)"
        );
    }
    Ok(())
}

pub struct Bubblewrap {
    environment: Environment,
}

impl Bubblewrap {
    pub fn new(environment: Environment) -> Result<Self> {
        ensure_available()?;
        Ok(Self { environment })
    }

    /// Every spelling of `path` that must resolve inside the namespace. A
    /// store reached through its canonical alias has two: actions are handed
    /// the alias spelling, while corgi itself works in physical paths, and
    /// both have to name the same directory.
    fn spellings(&self, path: &Path) -> Vec<PathBuf> {
        let mut spellings = vec![path.to_path_buf()];
        let Some(alias) = &self.environment.store_alias else {
            return spellings;
        };
        let root = &self.environment.store_root;
        if let Ok(relative) = path.strip_prefix(root) {
            spellings.push(alias.join(relative));
        } else if let Ok(relative) = path.strip_prefix(alias) {
            spellings.push(root.join(relative));
        }
        spellings
    }
}

impl Sandbox for Bubblewrap {
    fn command(
        &self,
        program: &Path,
        working_directory: &Path,
        reads: &[&Path],
        writes: &[&Path],
    ) -> Command {
        let environment = &self.environment;
        let mut arguments: Vec<OsString> = Vec::new();

        for directory in SYSTEM_READS {
            bind(&mut arguments, READ_ONLY, Path::new(directory));
        }
        // Shell-based build steps are as keyed as the script that spawns
        // them; the shell finds nothing on PATH that is not already keyed.
        bind(&mut arguments, READ_ONLY, Path::new("/bin/sh"));

        // The store carries every keyed input: the pinned toolchain, the
        // dependency sources under its cargo home, pinned tools, and the
        // artifact pool. Read-only in bulk; the action's own outputs are
        // rebound writable below.
        for path in [
            Path::new(&environment.sysroot),
            Path::new(&environment.cargo_home),
            Path::new(&environment.rustup_home),
        ]
        .into_iter()
        .chain(environment.store_roots())
        {
            bind(&mut arguments, READ_ONLY, path);
        }

        // Nothing under the workspace is visible except the reads granted
        // below, but the root itself has to exist: tools walk up to it and
        // resolve paths against it.
        arguments.push("--dir".into());
        arguments.push(environment.workspace_root.as_str().into());

        for read in reads {
            for path in readable_paths(read) {
                bind(&mut arguments, READ_ONLY, &path);
            }
        }
        for write in writes {
            for path in self.spellings(write) {
                bind(&mut arguments, READ_WRITE, &path);
            }
        }
        bind(&mut arguments, READ_ONLY, program);

        let mut command = Command::new(BWRAP);
        command
            .arg("--unshare-all")
            .arg("--die-with-parent")
            .arg("--proc")
            .arg("/proc")
            .arg("--dev")
            .arg("/dev")
            .args(arguments)
            .arg("--chdir")
            .arg(working_directory)
            .arg("--")
            .arg(program)
            .current_dir(working_directory);
        command
    }

    fn description(&self) -> &'static str {
        "bubblewrap"
    }
}

const READ_ONLY: &str = "--ro-bind-try";
const READ_WRITE: &str = "--bind";

/// Make `path` visible at its own spelling inside the namespace. Order
/// matters: a later bind over a subpath of an earlier one wins, which is how
/// an action's output directories become writable inside the read-only store.
fn bind(arguments: &mut Vec<OsString>, mode: &str, path: &Path) {
    arguments.push(mode.into());
    arguments.push(path.into());
    arguments.push(path.into());
}

/// The paths to bind so that `read` is readable but the entries the source
/// hash ignores are not. A directory holding none of them binds whole;
/// otherwise its entries bind one by one, leaving the ignored ones absent.
fn readable_paths(read: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(read) else {
        return vec![read.to_path_buf()];
    };
    let entries: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    let hides_nothing = !entries.iter().any(|entry| {
        entry
            .file_name()
            .is_some_and(|name| UNHASHED_ENTRIES.iter().any(|ignored| name == *ignored))
    });
    if hides_nothing {
        return vec![read.to_path_buf()];
    }
    entries
        .into_iter()
        .filter(|entry| {
            entry
                .file_name()
                .is_none_or(|name| !UNHASHED_ENTRIES.iter().any(|ignored| name == *ignored))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::readable_paths;
    use std::fs;

    #[test]
    fn a_directory_without_unhashed_entries_binds_whole() {
        let directory = temporary_directory("plain");
        fs::create_dir(directory.join("src")).unwrap();
        fs::write(directory.join("Cargo.toml"), "").unwrap();

        assert_eq!(readable_paths(&directory), vec![directory.clone()]);
    }

    #[test]
    fn unhashed_entries_are_left_out_of_the_namespace() {
        let directory = temporary_directory("unhashed");
        for entry in ["src", ".git", "target"] {
            fs::create_dir(directory.join(entry)).unwrap();
        }
        for entry in ["Cargo.toml", "Cargo.lock"] {
            fs::write(directory.join(entry), "").unwrap();
        }

        let mut bound = readable_paths(&directory);
        bound.sort();
        assert_eq!(
            bound,
            vec![directory.join("Cargo.toml"), directory.join("src")]
        );
    }

    #[test]
    fn a_file_binds_as_itself() {
        let directory = temporary_directory("file");
        let file = directory.join("icon.png");
        fs::write(&file, "").unwrap();

        assert_eq!(readable_paths(&file), vec![file]);
    }

    fn temporary_directory(tag: &str) -> std::path::PathBuf {
        let directory =
            std::env::temp_dir().join(format!("corgi-bubblewrap-{tag}-{}", std::process::id()));
        fs::remove_dir_all(&directory).ok();
        fs::create_dir_all(&directory).unwrap();
        directory
    }
}
