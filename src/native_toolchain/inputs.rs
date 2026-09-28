//! Content identities and read grants for supplied toolchain runtime inputs.
//!
//! Mutable trees are hashed without source exclusions. Symlinks may refer only
//! to declared mutable inputs or immutable Nix closures, never ambient host data.

use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub(super) struct RuntimeInputs {
    pub paths: Vec<PathBuf>,
    pub identity: String,
}

/// Resolve stable runtime roots, hashing mutable contents on every invocation.
///
/// The caller must keep these inputs unchanged until the build finishes. Root
/// aliases are projected at both names; relative links must retain their meaning
/// under that projection. Nix is needed only when a root or interior link uses it.
pub(super) fn resolve(roots: &[PathBuf]) -> Result<RuntimeInputs> {
    let policy = PathPolicy::new();
    let mut paths = BTreeSet::new();
    let mut declared = BTreeMap::new();
    for root in roots {
        policy.validate(root)?;
        let canonical = canonicalize(root)?;
        policy.validate(&canonical)?;
        let metadata = fs::metadata(&canonical)?;
        ensure!(
            metadata.is_file() || metadata.is_dir(),
            "unsupported runtime input: {}",
            root.display()
        );
        declared.insert(canonical.clone(), metadata.is_dir());
        paths.insert(root.clone());
        paths.insert(canonical);
    }
    let mut state = Resolver {
        policy,
        declared,
        paths,
        nix_roots: BTreeSet::new(),
        hash: Sha256::new(),
    };
    field(&mut state.hash, b"corgi-runtime-inputs-v1");
    let mut processed = BTreeSet::new();
    while let Some(path) = state.paths.difference(&processed).next().cloned() {
        let canonical = canonicalize(&path)?;
        field(&mut state.hash, path.as_os_str().as_bytes());
        field(&mut state.hash, canonical.as_os_str().as_bytes());
        state.hash_aliases(&path)?;
        for spelling in [&path, &canonical] {
            if let Some(root) = nix_root(spelling) {
                state.nix_roots.insert(root);
            }
        }
        // An external alias into the store still needs its relative links checked:
        // bind-mounting a directory at a new name can change where ".." leads.
        if nix_root(&path).is_none() {
            state.walk(&path, Path::new(""), nix_root(&canonical).is_some())?;
        }
        processed.insert(path);
    }
    if !state.nix_roots.is_empty() {
        let mut command = Command::new(super::find_executable("nix-store")?);
        command
            .arg0("nix-store")
            .args(["--query", "--requisites"])
            .args(&state.nix_roots);
        let output = super::output(&mut command, "resolving supplied Nix toolchain closures")?;
        let mut closure = BTreeSet::new();
        for line in output.lines().filter(|line| !line.is_empty()) {
            let path = PathBuf::from(line);
            state.policy.validate(&path)?;
            ensure!(
                nix_root(&path).as_ref() == Some(&path) && path.exists(),
                "Nix returned an invalid runtime dependency: {}",
                path.display()
            );
            let canonical = canonicalize(&path)?;
            ensure!(
                nix_root(&canonical).is_some(),
                "Nix runtime dependency escapes the store: {}",
                path.display()
            );
            closure.insert(path);
        }
        ensure!(
            state.nix_roots.is_subset(&closure),
            "Nix did not return the complete supplied toolchain closure"
        );
        for path in &closure {
            let canonical = canonicalize(path)?;
            ensure!(
                nix_root(&canonical).is_some_and(|root| closure.contains(&root)),
                "Nix omitted the target of runtime dependency {}",
                path.display()
            );
            field(&mut state.hash, b"nix");
            field(&mut state.hash, path.as_os_str().as_bytes());
        }
        state.paths.extend(closure);
    }
    // Include exactly what is exposed, independently of traversal/discovery order.
    for path in &state.paths {
        field(&mut state.hash, b"grant");
        field(&mut state.hash, path.as_os_str().as_bytes());
    }
    Ok(RuntimeInputs {
        paths: state.paths.into_iter().collect(),
        identity: crate::store::hex(&state.hash.finalize()),
    })
}

struct Resolver {
    policy: PathPolicy,
    declared: BTreeMap<PathBuf, bool>,
    paths: BTreeSet<PathBuf>,
    nix_roots: BTreeSet<PathBuf>,
    hash: Sha256,
}

