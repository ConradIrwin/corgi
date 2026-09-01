//! macOS confinement, via seatbelt profiles handed to `sandbox-exec`.
//!
//! Seatbelt filters operations on the real filesystem, so the policy is a
//! deny-by-default profile with allow rules naming absolute paths. Later
//! rules win, which is how the read allowances are trimmed back around
//! directories the source hash deliberately ignores.

use super::{Environment, Sandbox, UNHASHED_ENTRIES};
use anyhow::{bail, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

pub fn ensure_available() -> Result<()> {
    if !Path::new(SANDBOX_EXEC).is_file() {
        bail!("corgi builds require {SANDBOX_EXEC}");
    }
    Ok(())
}

pub struct Seatbelt {
    environment: Environment,
    /// The active Xcode or Command Line Tools directory.
    developer_dir: String,
    /// Canonical darwin per-user temp/cache dirs: xcrun, clang, and ld use
    /// these regardless of `$TMPDIR`, and without them every link takes a
    /// ~1.5s slow path.
    per_user_dirs: Vec<String>,
}

impl Seatbelt {
    pub fn new(environment: Environment) -> Result<Self> {
        ensure_available()?;
        let developer_dir = capture(Command::new("/usr/bin/xcode-select").arg("-p"))
            .map(|output| output.trim().to_string())
            .unwrap_or_else(|| "/Library/Developer/CommandLineTools".to_string());
        let mut per_user_dirs = Vec::new();
        for key in ["DARWIN_USER_TEMP_DIR", "DARWIN_USER_CACHE_DIR"] {
            let Some(directory) = capture(Command::new("/usr/bin/getconf").arg(key)) else {
                continue;
            };
            let directory = directory.trim().trim_end_matches('/').to_string();
            if directory.is_empty() {
                continue;
            }
            per_user_dirs.push(if directory.starts_with("/var/") {
                format!("/private{directory}")
            } else {
                directory
            });
        }
        Ok(Self {
            environment,
            developer_dir,
            per_user_dirs,
        })
    }

    fn profile(&self, reads: &[&Path], writes: &[&Path]) -> String {
        let environment = &self.environment;
        let mut prof = String::from(concat!(
            "(version 1)\n",
            "(deny default)\n",
            "(allow process-fork)\n",
            "(allow process-info*)\n",
            "(allow file-map-executable)\n",
            "(allow signal (target same-sandbox))\n",
            "(allow sysctl-read)\n",
            "(allow mach-lookup)\n",
            "(allow file-read-metadata)\n",
        ));
        // exec allowlist: the *only* runnable binaries are dispatchers
        // (/usr/bin/cc, the rustup shim) and tools whose identity is part of
        // the action key (rustc, clang/ld via the toolchain hash, build
        // scripts via their content hash)
        prof.push_str("(allow process-exec*\n");
        prof.push_str("  (literal \"/usr/bin/cc\")\n");
        let exec_lits = vec![
            format!("{}/bin/rustc", environment.cargo_home),
            format!("{}/bin/rustc", environment.sysroot),
        ];
        // the whole pinned-toolchain bin dir is keyed content (e.g.
        // proc-macro-crate spawns `cargo locate-project` at macro expansion)
        let toolchain_bin = Path::new(&environment.sysroot).join("bin");
        if let Ok(canon) = fs::canonicalize(&toolchain_bin) {
            prof.push_str(&format!("  (subpath \"{}\")\n", canon.display()));
        }
        // rust-lld & friends live under lib/rustlib/<triple>/bin — also keyed
        let rustlib = Path::new(&environment.sysroot).join("lib/rustlib");
        if let Ok(canon) = fs::canonicalize(&rustlib) {
            prof.push_str(&format!("  (subpath \"{}\")\n", canon.display()));
        }
        for p in exec_lits {
            // seatbelt matches canonical paths: ~/.cargo/bin/rustc is a symlink
            // to rustup, so resolve before emitting the rule
            let canon = fs::canonicalize(&p)
                .map(|c| c.display().to_string())
                .unwrap_or(p);
            prof.push_str(&format!("  (literal \"{canon}\")\n"));
        }
        prof.push_str(&format!(
            "  (subpath \"{}/Toolchains\")\n",
            self.developer_dir
        ));
        // the Apple tool group, keyed collectively via the Xcode identity in
        // the toolchain hash
        for p in [
            "/usr/bin/ar",
            "/usr/bin/ranlib",
            "/usr/bin/clang",
            "/usr/bin/clang++",
            "/usr/bin/c++",
            "/usr/bin/xcrun",
            "/usr/bin/xcodebuild",
            "/usr/bin/xcode-select",
            "/bin/sh",
        ] {
            prof.push_str(&format!("  (literal \"{p}\")\n"));
        }
        prof.push_str("  (subpath \"/private/var/run/com.apple.security.cryptexd\")\n");
        for root in environment.store_roots() {
            prof.push_str(&format!(
                "  (subpath \"{}\")\n",
                root.join("tools").display()
            ));
        }
        // actions may execute binaries they just built in their own writable
        // dirs (autoconf/aws-lc style compile-and-run probes): those binaries
        // are products of keyed inputs, so this stays hermetic
        for w in writes {
            prof.push_str(&format!("  (subpath \"{}\")\n", w.display()));
        }
        for root in environment.store_roots() {
            prof.push_str(&format!(
                "  (subpath \"{}\")\n",
                root.join("pool").display()
            ));
        }
        prof.push_str(")\n");
        prof.push_str("(allow file-read*\n  (literal \"/\")\n  (literal \"/dev/null\")\n  (literal \"/dev/urandom\")\n  (literal \"/dev/random\")\n  (literal \"/dev/zero\")\n");
        // Compiles run with cwd = the workspace root (cargo's shape); getcwd
        // needs the directory node itself, but nothing under it beyond the
        // explicitly granted package/extra-input subpaths.
        prof.push_str(&format!("  (literal \"{}\")\n", environment.workspace_root));
        for p in [
            "/usr",
            "/bin",
            "/sbin",
            "/System",
            "/Library",
            "/Applications",
            "/opt",
            "/private/etc",
            "/private/var/db",
            "/private/preboot",
            "/private/var/run/com.apple.security.cryptexd",
        ] {
            prof.push_str(&format!("  (subpath \"{p}\")\n"));
        }
        let mut reads_allowed: Vec<String> = vec![
            environment.sysroot.clone(),
            environment.cargo_home.clone(),
            environment.rustup_home.clone(),
            self.developer_dir.clone(),
        ];
        reads_allowed.extend(
            environment
                .store_roots()
                .map(|root| root.display().to_string()),
        );
        for d in &self.per_user_dirs {
            reads_allowed.push(d.clone());
        }
        for r in reads_allowed {
            prof.push_str(&format!("  (subpath \"{r}\")\n"));
        }
        let workspace_root = Path::new(&environment.workspace_root);
        let mut input_directories = std::collections::BTreeSet::new();
        for path in reads {
            let mut ancestor = path.parent();
            while let Some(directory) = ancestor {
                if !directory.starts_with(workspace_root) {
                    break;
                }
                input_directories.insert(directory);
                ancestor = directory.parent();
            }
        }
        for directory in input_directories {
            prof.push_str(&format!("  (literal \"{}\")\n", directory.display()));
        }
        for path in reads {
            let operation = if path.is_dir() { "subpath" } else { "literal" };
            prof.push_str(&format!("  ({operation} \"{}\")\n", path.display()));
        }
        prof.push_str(")\n");
        // Align the readable set with the hashed set: the source hash deliberately
        // excludes .git, build output dirs, and Cargo.lock, so reading them must
        // be denied or they become unhashed inputs. Later SBPL rules win.
        prof.push_str("(deny file-read* file-read-metadata\n");
        let mut deny_roots: Vec<String> = vec![environment.workspace_root.clone()];
        for path in reads {
            if path.is_dir() {
                deny_roots.push(path.display().to_string());
            }
        }
        deny_roots.sort();
        deny_roots.dedup();
        for r in &deny_roots {
            for entry in UNHASHED_ENTRIES {
                let operation = if entry == "Cargo.lock" {
                    "literal"
                } else {
                    "subpath"
                };
                prof.push_str(&format!("  ({operation} \"{r}/{entry}\")\n"));
            }
        }
        prof.push_str(")\n(allow file-write*\n  (literal \"/dev/null\")\n");
        for d in &self.per_user_dirs {
            prof.push_str(&format!("  (subpath \"{}\")\n", d));
        }
        for w in writes {
            prof.push_str(&format!("  (subpath \"{}\")\n", w.display()));
        }
        prof.push_str(")\n");
        prof
    }
}

impl Sandbox for Seatbelt {
    fn command(
        &self,
        program: &Path,
        working_directory: &Path,
        reads: &[&Path],
        writes: &[&Path],
    ) -> Command {
        let mut command = Command::new(SANDBOX_EXEC);
        command
            .arg("-p")
            .arg(self.profile(reads, writes))
            .arg(program)
            .current_dir(working_directory);
        command
    }

    fn description(&self) -> &'static str {
        "seatbelt"
    }
}

fn capture(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}
