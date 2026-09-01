//! Hermetic execution of build actions.
//!
//! Every rustc invocation and every build-script run happens inside a
//! deny-by-default confinement: reads are limited to the toolchain, the store,
//! and the action's declared inputs; writes to the action's output and scratch
//! directories; execution to binaries whose identity is part of the action
//! key; and the network is unreachable. Anything an action reaches for outside
//! that set fails loudly instead of silently becoming an unkeyed input.
//!
//! Each host enforces the same policy with a different kernel facility, so the
//! policy is expressed once as a set of read, write, and exec paths and
//! translated by a per-host [`Sandbox`] implementation.

use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "linux")]
mod bubblewrap;
#[cfg(target_os = "macos")]
mod seatbelt;

/// The machine-wide paths every policy is written in terms of: the pinned
/// toolchain, the store that holds keyed content, and the workspace under
/// build.
pub struct Environment {
    /// Physical store root; the store's own subdirectories (`pool`, `tools`,
    /// …) are derived from it.
    pub store_root: PathBuf,
    /// Machine-independent spelling of the store root, when the store is not
    /// already at the canonical path.
    pub store_alias: Option<PathBuf>,
    pub sysroot: String,
    pub cargo_home: String,
    pub rustup_home: String,
    pub workspace_root: String,
}

impl Environment {
    /// Every spelling of the store root that an action may encounter: its
    /// physical path, and the canonical alias the action is actually handed.
    fn store_roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.store_root.as_path()).chain(self.store_alias.as_deref())
    }
}

pub trait Sandbox: Send + Sync {
    /// A command that runs `program` in `working_directory`, able to read only
    /// `reads` (plus the toolchain and store) and to write only under
    /// `writes`, with no network. Children inherit the confinement.
    ///
    /// Callers append their own arguments and environment to the result.
    fn command(
        &self,
        program: &Path,
        working_directory: &Path,
        reads: &[&Path],
        writes: &[&Path],
    ) -> Command;

    /// How the confinement is enforced, for `--verbose` output.
    fn description(&self) -> &'static str;
}

/// Fail early, before any planning work, when this host cannot confine
/// actions at all: corgi has no unsandboxed build mode.
pub fn ensure_available() -> Result<()> {
    #[cfg(target_os = "macos")]
    return seatbelt::ensure_available();
    #[cfg(target_os = "linux")]
    return bubblewrap::ensure_available();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    anyhow::bail!("corgi builds require macOS or Linux");
}

/// Directory and file names that the source hash deliberately ignores, and
/// that actions must therefore be unable to read: reading them would make
/// them unkeyed inputs.
const UNHASHED_ENTRIES: [&str; 4] = [".git", "target", "dtarget", "Cargo.lock"];

pub fn for_host(environment: Environment) -> Result<Box<dyn Sandbox>> {
    #[cfg(target_os = "macos")]
    return Ok(Box::new(seatbelt::Seatbelt::new(environment)?));
    #[cfg(target_os = "linux")]
    return Ok(Box::new(bubblewrap::Bubblewrap::new(environment)?));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = environment;
        anyhow::bail!("corgi builds require macOS or Linux");
    }
}