impl Resolver {
    fn walk(&mut self, root: &Path, relative: &Path, immutable: bool) -> Result<()> {
        let path = if relative.as_os_str().is_empty() {
            root.to_path_buf()
        } else {
            root.join(relative)
        };
        let metadata = if relative.as_os_str().is_empty() {
            fs::metadata(&path)
        } else {
            fs::symlink_metadata(&path)
        }
        .with_context(|| format!("reading runtime input {}", path.display()))?;
        field(&mut self.hash, relative.as_os_str().as_bytes());
        field(&mut self.hash, &metadata.permissions().mode().to_le_bytes());
        if metadata.is_dir() {
            field(&mut self.hash, b"directory");
            let mut entries = fs::read_dir(&path)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<std::io::Result<Vec<_>>>()?;
            entries.sort();
            for name in entries {
                self.walk(root, &relative.join(name), immutable)?;
            }
        } else if metadata.is_file() {
            field(&mut self.hash, b"file");
            if !immutable {
                field(&mut self.hash, crate::store::sha256_file(&path)?.as_bytes());
            }
        } else if metadata.file_type().is_symlink() {
            field(&mut self.hash, b"symlink");
            let text = fs::read_link(&path)?;
            field(&mut self.hash, text.as_os_str().as_bytes());
            let target =
                self.normalize_target(&path.parent().context("symlink has no parent")?.join(text))?;
            let canonical = canonicalize(&path)?;
            ensure!(
                canonicalize(&target)? == canonical,
                "runtime symlink {} changes meaning under its root alias; use a stable absolute link or declare its target in CORGI_NATIVE_RUNTIME_ROOTS",
                path.display()
            );
            self.dependency(&target)
                .with_context(|| format!("runtime symlink {}", path.display()))?;
        } else {
            bail!("unsupported special runtime input: {}", path.display());
        }
        Ok(())
    }

    fn dependency(&mut self, target: &Path) -> Result<()> {
        let canonical = canonicalize(target)?;
        self.policy.validate(target)?;
        self.policy.validate(&canonical)?;
        if let Some(root) = nix_root(&canonical) {
            self.nix_roots.insert(root);
        } else {
            ensure!(
                self.declared.iter().any(|(root, directory)| {
                    canonical == *root || (*directory && canonical.starts_with(root))
                }),
                "runtime dependency escapes to undeclared {}; add it to CORGI_NATIVE_RUNTIME_ROOTS",
                canonical.display()
            );
        }
        if let Some(root) = nix_root(target) {
            self.nix_roots.insert(root);
        }
        // The canonical destination alone is insufficient when link text names
        // an external alias. Bind that exact target, never its unkeyed parent.
        for destination in [target.to_path_buf(), canonical] {
            if !self.paths.iter().any(|root| {
                destination == *root || (root.is_dir() && destination.starts_with(root))
            }) {
                self.paths.insert(destination);
            }
        }
        Ok(())
    }

    fn normalize_target(&mut self, path: &Path) -> Result<PathBuf> {
        let mut result = PathBuf::new();
        for component in path.components() {
            if component == Component::ParentDir {
                // "directory/../file" still needs directory to exist at runtime.
                // Never erase that dependency just because the endpoint is keyed.
                self.dependency(&result)?;
                result.pop();
            } else if component != Component::CurDir {
                result.push(component);
            }
        }
        Ok(result)
    }

    fn hash_aliases(&mut self, path: &Path) -> Result<()> {
        let mut prefix = PathBuf::new();
        for component in path.components() {
            prefix.push(component);
            let metadata = fs::symlink_metadata(&prefix)?;
            if metadata.file_type().is_symlink() {
                field(&mut self.hash, b"alias");
                field(&mut self.hash, prefix.as_os_str().as_bytes());
                field(&mut self.hash, &metadata.permissions().mode().to_le_bytes());
                field(
                    &mut self.hash,
                    fs::read_link(&prefix)?.as_os_str().as_bytes(),
                );
            }
        }
        Ok(())
    }
}

struct PathPolicy {
    forbidden: BTreeSet<PathBuf>,
    special: BTreeSet<PathBuf>,
}

