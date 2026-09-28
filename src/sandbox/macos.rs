//! Deny-by-default Darwin build sandboxes using Seatbelt.
//!
//! Commands can read keyed inputs and runtime dependencies, and write only
//! action outputs and Darwin's per-user caches. Children inherit the sandbox.

use crate::store::Store;
use anyhow::{ensure, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct MacosSandbox {
    pub sysroot: PathBuf,
    pub rustc: PathBuf,
    pub cargo_home: PathBuf,
    pub rustup_home: PathBuf,
    pub store_root: PathBuf,
    pub pool: PathBuf,
    pub runtime_paths: Vec<PathBuf>,
    pub cache_directories: Vec<String>,
}

/// Fail before provisioning tools on a host without Seatbelt.
pub fn check_macos_sandbox() -> Result<()> {
    require_sandbox_executable(Path::new("/usr/bin/sandbox-exec"))
}

fn require_sandbox_executable(path: &Path) -> Result<()> {
    ensure!(path.is_file(), "corgi builds require {}", path.display());
    Ok(())
}

pub fn setup_macos_sandbox(
    store: &Store,
    rustc: &Path,
    sysroot: &Path,
    cargo_home: &Path,
    runtime_paths: Vec<PathBuf>,
) -> Result<MacosSandbox> {
    check_macos_sandbox()?;
    let home = std::env::var("HOME").unwrap_or_default();
    let rustup_home = std::env::var("RUSTUP_HOME").unwrap_or_else(|_| format!("{home}/.rustup"));
    // xcrun/clang/ld use these regardless of TMPDIR; omitting them makes links
    // take a slow path. Failed probes remain optional.
    let mut cache_directories = Vec::new();
    for key in ["DARWIN_USER_TEMP_DIR", "DARWIN_USER_CACHE_DIR"] {
        if let Ok(output) = Command::new("/usr/bin/getconf").arg(key).output() {
            if output.status.success() {
                if let Some(directory) =
                    canonical_cache_directory(&String::from_utf8_lossy(&output.stdout))
                {
                    cache_directories.push(directory);
                }
            }
        }
    }
    Ok(MacosSandbox {
        sysroot: sysroot.to_owned(),
        rustc: rustc.to_owned(),
        cargo_home: cargo_home.to_owned(),
        rustup_home: rustup_home.into(),
        store_root: store.root.clone(),
        pool: store.root.join("pool"),
        runtime_paths,
        cache_directories,
    })
}

fn canonical_cache_directory(raw: &str) -> Option<String> {
    let directory = raw.trim().trim_end_matches('/');
    if directory.is_empty() {
        None
    } else if directory.starts_with("/var/") {
        Some(format!("/private{directory}"))
    } else {
        Some(directory.to_string())
    }
}

fn path_filter(operation: &str, path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("sandbox paths must be valid UTF-8: {}", path.display()))?;
    // Reject control characters rather than relying on Seatbelt's escape dialect.
    ensure!(
        !path.chars().any(char::is_control),
        "sandbox paths must not contain control characters: {path:?}"
    );
    let escaped = path.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("  ({operation} \"{escaped}\")\n"))
}

