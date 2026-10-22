//! Arbitration and process cleanup for compilers racing a verified remote result.
//!
//! The compiler worker owns installation of a remote winner, so publication
//! cannot overlap local ingestion. Pipelined producers do not use this path:
//! consumers may already hold their metadata.

use super::remote_cache::Record;
use crate::report::RemoteDownload;
use anyhow::{Context, Result};
use std::{fs, path::Path, process::Command, sync::Mutex, time::Duration};

#[derive(Default)]
pub(super) struct Race {
    state: Mutex<State>,
}

#[derive(Default)]
enum State {
    #[default]
    Open,
    Local,
    Remote(Box<(Record, RemoteDownload)>),
}

impl Race {
    pub(super) fn offer(
        &self,
        record: Record,
        download: RemoteDownload,
    ) -> Result<(), Box<(Record, RemoteDownload)>> {
        let remote = Box::new((record, download));
        let mut state = self.state.lock().unwrap();
        if !matches!(*state, State::Open) {
            return Err(remote);
        }
        *state = State::Remote(remote);
        Ok(())
    }

    pub(super) fn check(&self) -> Result<()> {
        if matches!(*self.state.lock().unwrap(), State::Remote(_)) {
            return Err(Superseded.into());
        }
        Ok(())
    }

    /// Seal local publication before writing any shared action or artifact state.
    pub(super) fn commit_local(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, State::Remote(_)) {
            return Err(Superseded.into());
        }
        *state = State::Local;
        Ok(())
    }

    pub(super) fn finish(&self) -> Option<(Record, RemoteDownload)> {
        match std::mem::replace(&mut *self.state.lock().unwrap(), State::Local) {
            State::Remote(remote) => Some(*remote),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(super) struct Superseded;

impl std::fmt::Display for Superseded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("compiler superseded by verified remote result")
    }
}

impl std::error::Error for Superseded {}

#[derive(Debug)]
pub(super) struct NotNeeded;

impl std::fmt::Display for NotNeeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("action no longer needed")
    }
}

impl std::error::Error for NotNeeded {}

#[derive(Debug)]
pub(super) struct TeardownFailed;

impl std::fmt::Display for TeardownFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("could not stop and reap losing compiler")
    }
}

pub(super) struct Staging(pub Vec<std::path::PathBuf>);

