//! Cargo-compatible messages describe exported artifacts and the build outcome.
//!
//! Compiler diagnostics remain on stderr. Artifact fields come from the resolved
//! unit graph and exported files, including when an action was satisfied by cache.

use super::{Ctx, Kind, UnitResult};
use anyhow::Result;
use serde_json::json;
use std::io::{self, Write};
use std::path::PathBuf;

pub(super) fn artifact(
    context: &Ctx,
    index: usize,
    result: &UnitResult,
    filenames: &[PathBuf],
) -> Result<()> {
    let unit = &context.units[index];
    let package = &context.meta.packages[unit.pkg];
    let profile = &unit.profile;
    write_message(json!({
        "reason": "compiler-artifact",
        "package_id": package.id,
        "manifest_path": package.manifest_path,
        "target": unit.target,
        "profile": {
            "opt_level": profile.opt_level,
            "debuginfo": profile.debuginfo,
            "debug_assertions": profile.debug_assertions,
            "overflow_checks": profile.overflow_checks,
            "test": matches!(unit.kind, Kind::Test),
        },
        "features": unit.features,
        "filenames": filenames,
        "executable": if matches!(unit.kind, Kind::Bin | Kind::Test) {
            filenames.first()
        } else {
            None
        },
        "fresh": result.cached,
    }))
}

pub(super) fn finished(success: bool) -> Result<()> {
    write_message(json!({"reason": "build-finished", "success": success}))
}

fn write_message(message: serde_json::Value) -> Result<()> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, &message)?;
    writeln!(output)?;
    Ok(())
}