impl MacosSandbox {
    /// Construct a sandbox command; the caller supplies arguments and environment.
    pub fn command(
        &self,
        program: &str,
        workspace: &Path,
        extra_reads: &[&Path],
        writes: &[&Path],
    ) -> Result<Command> {
        let mut profile = String::from(concat!(
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
        let runtime_filters = self
            .runtime_paths
            .iter()
            .map(|path| {
                let operation = if path.is_dir() { "subpath" } else { "literal" };
                path_filter(operation, path)
            })
            .collect::<Result<String>>()?;
        // Only the rustup shim and keyed tools may execute. The whole pinned
        // bin directory is keyed (proc macros may spawn cargo locate-project).
        profile.push_str("(allow process-exec*\n");
        let mut executables = vec![
            self.cargo_home.join("bin/rustc"),
            self.sysroot.join("bin/rustc"),
        ];
        if let Ok(rustc) = fs::canonicalize(&self.rustc) {
            executables.push(rustc);
        }
        for directory in [self.sysroot.join("bin"), self.sysroot.join("lib/rustlib")] {
            // rust-lld and friends live under lib/rustlib/<triple>/bin.
            if let Ok(canonical) = fs::canonicalize(directory) {
                profile.push_str(&path_filter("subpath", &canonical)?);
            }
        }
        profile.push_str(&runtime_filters);
        for path in executables {
            // Seatbelt matches canonical paths, including the rustc -> rustup shim.
            let canonical = fs::canonicalize(&path).unwrap_or(path);
            profile.push_str(&path_filter("literal", &canonical)?);
        }
        // /bin/sh dispatches to the variant selected in /private/var/select/sh.
        for path in ["/bin/sh", "/bin/bash", "/bin/dash", "/bin/zsh"] {
            profile.push_str(&path_filter("literal", Path::new(path))?);
        }
        profile.push_str("  (subpath \"/private/var/run/com.apple.security.cryptexd\")\n");
        profile.push_str(&path_filter("subpath", &self.store_root.join("tools"))?);
        // Compile-and-run probes execute products of keyed inputs.
        for path in writes {
            profile.push_str(&path_filter("subpath", path)?);
        }
        profile.push_str(&path_filter("subpath", &self.pool)?);
        profile.push_str(")\n");
        profile.push_str("(allow file-read*\n  (literal \"/\")\n  (literal \"/dev/null\")\n  (literal \"/dev/urandom\")\n  (literal \"/dev/random\")\n  (literal \"/dev/zero\")\n");
        // getcwd needs the workspace node, not all of its contents.
        profile.push_str(&path_filter("literal", workspace)?);
        // Host-installed tools and libraries are not keyed inputs.
        profile.push_str("  (subpath \"/private/var/run/com.apple.security.cryptexd\")\n");
        let mut reads: Vec<PathBuf> = [
            &self.sysroot,
            &self.cargo_home,
            &self.rustup_home,
            &self.store_root,
        ]
        .into_iter()
        .cloned()
        .collect();
        // A broad cache grant must not expose an entire workspace in /tmp.
        for directory in &self.cache_directories {
            if !workspace.starts_with(directory) {
                reads.push(directory.into());
            } else {
                reads.push(Path::new(directory).join("xcrun_db"));
            }
        }
        for path in reads {
            if !path.as_os_str().is_empty() {
                profile.push_str(&path_filter("subpath", &path)?);
            }
        }
        profile.push_str(&runtime_filters);
        let mut input_directories = std::collections::BTreeSet::new();
        for path in extra_reads {
            let mut ancestor = path.parent();
            while let Some(directory) = ancestor {
                input_directories.insert(directory);
                ancestor = directory.parent();
            }
        }
        for directory in input_directories {
            profile.push_str(&path_filter("literal", directory)?);
        }
        for path in extra_reads {
            let operation = if path.is_dir() { "subpath" } else { "literal" };
            profile.push_str(&path_filter(operation, path)?);
        }
        profile.push_str(")\n");
        profile.push_str(concat!(
            "(deny file-read* (subpath \"/Library/Developer\") ",
            "(regex #\"/[^/]+[.]app/Contents/Developer(/|$)\"))\n",
        ));
        if !runtime_filters.is_empty() {
            // Explicit keyed SDKs may live inside otherwise unkeyed Xcode installs.
            profile.push_str("(allow file-read*\n");
            profile.push_str(&runtime_filters);
            profile.push_str(")\n");
        }
        // These are excluded from source hashes, so they must not become
        // unhashed inputs through a broader grant. Later SBPL rules win.
        profile.push_str("(deny file-read* file-read-metadata\n");
        let mut deny_roots = vec![workspace.to_owned()];
        for path in extra_reads {
            if path.is_dir() {
                deny_roots.push(path.to_path_buf());
            }
        }
        deny_roots.sort();
        deny_roots.dedup();
        for root in &deny_roots {
            for directory in [".git", "target"] {
                profile.push_str(&path_filter("subpath", &root.join(directory))?);
            }
            profile.push_str(&path_filter("literal", &root.join("Cargo.lock"))?);
        }
        profile.push_str(")\n(allow file-write*\n  (literal \"/dev/null\")\n");
        for directory in &self.cache_directories {
            profile.push_str(&path_filter("subpath", Path::new(directory))?);
        }
        for path in writes {
            profile.push_str(&path_filter("subpath", path)?);
        }
        profile.push_str(")\n");
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command.arg("-p").arg(profile).arg(program);
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_preflight_rejects_missing_files_and_directories() {
        let fixture = Fixture::new();
        let directory = &fixture.0;
        let file = directory.join("sandbox-exec");
        fs::write(&file, "").unwrap();
        assert!(require_sandbox_executable(&file).is_ok());
        for path in [directory.to_path_buf(), file.join("missing")] {
            let error = require_sandbox_executable(&path).unwrap_err().to_string();
            assert_eq!(error, format!("corgi builds require {}", path.display()));
        }
    }

    #[test]
    fn cache_directories_use_canonical_darwin_spelling() {
        for (raw, expected) in [
            ("", None),
            (" \n", None),
            ("/", None),
            (
                "/var/folders/user/T/\n",
                Some("/private/var/folders/user/T"),
            ),
            (
                "/private/var/folders/user/C///",
                Some("/private/var/folders/user/C"),
            ),
            (" /custom/cache/ \n", Some("/custom/cache")),
            ("/variable/cache", Some("/variable/cache")),
        ] {
            assert_eq!(canonical_cache_directory(raw).as_deref(), expected);
        }
    }

    #[test]
    fn managed_and_supplied_profiles_keep_runtime_grants_scoped() {
        for supplied in [false, true] {
            let mut sandbox = sandbox();
            if supplied {
                sandbox.sysroot = "/nix/store/pinned-rust".into();
                sandbox.runtime_paths = vec![
                    "/nix/store/pinned-clang".into(),
                    "/nix/store/pinned-libc".into(),
                ];
            }
            let profile = profile(&sandbox, Path::new("/workspace"), &[], &[]);
            let execution = rule(&profile, "(allow process-exec*\n");
            let reads = rule(&profile, "(allow file-read*\n");
            for path in &sandbox.runtime_paths {
                // These synthetic paths need not exist on the test host.
                let grant = format!("(literal \"{}\")", path.display());
                assert!(execution.contains(&grant));
                assert!(reads.contains(&grant));
            }
            assert!(reads.contains(&format!("(subpath \"{}\")", sandbox.sysroot.display())));
            assert!(execution.contains(&format!(
                "(literal \"{}/bin/rustc\")",
                sandbox.sysroot.display()
            )));
            assert!(!profile.contains("(subpath \"/nix/store\")"));
            for path in ["/usr", "/bin", "/Library", "/Applications", "/opt/homebrew"] {
                assert!(!reads.contains(&format!("(subpath \"{path}\")")));
            }
            assert!(profile.contains(concat!(
                "(deny file-read* (subpath \"/Library/Developer\") ",
                "(regex #\"/[^/]+[.]app/Contents/Developer(/|$)\"))"
            )));
            assert!(profile.starts_with("(version 1)\n(deny default)\n"));
            assert!(!profile.contains("(allow network"));
            if !supplied {
                assert!(!profile.contains("/nix/store/"));
            }
        }
    }

    #[test]
    fn declared_developer_inputs_are_scoped_and_source_exclusions_still_win() {
        let fixture = Fixture::new();
        let developer = fixture.0.join("Xcode.app/Contents/Developer");
        let sdk = developer.join("SDKs/Declared.sdk");
        let compiler = developer.join("Toolchains/Declared/bin/clang");
        fs::create_dir_all(&sdk).unwrap();
        fs::create_dir_all(compiler.parent().unwrap()).unwrap();
        fs::write(&compiler, "").unwrap();
        let mut sandbox = sandbox();
        sandbox.runtime_paths = vec![
            sdk.clone(),
            compiler.clone(),
            "/Library/Developer/Declared/bin/clang".into(),
        ];
        let profile = profile(&sandbox, &sdk, &[&sdk], &[]);
        let (_, after_developer_deny) = profile
            .split_once(concat!(
                "(deny file-read* (subpath \"/Library/Developer\") ",
                "(regex #\"/[^/]+[.]app/Contents/Developer(/|$)\"))\n",
            ))
            .unwrap();
        // This is the entire exception: neither the developer root nor sibling
        // SDKs/toolchains get a grant. Files must not admit descendants.
        let grants = format!(
            "  (subpath \"{}\")\n  (literal \"{}\")\n  (literal \"/Library/Developer/Declared/bin/clang\")",
            sdk.display(),
            compiler.display(),
        );
        assert_eq!(rule(after_developer_deny, "(allow file-read*\n"), grants);
        let execution = rule(&profile, "(allow process-exec*\n");
        assert!(execution.contains(&grants));
        assert!(!profile.contains(&format!("(subpath \"{}\")", compiler.display())));
        let (_, after_runtime_grants) = after_developer_deny.split_once("\n)\n").unwrap();
        assert!(after_runtime_grants.starts_with("(deny file-read* file-read-metadata\n"));
        let exclusions = rule(
            after_runtime_grants,
            "(deny file-read* file-read-metadata\n",
        );
        for name in [".git", "target"] {
            assert!(exclusions.contains(&format!("(subpath \"{}/{name}\")", sdk.display())));
        }
        assert!(exclusions.contains(&format!("(literal \"{}/Cargo.lock\")", sdk.display())));
        assert!(!after_runtime_grants.contains("(allow file-read"));
    }

    #[test]
    fn all_interpolated_paths_escape_quotes_and_backslashes() {
        let mut sandbox = sandbox();
        let path = Path::new("/input/quote\"back\\slash");
        sandbox.sysroot = path.into();
        sandbox.rustc = path.into();
        sandbox.cargo_home = path.into();
        sandbox.rustup_home = path.into();
        sandbox.store_root = path.into();
        sandbox.pool = path.into();
        sandbox.runtime_paths = vec![path.into()];
        sandbox.cache_directories = vec![path.to_str().unwrap().to_owned()];
        let profile = profile(&sandbox, path, &[path], &[path]);
        let escaped = r#"/input/quote\"back\\slash"#;
        assert!(!profile.contains(path.to_str().unwrap()));
        assert!(profile.contains(&format!("(literal \"{escaped}\")")));
        assert!(profile.contains(&format!("(subpath \"{escaped}\")")));
        assert!(profile.contains(&format!("(literal \"{escaped}/bin/rustc\")")));
        assert!(profile.contains(&format!("(subpath \"{escaped}/tools\")")));
        assert!(profile.contains(&format!("(subpath \"{escaped}/xcrun_db\")")));
        assert!(profile.contains(&format!("(subpath \"{escaped}/.git\")")));
        assert!(profile.contains(&format!("(literal \"{escaped}/Cargo.lock\")")));
    }

    #[test]
    fn control_characters_cannot_inject_profile_rules() {
        for path in [
            "/input/new\nline",
            "/input/return\rline",
            "/input/tab\tname",
            "/input/nul\0name",
            "/input/\")\n(allow default)\n; ",
        ] {
            let mut sandbox = sandbox();
            sandbox.runtime_paths = vec![path.into()];
            let error = sandbox
                .command("rustc", Path::new("/workspace"), &[], &[])
                .unwrap_err();
            assert!(error.to_string().contains("control characters"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_rejected_without_lossy_substitution() {
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(std::ffi::OsStr::from_bytes(b"/input/\xff"));
        assert!(path_filter("literal", path)
            .unwrap_err()
            .to_string()
            .contains("valid UTF-8"));
    }

    #[test]
    fn source_grants_exclude_unhashed_inputs_without_broadening_workspace() {
        let fixture = Fixture::new();
        let source = fixture.0.join("src");
        fs::create_dir(&source).unwrap();
        let file = source.join("source.rs");
        fs::write(&file, "").unwrap();
        let profile = profile(&sandbox(), Path::new("/workspace"), &[&source, &file], &[]);
        let reads = rule(&profile, "(allow file-read*\n");
        assert!(reads.contains("(literal \"/workspace\")"));
        assert!(!reads.contains("(subpath \"/workspace\")"));
        assert!(reads.contains(&format!("(subpath \"{}\")", source.display())));
        assert!(reads.contains(&format!("(literal \"{}\")", file.display())));
        assert!(reads.contains(&format!(
            "(literal \"{}\")",
            source.parent().unwrap().display()
        )));
        let exclusions = rule(&profile, "(deny file-read* file-read-metadata\n");
        for root in [Path::new("/workspace"), source.as_path()] {
            for directory in [".git", "target"] {
                assert!(
                    exclusions.contains(&format!("(subpath \"{}/{directory}\")", root.display()))
                );
            }
            assert!(exclusions.contains(&format!("(literal \"{}/Cargo.lock\")", root.display())));
        }
        assert!(
            profile.find("(deny file-read* file-read-metadata").unwrap()
                > profile.find("(allow file-read*\n").unwrap()
        );
    }

    #[test]
    fn output_and_cache_permissions_preserve_temp_workspace_isolation() {
        let sandbox = sandbox();
        for workspace in ["/workspace", "/private/var/folders/user/T/workspace"] {
            let profile = profile(
                &sandbox,
                Path::new(workspace),
                &[],
                &[Path::new("/store/output")],
            );
            let reads = rule(&profile, "(allow file-read*\n");
            let writes = rule(&profile, "(allow file-write*\n");
            let execution = rule(&profile, "(allow process-exec*\n");
            assert!(writes.contains("(literal \"/dev/null\")"));
            assert!(writes.contains("(subpath \"/store/output\")"));
            assert!(execution.contains("(subpath \"/store/output\")"));
            assert!(execution.contains("(subpath \"/store/tools\")"));
            assert!(execution.contains("(subpath \"/pool\")"));
            assert!(!writes.contains("(subpath \"/store\")"));
            for directory in &sandbox.cache_directories {
                assert!(writes.contains(&format!("(subpath \"{directory}\")")));
                assert!(!execution.contains(&format!("(subpath \"{directory}\")")));
            }
            assert!(reads.contains("(subpath \"/private/var/folders/user/C\")"));
            if workspace == "/workspace" {
                assert!(reads.contains("(subpath \"/private/var/folders/user/T\")"));
            } else {
                assert!(!reads.contains("(subpath \"/private/var/folders/user/T\")"));
                assert!(reads.contains("(subpath \"/private/var/folders/user/T/xcrun_db\")"));
            }
        }
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};

            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "corgi-darwin-profile-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn sandbox() -> MacosSandbox {
        MacosSandbox {
            sysroot: "/managed/toolchain".into(),
            rustc: "/managed/toolchain/bin/rustc".into(),
            cargo_home: "/managed/cargo".into(),
            rustup_home: "/managed/rustup".into(),
            store_root: "/store".into(),
            pool: "/pool".into(),
            runtime_paths: vec![],
            cache_directories: vec![
                "/private/var/folders/user/T".into(),
                "/private/var/folders/user/C".into(),
            ],
        }
    }

    fn profile(
        sandbox: &MacosSandbox,
        workspace: &Path,
        reads: &[&Path],
        writes: &[&Path],
    ) -> String {
        let command = sandbox.command("rustc", workspace, reads, writes).unwrap();
        assert_eq!(command.get_program(), "/usr/bin/sandbox-exec");
        let arguments: Vec<_> = command.get_args().collect();
        assert_eq!(arguments.len(), 3);
        assert_eq!(arguments[0], "-p");
        assert_eq!(arguments[2], "rustc");
        arguments[1].to_str().unwrap().to_owned()
    }

    fn rule<'a>(profile: &'a str, start: &str) -> &'a str {
        profile
            .split_once(start)
            .unwrap()
            .1
            .split_once("\n)\n")
            .unwrap()
            .0
    }
}