impl Drop for Staging {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// Own the global permits; rustc can lose only tokens in its private jobserver.
///
/// Killing a child using the global jobserver directly would strand its codegen
/// tokens. Escrow currently spare capacity until this compiler has been reaped.
pub(super) fn run(
    command: &mut Command,
    jobserver: &jobserver::Client,
    mut check_cancelled: impl FnMut() -> Result<()>,
    stderr_path: &Path,
) -> Result<(bool, String)> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let helper = jobserver.clone().into_helper_thread(move |permit| {
        let _ = sender.send(permit);
    })?;
    let mut permits = Vec::new();
    helper.request_token();
    loop {
        check_cancelled()?;
        match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(permit) => {
                permits.push(permit?);
                break;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => return Err(error).context("acquiring compiler capacity"),
        }
    }
    while jobserver.available()? > 0 {
        helper.request_token();
        match receiver.recv_timeout(Duration::from_millis(1)) {
            Ok(permit) => permits.push(permit?),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(error) => return Err(error).context("acquiring spare compiler capacity"),
        }
    }
    // Dropping the helper interrupts a pending acquire. Keep any permit that
    // arrived during shutdown; none may be stranded in the callback channel.
    drop(helper);
    for permit in receiver.try_iter() {
        permits.push(permit?);
    }
    let private = jobserver::Client::new(permits.len() - 1)?;
    private.configure(command);
    check_cancelled()?;
    // File-backed stderr avoids a pipe reader keeping cancellation blocked.
    command.stderr(fs::File::create(stderr_path)?);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("spawning cancellable compiler")?;
    let result: Result<bool> = (|| loop {
        check_cancelled()?;
        if let Some(status) = child.try_wait()? {
            return Ok(status.success());
        }
        std::thread::sleep(Duration::from_millis(10));
    })();
    if result.is_err() {
        // Kill the group as well as rustc: a linker must not keep writing into
        // staging after the worker begins cleanup or installs the winner.
        #[cfg(unix)]
        {
            let result = unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                // macOS returns EPERM for a group containing only zombies.
                // Confirm exit before treating it as an already-stopped group.
                let already_exited = cfg!(target_os = "macos")
                    && error.raw_os_error() == Some(libc::EPERM)
                    && child.try_wait().context(TeardownFailed)?.is_some();
                if error.raw_os_error() != Some(libc::ESRCH) && !already_exited {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error).context(TeardownFailed);
                }
            }
        }
        #[cfg(not(unix))]
        child.kill().context(TeardownFailed)?;
        child.wait().context(TeardownFailed)?;
    }
    let success = result?;
    Ok((
        success,
        String::from_utf8_lossy(&fs::read(stderr_path)?).into_owned(),
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn cancellation_reaps_process_group_and_restores_borrowed_tokens() {
        let directory = std::env::temp_dir().join(format!("corgi-race-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let _cleanup = Staging(vec![directory.clone()]);
        let marker = directory.join("child");
        let jobserver = jobserver::Client::new(3).unwrap();
        let mut command = Command::new("python3");
        command
            .arg("-c")
            .arg(
                r#"
import os, re, subprocess, sys, time
read_fd = int(re.search(r"--jobserver-auth=(\d+),", os.environ["CARGO_MAKEFLAGS"])[1])
# Deliberately strand rustc's extra codegen tokens when killed.
assert len(os.read(read_fd, 2)) == 2
child = subprocess.Popen(["sleep", "60"])
open(sys.argv[1], "w").write(str(child.pid))
time.sleep(60)
"#,
            )
            .arg(&marker);
        let start = Instant::now();
        let error = run(
            &mut command,
            &jobserver,
            || {
                anyhow::ensure!(
                    start.elapsed() < Duration::from_secs(5),
                    "child did not start"
                );
                if marker.exists() {
                    Err(Superseded.into())
                } else {
                    Ok(())
                }
            },
            &directory.join("stderr"),
        )
        .unwrap_err();
        assert!(error.is::<Superseded>(), "{error:#}");
        assert!(start.elapsed() < Duration::from_secs(5));
        assert_eq!(jobserver.available().unwrap(), 3);
        let pid = fs::read_to_string(marker).unwrap();
        // Orphans may briefly remain as zombies until the OS reaps them.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let output = Command::new("ps")
                .args(["-p", &pid, "-o", "stat="])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&output.stdout);
            if state.trim().is_empty() || state.trim().starts_with('Z') {
                break;
            }
            assert!(Instant::now() < deadline, "descendant survived: {state}");
            std::thread::sleep(Duration::from_millis(10));
        }

        // Cancellation can arrive after exit but before the compiler is reaped.
        let marker = directory.join("exited-pid");
        let stderr = directory.join("exited-stderr");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "echo $$ > \"$1\"", "exited-compiler"])
            .arg(&marker);
        let error = run(
            &mut command,
            &jobserver,
            || {
                if !stderr.exists() {
                    return Ok(());
                }
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Ok(pid) = fs::read_to_string(&marker) {
                        if !pid.trim().is_empty() {
                            let output = Command::new("ps")
                                .args(["-p", pid.trim(), "-o", "stat="])
                                .output()?;
                            if String::from_utf8_lossy(&output.stdout)
                                .trim()
                                .starts_with('Z')
                            {
                                return Err(Superseded.into());
                            }
                        }
                    }
                    anyhow::ensure!(Instant::now() < deadline, "compiler did not exit");
                    std::thread::sleep(Duration::from_millis(10));
                }
            },
            &stderr,
        )
        .unwrap_err();
        assert!(error.is::<Superseded>(), "{error:#}");
        assert_eq!(jobserver.available().unwrap(), 3);
    }
}