impl PathPolicy {
    fn new() -> Self {
        let mut forbidden = BTreeSet::new();
        let mut special = BTreeSet::new();
        for name in [
            "/",
            "/usr",
            "/nix",
            "/nix/store",
            "/run",
            "/etc",
            "/tmp",
            "/dev",
            "/proc",
            "/sys",
        ] {
            forbidden.insert(PathBuf::from(name));
            if let Ok(canonical) = fs::canonicalize(name) {
                forbidden.insert(canonical);
            }
        }
        for name in ["/dev", "/proc", "/sys"] {
            special.insert(PathBuf::from(name));
            if let Ok(canonical) = fs::canonicalize(name) {
                special.insert(canonical);
            }
        }
        Self { forbidden, special }
    }

    fn validate(&self, path: &Path) -> Result<()> {
        ensure!(
            path.is_absolute()
                && !path.components().any(|part| matches!(part, Component::ParentDir)),
            "runtime roots must be nonempty absolute paths without '..': {}; check CORGI_NATIVE_RUNTIME_ROOTS",
            path.display()
        );
        ensure!(
            !self.forbidden.contains(path)
                && !self.special.iter().any(|root| path.starts_with(root)),
            "unsafe runtime root {}; select a specific toolchain or dependency in CORGI_NATIVE_RUNTIME_ROOTS",
            path.display()
        );
        Ok(())
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path).with_context(|| {
        format!(
            "resolving runtime input {} (missing or cyclic dependency); check CORGI_NATIVE_RUNTIME_ROOTS",
            path.display()
        )
    })
}

