pub mod linux;
pub mod macos;

use anyhow::Result;
use linux::LinuxSandbox;
use macos::MacosSandbox;
use std::path::Path;
use std::process::Command;

pub(crate) enum Sandbox {
    Linux(LinuxSandbox),
    Darwin(MacosSandbox),
}

impl Sandbox {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Linux(_) => "bubblewrap",
            Self::Darwin(_) => "seatbelt",
        }
    }

    pub(crate) fn command(
        &self,
        program: &str,
        workspace: &Path,
        extra_reads: &[&Path],
        writes: &[&Path],
    ) -> Result<Command> {
        match self {
            Self::Linux(sandbox) => sandbox.command(program, workspace, extra_reads, writes),
            Self::Darwin(sandbox) => sandbox.command(program, workspace, extra_reads, writes),
        }
    }
}