fn nix_root(path: &Path) -> Option<PathBuf> {
    let name = path.strip_prefix("/nix/store").ok()?.components().next()?;
    matches!(name, Component::Normal(_)).then(|| Path::new("/nix/store").join(name))
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn local_inputs_are_deterministic_and_hash_same_length_rewrites() {
        let fixture = Fixture::new();
        let first = fixture.directory("first");
        let second = fixture.directory("second");
        fs::write(first.join("tool"), b"aaaa").unwrap();
        fs::write(second.join("header"), b"bbbb").unwrap();
        let initial = resolve(&[first.clone(), second.clone()]).unwrap();
        let reordered = resolve(&[second.clone(), first.clone(), first.clone()]).unwrap();
        let file = resolve(&[first.join("tool")]).unwrap();
        assert_eq!(file.paths, vec![first.join("tool")]);
        assert_eq!(initial.identity, reordered.identity);
        assert_eq!(initial.paths, reordered.paths);
        fs::write(first.join("tool"), b"cccc").unwrap();
        assert_ne!(
            file.identity,
            resolve(&[first.join("tool")]).unwrap().identity
        );
        assert_ne!(
            initial.identity,
            resolve(&[first, second]).unwrap().identity
        );
    }

    #[test]
    fn executable_permissions_are_inputs() {
        let fixture = Fixture::new();
        let root = fixture.directory("tools");
        let executable = root.join("tool");
        fs::write(&executable, b"tool").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o644)).unwrap();
        let before = resolve(std::slice::from_ref(&root)).unwrap().identity;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        assert_ne!(before, resolve(&[root]).unwrap().identity);
    }

    #[test]
    fn source_exclusions_are_runtime_inputs() {
        let fixture = Fixture::new();
        let root = fixture.directory("tools");
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir(root.join("target")).unwrap();
        for relative in [".git/config", "target/library", "Cargo.lock"] {
            let before = resolve(std::slice::from_ref(&root)).unwrap().identity;
            fs::write(root.join(relative), b"one").unwrap();
            let added = resolve(std::slice::from_ref(&root)).unwrap().identity;
            assert_ne!(before, added, "{relative}");
            fs::write(root.join(relative), b"two").unwrap();
            assert_ne!(
                added,
                resolve(std::slice::from_ref(&root)).unwrap().identity,
                "{relative}"
            );
        }
    }

    #[test]
    fn root_aliases_and_internal_links_cover_their_contents() {
        let fixture = Fixture::new();
        let root = fixture.directory("tools");
        fs::write(root.join("library"), b"one").unwrap();
        symlink("library", root.join("link")).unwrap();
        let alias = fixture.0.join("alias");
        symlink(&root, &alias).unwrap();
        let initial = resolve(std::slice::from_ref(&alias)).unwrap();
        assert!(initial.paths.contains(&alias));
        assert!(initial.paths.contains(&root));
        assert_ne!(
            initial.identity,
            resolve(std::slice::from_ref(&root)).unwrap().identity
        );
        fs::write(root.join("library"), b"two").unwrap();
        assert_ne!(
            initial.identity,
            resolve(std::slice::from_ref(&alias)).unwrap().identity
        );
        let before = resolve(std::slice::from_ref(&alias)).unwrap().identity;
        fs::remove_file(root.join("link")).unwrap();
        symlink("./library", root.join("link")).unwrap();
        assert_ne!(before, resolve(&[alias]).unwrap().identity);
    }

    #[test]
    fn external_alias_targets_are_granted_without_their_parents() {
        let fixture = Fixture::new();
        let tools = fixture.directory("tools");
        let dependency = fixture.directory("dependency");
        fs::write(dependency.join("library"), b"one").unwrap();
        let alias = fixture.0.join("alias");
        symlink(&dependency, &alias).unwrap();
        symlink(alias.join("library"), tools.join("library")).unwrap();
        let initial = resolve(&[tools.clone(), dependency.clone()]).unwrap();
        assert!(initial.paths.contains(&alias.join("library")));
        assert!(!initial.paths.contains(&alias));
        assert!(!initial.paths.contains(&fixture.0));
        fs::write(dependency.join("library"), b"two").unwrap();
        assert_ne!(
            initial.identity,
            resolve(&[tools, dependency]).unwrap().identity
        );
    }

    #[test]
    fn parent_traversal_keeps_intermediate_directories_keyed() {
        let fixture = Fixture::new();
        let tools = fixture.directory("tools");
        let dependency = fixture.directory("dependency");
        let intermediate = dependency.join("intermediate");
        fs::create_dir(&intermediate).unwrap();
        let library = dependency.join("library");
        fs::write(&library, b"one").unwrap();
        symlink(intermediate.join("../library"), tools.join("library")).unwrap();
        assert!(resolve(&[tools.clone(), library.clone()]).is_err());
        let resolved = resolve(&[tools, library, intermediate.clone()]).unwrap();
        assert!(resolved.paths.contains(&intermediate));
        assert!(!resolved.paths.contains(&dependency));
    }

    #[test]
    fn escapes_missing_roots_cycles_and_unsafe_roots_fail_closed() {
        let fixture = Fixture::new();
        let tools = fixture.directory("tools");
        let outside = fixture.directory("outside");
        fs::write(outside.join("private"), b"secret").unwrap();
        symlink(outside.join("private"), tools.join("link")).unwrap();
        let error = format!("{:#}", resolve(std::slice::from_ref(&tools)).err().unwrap());
        assert!(error.contains("CORGI_NATIVE_RUNTIME_ROOTS"));
        assert!(error.contains("undeclared"));
        assert!(resolve(&[tools.clone(), outside]).is_ok());
        fs::remove_file(tools.join("link")).unwrap();
        symlink("link", tools.join("link")).unwrap();
        assert!(resolve(&[tools]).is_err());
        for root in [
            "",
            "relative",
            "/",
            "/usr",
            "/nix",
            "/nix/store",
            "/run",
            "/etc",
            "/tmp",
            "/dev",
            "/dev/null",
            "/proc",
            "/sys",
        ] {
            assert!(resolve(&[PathBuf::from(root)]).is_err(), "{root}");
        }
        assert!(resolve(&[fixture.0.join("missing")]).is_err());
        let alias = fixture.0.join("unsafe-alias");
        symlink("/", &alias).unwrap();
        assert!(resolve(&[alias]).is_err());
    }

    #[test]
    fn special_files_and_alias_relative_escapes_fail_closed() {
        let fixture = Fixture::new();
        let tools = fixture.directory("tools");
        let socket = tools.join("socket");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(resolve(std::slice::from_ref(&tools)).is_err());
        fs::remove_file(socket).unwrap();
        let outside = fixture.directory("outside");
        fs::write(outside.join("library"), b"one").unwrap();
        symlink("../outside/library", tools.join("library")).unwrap();
        assert!(resolve(&[tools.clone(), outside.clone()]).is_ok());
        let alias_parent = fixture.directory("aliases");
        let alias = alias_parent.join("tools");
        symlink(tools, &alias).unwrap();
        assert!(resolve(&[alias, outside]).is_err());
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "corgi-runtime-inputs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }

        fn directory(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir(&path).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }
}
