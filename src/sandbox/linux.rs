//! Deny-by-default Linux build sandboxes using Bubblewrap.
//!
//! Only declared inputs, runtime dependencies, and action outputs enter the
//! filesystem. Directory inputs exclude unhashed source metadata. Network
//! namespaces and a syscall filter also exclude host Unix sockets and VM sockets.

use anyhow::{bail, ensure, Context, Result};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::{self, File};
#[cfg(target_os = "linux")]
use std::io::{Seek, Write};
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::store::Store;

pub struct LinuxSandbox {
    pub executable: PathBuf,
    pub runtime_paths: Vec<PathBuf>,
}

pub fn setup_linux_sandbox(store: &Store, mut runtime_paths: Vec<PathBuf>) -> Result<LinuxSandbox> {
    ensure!(
        cfg!(target_os = "linux"),
        "Bubblewrap sandboxing requires Linux"
    );
    let executable = crate::native_toolchain::find_executable("bwrap")
        .context("Linux builds require Bubblewrap (bwrap) on PATH")?;
    runtime_paths.push(store.root.clone());
    Ok(LinuxSandbox {
        executable,
        runtime_paths,
    })
}

impl LinuxSandbox {
    /// Construct a sandbox command; the caller supplies arguments and environment.
    ///
    /// Grants must be existing absolute paths. Symlinked grant roots retain both
    /// their requested and canonical names. Interior source symlinks are preserved,
    /// but do not implicitly grant their targets. Runtime directories are not
    /// source inputs and do not receive source exclusions.
    ///
    /// The caller must keep grants stable until spawn, pass only keyed runtime
    /// dependencies, and avoid inheriting host directory or socket descriptors.
    /// This does not defend against concurrent changes by another host process
    /// running as the same user. Construction and sandbox setup fail closed.
    pub fn command(
        &self,
        program: &str,
        workspace_root: &Path,
        reads: &[&Path],
        writes: &[&Path],
    ) -> Result<Command> {
        ensure!(
            self.executable.is_absolute(),
            "bwrap must be an absolute path"
        );
        let workspace = Grant::new(workspace_root)?;
        ensure!(
            workspace.source.is_dir(),
            "workspace root must be a directory"
        );
        let runtime = self
            .runtime_paths
            .iter()
            .map(|path| Grant::new(path))
            .collect::<Result<Vec<_>>>()?;
        let reads = reads
            .iter()
            .map(|path| Grant::new(path))
            .collect::<Result<Vec<_>>>()?;
        let writes = writes
            .iter()
            .map(|path| Grant::new(path))
            .collect::<Result<Vec<_>>>()?;
        for grant in &writes {
            ensure!(
                grant.source.is_dir(),
                "writable grant must be a directory: {}",
                grant.source.display()
            );
        }

        let mut mounts = BTreeMap::new();
        for grant in &runtime {
            for destination in grant.destinations() {
                // A runtime parent already exposes this symlink and its target is
                // mounted separately. Binding through it again can fail.
                if destination != grant.source
                    && runtime
                        .iter()
                        .any(|parent| destination.starts_with(&parent.source))
                {
                    continue;
                }
                mounts.insert(destination, Mount::ReadOnly(grant.source.clone()));
            }
        }
        let mut source_roots = BTreeMap::new();
        for grant in &reads {
            for destination in grant.destinations() {
                source_roots.insert(destination, &grant.source);
            }
        }
        for (destination, source) in source_roots {
            if source.is_dir() {
                // A nested source root has its own top-level exclusions. Remove
                // the broader projection's children before replacing that view.
                mounts.retain(|path, _| !path.starts_with(&destination));
                source_mounts(source, &destination, true, &mut mounts)?;
            } else {
                mounts.insert(destination, Mount::ReadOnly(source.clone()));
            }
        }
        for grant in &writes {
            for destination in grant.destinations() {
                mounts.insert(destination, Mount::Writable(grant.source.clone()));
            }
        }

        // A separately granted descendant must not reintroduce an excluded input.
        // Writes are deliberate action outputs, and may live under workspace target.
        for root in std::iter::once(&workspace).chain(reads.iter().filter(|r| r.source.is_dir())) {
            for root in root.destinations() {
                for grant in runtime.iter().chain(&reads) {
                    for destination in grant.destinations() {
                        ensure!(
                            !excluded_descendant(&root, &destination),
                            "read grant is an unhashed source exclusion: {}",
                            destination.display()
                        );
                    }
                }
            }
        }

        let filter = seccomp_file()?;
        let mut command = Command::new(&self.executable);
        command.args([
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-net",
            "--unshare-cgroup-try",
            "--disable-userns",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--hostname",
            "corgi",
            "--tmpfs",
            "/tmp",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
        ]);
        // Empty ancestors allow getcwd without granting the workspace's contents.
        for destination in workspace.destinations() {
            command.arg("--dir").arg(destination);
        }
        // Path ordering puts parents before children, including writable children
        // of read-only runtime/store mounts. No shell parsing or UTF-8 conversion.
        let mut projections = Vec::new();
        for (destination, mount) in mounts {
            match mount {
                Mount::ReadOnly(source) => {
                    command.arg("--ro-bind").arg(source).arg(destination);
                }
                Mount::Writable(source) => {
                    command.arg("--bind").arg(source).arg(destination);
                }
                Mount::Projection => {
                    command.arg("--tmpfs").arg(&destination);
                    projections.push(destination);
                }
                Mount::Symlink(target) => {
                    command.arg("--symlink").arg(target).arg(destination);
                }
            }
        }
        for projection in projections {
            command.arg("--remount-ro").arg(projection);
        }
        command.args(["--remount-ro", "/", "--seccomp"]);
        command
            .arg(filter.as_raw_fd().to_string())
            .arg("--")
            .arg(program);
        let filter_path = CString::new(format!("/proc/self/fd/{}", filter.as_raw_fd()))?;
        // Reopen rather than dup the filter so repeated/concurrent spawns have
        // independent read offsets. Command owns the original until dropped. Only
        // this descriptor loses CLOEXEC; the caller's jobserver fds are untouched.
        unsafe {
            command.pre_exec(move || {
                let descriptor = libc::open(filter_path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                if descriptor == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let result = libc::dup2(descriptor, filter.as_raw_fd());
                let error = std::io::Error::last_os_error();
                if libc::close(descriptor) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if result == -1 {
                    return Err(error);
                }
                Ok(())
            });
        }
        Ok(command)
    }
}

struct Grant {
    source: PathBuf,
    requested: PathBuf,
}

impl Grant {
    fn new(path: &Path) -> Result<Self> {
        ensure!(
            path.is_absolute() && !path.components().any(|c| matches!(c, Component::ParentDir)),
            "sandbox grants must be absolute without '..': {}",
            path.display()
        );
        let source = fs::canonicalize(path)
            .with_context(|| format!("resolving sandbox grant {}", path.display()))?;
        for name in [path, source.as_path()] {
            ensure!(
                !matches!(
                    name.to_str(),
                    Some("/" | "/nix" | "/nix/store" | "/run" | "/etc" | "/tmp")
                ) && !name.starts_with("/dev")
                    && !name.starts_with("/proc")
                    && !name.starts_with("/sys"),
                "sandbox grant is too broad or overlaps a private filesystem: {}",
                name.display()
            );
        }
        let metadata = fs::metadata(&source)?;
        ensure!(
            metadata.is_dir() || metadata.is_file(),
            "sandbox grants must be regular files or directories: {}",
            path.display()
        );
        Ok(Self {
            source,
            requested: path.components().collect(),
        })
    }

    fn destinations(&self) -> Vec<PathBuf> {
        if self.source == self.requested {
            vec![self.source.clone()]
        } else {
            vec![self.source.clone(), self.requested.clone()]
        }
    }
}

enum Mount {
    ReadOnly(PathBuf),
    Writable(PathBuf),
    Projection,
    Symlink(PathBuf),
}

/// Project only directories containing exclusions; unaffected subtrees stay single mounts.
fn source_mounts(
    source: &Path,
    destination: &Path,
    top_level: bool,
    mounts: &mut BTreeMap<PathBuf, Mount>,
) -> Result<bool> {
    let mut children = BTreeMap::new();
    let mut projected = false;
    for entry in fs::read_dir(source)
        .with_context(|| format!("reading sandbox source directory {}", source.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" || (top_level && (name == "target" || name == "Cargo.lock")) {
            projected = true;
            continue;
        }
        let child_destination = destination.join(name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            projected |= source_mounts(&entry.path(), &child_destination, false, &mut children)?;
        } else if file_type.is_symlink() {
            children.insert(
                child_destination,
                Mount::Symlink(fs::read_link(entry.path())?),
            );
        } else if file_type.is_file() {
            children.insert(child_destination, Mount::ReadOnly(entry.path()));
        } else {
            bail!(
                "special file in sandbox source input: {}",
                entry.path().display()
            );
        }
    }
    if projected {
        mounts.insert(destination.to_path_buf(), Mount::Projection);
        mounts.extend(children);
    } else {
        mounts.insert(
            destination.to_path_buf(),
            Mount::ReadOnly(source.to_path_buf()),
        );
    }
    Ok(projected)
}

fn excluded_descendant(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    let mut components = relative.components();
    let Some(first) = components.next() else {
        return false;
    };
    first.as_os_str() == "target"
        || first.as_os_str() == "Cargo.lock"
        || first.as_os_str() == ".git"
        || components.any(|component| component.as_os_str() == ".git")
}

/// Network namespaces do not isolate pathname Unix sockets or VM host sockets.
#[cfg(target_os = "linux")]
fn seccomp_program() -> Result<Vec<libc::sock_filter>> {
    ensure!(
        cfg!(target_endian = "little"),
        "Linux sandbox seccomp requires a little-endian architecture"
    );
    let architecture = match std::env::consts::ARCH {
        "x86_64" => 0xc000_003e,
        "aarch64" => 0xc000_00b7,
        architecture => bail!("Linux sandbox seccomp does not support {architecture}"),
    };
    let statement = |code: u16, k| libc::sock_filter {
        code,
        jt: 0,
        jf: 0,
        k,
    };
    let equal = |k, jt, jf| libc::sock_filter {
        code: (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16,
        jt,
        jf,
        k,
    };
    let load = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    let ret = (libc::BPF_RET | libc::BPF_K) as u16;
    let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
    let mut filter = vec![
        statement(load, 4), // seccomp_data.arch
        equal(architecture, 1, 0),
        statement(ret, libc::SECCOMP_RET_KILL_PROCESS),
        statement(load, 0), // seccomp_data.nr
    ];
    // x32 shares the x86_64 audit architecture but has a separate syscall ABI.
    if std::env::consts::ARCH == "x86_64" {
        filter.push(libc::sock_filter {
            code: (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16,
            jt: 0,
            jf: 1,
            k: 0x4000_0000,
        });
        filter.push(statement(ret, libc::SECCOMP_RET_KILL_PROCESS));
    }
    for syscall in [
        libc::SYS_socket,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
    ] {
        filter.push(equal(syscall as u32, 0, 1));
        filter.push(statement(ret, deny));
    }
    filter.extend([
        equal(libc::SYS_socketpair as u32, 0, 3),
        statement(load, 16), // low word of seccomp_data.args[0] on supported little-endian hosts
        equal(libc::AF_UNIX as u32, 1, 0),
        statement(ret, deny),
        statement(ret, libc::SECCOMP_RET_ALLOW),
    ]);
    Ok(filter)
}

#[cfg(target_os = "linux")]
fn seccomp_file() -> Result<File> {
    let filter = seccomp_program()?;
    // The filter exists only in memory and is never a shared filesystem artifact.
    let descriptor = unsafe { libc::memfd_create(c"corgi-seccomp".as_ptr(), libc::MFD_CLOEXEC) };
    ensure!(
        descriptor >= 0,
        "creating seccomp fd: {}",
        std::io::Error::last_os_error()
    );
    let mut file = unsafe { File::from_raw_fd(descriptor) };
    for instruction in filter {
        file.write_all(&instruction.code.to_ne_bytes())?;
        file.write_all(&[instruction.jt, instruction.jf])?;
        file.write_all(&instruction.k.to_ne_bytes())?;
    }
    file.rewind()?;
    Ok(file)
}

#[cfg(not(target_os = "linux"))]
fn seccomp_file() -> Result<File> {
    bail!("Bubblewrap sandboxing requires Linux")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn fresh_filesystem_and_required_isolation() -> Result<()> {
        let fixture = Fixture::new()?;
        let workspace = fixture.directory("workspace")?;
        let command = fixture
            .sandbox()
            .command("compiler with spaces", &workspace, &[], &[])?;
        let arguments = arguments(&command);
        for flag in [
            "--unshare-user",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-net",
            "--disable-userns",
            "--die-with-parent",
            "--new-session",
        ] {
            assert!(arguments.contains(&OsString::from(flag)));
        }
        assert!(contains(&arguments, &["--tmpfs".as_ref(), "/tmp".as_ref()]));
        assert!(contains(
            &arguments,
            &["--dir".as_ref(), workspace.as_os_str()]
        ));
        assert!(!arguments.contains(&OsString::from("--ro-bind")));
        assert_eq!(
            arguments.last(),
            Some(&OsString::from("compiler with spaces"))
        );
        Ok(())
    }

    #[test]
    fn source_exclusions_are_absent_including_nested_git_and_symlinks() -> Result<()> {
        let fixture = Fixture::new()?;
        let source = fixture.directory("source with spaces")?;
        fs::write(source.join("Cargo.toml"), "manifest")?;
        fs::write(source.join("Cargo.lock"), "unhashed")?;
        fixture.directory("source with spaces/target")?;
        fixture.directory("source with spaces/.git")?;
        fixture.directory("source with spaces/nested/.git")?;
        fs::write(source.join("nested/target"), "hashed nested file")?;
        fs::write(source.join("nested/Cargo.lock"), "hashed nested lock")?;
        symlink("Cargo.toml", source.join("manifest link"))?;
        let command = fixture
            .sandbox()
            .command("rustc", &source, &[&source], &[])?;
        let arguments = arguments(&command);
        for excluded in [".git", "target", "Cargo.lock", "nested/.git"] {
            assert!(!arguments.contains(&source.join(excluded).into_os_string()));
        }
        for included in ["Cargo.toml", "nested/target", "nested/Cargo.lock"] {
            assert!(arguments.contains(&source.join(included).into_os_string()));
        }
        assert!(contains(
            &arguments,
            &[
                "--symlink".as_ref(),
                "Cargo.toml".as_ref(),
                source.join("manifest link").as_os_str(),
            ]
        ));
        assert!(contains(
            &arguments,
            &["--remount-ro".as_ref(), source.as_os_str()]
        ));
        Ok(())
    }

    #[test]
    fn excluded_symlink_is_not_followed_or_granted() -> Result<()> {
        let fixture = Fixture::new()?;
        let source = fixture.directory("source")?;
        let outside = fixture.directory("outside")?;
        symlink(&outside, source.join(".git"))?;
        let command = fixture
            .sandbox()
            .command("rustc", &source, &[&source], &[])?;
        let arguments = arguments(&command);
        assert!(!arguments.contains(&outside.into_os_string()));
        assert!(!arguments.contains(&source.join(".git").into_os_string()));
        Ok(())
    }

    #[test]
    fn nested_source_roots_keep_exclusions_independent_of_grant_order() -> Result<()> {
        let fixture = Fixture::new()?;
        let workspace = fixture.directory("workspace")?;
        let nested = fixture.directory("workspace/nested")?;
        fixture.directory("workspace/nested/.git")?;
        fs::write(nested.join("Cargo.lock"), "unhashed for nested root")?;
        fs::write(nested.join("source.rs"), "hashed")?;
        for reads in [
            [workspace.as_path(), nested.as_path()],
            [&nested, &workspace],
        ] {
            let command = fixture
                .sandbox()
                .command("rustc", &workspace, &reads, &[])?;
            let arguments = arguments(&command);
            assert!(contains(
                &arguments,
                &["--tmpfs".as_ref(), nested.as_os_str()]
            ));
            assert!(!arguments.contains(&nested.join("Cargo.lock").into_os_string()));
        }
        Ok(())
    }

    #[test]
    fn unaffected_subtrees_use_one_bind_and_interior_symlinks_do_not_grant_targets() -> Result<()> {
        let fixture = Fixture::new()?;
        let source = fixture.directory("source")?;
        let subtree = fixture.directory("source/subtree")?;
        let outside = fixture.directory("outside")?;
        fs::write(source.join("Cargo.lock"), "excluded")?;
        fs::write(subtree.join("source.rs"), "hashed")?;
        symlink(&outside, subtree.join("outside"))?;
        let command = fixture
            .sandbox()
            .command("rustc", &source, &[&source], &[])?;
        let arguments = arguments(&command);
        assert!(contains(
            &arguments,
            &[
                "--ro-bind".as_ref(),
                subtree.as_os_str(),
                subtree.as_os_str()
            ]
        ));
        assert!(!arguments.contains(&subtree.join("source.rs").into_os_string()));
        assert!(!arguments.contains(&outside.into_os_string()));
        Ok(())
    }

    #[test]
    fn runtime_is_not_projected_and_nested_writes_follow_parent_reads() -> Result<()> {
        let fixture = Fixture::new()?;
        let runtime = fixture.directory("runtime")?;
        fs::write(runtime.join("Cargo.lock"), "runtime lock")?;
        fixture.directory("runtime/.git")?;
        let writable = fixture.directory("runtime/action")?;
        let workspace = fixture.directory("workspace")?;
        let sandbox = LinuxSandbox {
            runtime_paths: vec![runtime.clone()],
            ..fixture.sandbox()
        };
        let command = sandbox.command("rustc", &workspace, &[], &[&writable])?;
        let arguments = arguments(&command);
        assert!(contains(
            &arguments,
            &[
                "--ro-bind".as_ref(),
                runtime.as_os_str(),
                runtime.as_os_str(),
            ]
        ));
        assert!(contains(
            &arguments,
            &[
                "--bind".as_ref(),
                writable.as_os_str(),
                writable.as_os_str(),
            ]
        ));
        assert!(
            arguments.iter().position(|arg| arg == "--ro-bind")
                < arguments.iter().position(|arg| arg == "--bind")
        );
        Ok(())
    }

    #[test]
    fn grant_aliases_keep_original_and_canonical_names() -> Result<()> {
        let fixture = Fixture::new()?;
        let source = fixture.directory("real source")?;
        let alias = fixture.0.join("alias");
        symlink(&source, &alias)?;
        let command = fixture.sandbox().command("rustc", &alias, &[&alias], &[])?;
        let arguments = arguments(&command);
        for destination in [&source, &alias] {
            assert!(contains(
                &arguments,
                &[
                    "--ro-bind".as_ref(),
                    source.as_os_str(),
                    destination.as_os_str(),
                ]
            ));
        }
        Ok(())
    }

    #[test]
    fn malformed_missing_and_overbroad_grants_fail_closed() -> Result<()> {
        let fixture = Fixture::new()?;
        let workspace = fixture.directory("workspace")?;
        for path in [
            PathBuf::from("relative"),
            workspace.join("missing"),
            PathBuf::from("/"),
            PathBuf::from("/etc"),
            PathBuf::from("/tmp"),
            workspace.join("../workspace"),
        ] {
            assert!(fixture
                .sandbox()
                .command("rustc", &workspace, &[&path], &[])
                .is_err());
        }
        let file = workspace.join("file");
        fs::write(&file, "")?;
        assert!(fixture
            .sandbox()
            .command("rustc", &workspace, &[], &[&file])
            .is_err());
        Ok(())
    }

    #[test]
    fn explicit_reads_cannot_restore_source_exclusions() -> Result<()> {
        let fixture = Fixture::new()?;
        let workspace = fixture.directory("workspace")?;
        let target = fixture.directory("workspace/target")?;
        fs::write(target.join("unhashed"), "")?;
        assert!(fixture
            .sandbox()
            .command("rustc", &workspace, &[&target], &[])
            .is_err());
        Ok(())
    }

    #[test]
    fn filter_denies_host_socket_and_alternate_syscall_routes() -> Result<()> {
        let filter = seccomp_program()?;
        let architecture = if std::env::consts::ARCH == "x86_64" {
            0xc000_003e
        } else {
            0xc000_00b7
        };
        for syscall in [
            libc::SYS_socket,
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
        ] {
            assert_eq!(
                evaluate(&filter, architecture, syscall as u32, 0),
                libc::SECCOMP_RET_ERRNO | libc::EPERM as u32
            );
        }
        assert_eq!(
            evaluate(
                &filter,
                architecture,
                libc::SYS_socketpair as u32,
                libc::AF_UNIX as u32
            ),
            libc::SECCOMP_RET_ALLOW
        );
        assert_eq!(
            evaluate(
                &filter,
                architecture,
                libc::SYS_socketpair as u32,
                libc::AF_INET as u32
            ),
            libc::SECCOMP_RET_ERRNO | libc::EPERM as u32
        );
        assert_eq!(
            evaluate(&filter, architecture, libc::SYS_execve as u32, 0),
            libc::SECCOMP_RET_ALLOW
        );
        assert_eq!(
            evaluate(&filter, 0x4000_0003, libc::SYS_execve as u32, 0),
            libc::SECCOMP_RET_KILL_PROCESS
        );
        if std::env::consts::ARCH == "x86_64" {
            assert_eq!(
                evaluate(
                    &filter,
                    architecture,
                    0x4000_0000 | libc::SYS_socket as u32,
                    0
                ),
                libc::SECCOMP_RET_KILL_PROCESS
            );
        }
        Ok(())
    }

    fn evaluate(
        filter: &[libc::sock_filter],
        architecture: u32,
        syscall: u32,
        argument: u32,
    ) -> u32 {
        let mut accumulator = 0;
        let mut position = 0;
        loop {
            let instruction = &filter[position];
            match instruction.code as u32 {
                code if code == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS => {
                    accumulator = match instruction.k {
                        0 => syscall,
                        4 => architecture,
                        16 => argument,
                        _ => panic!("unknown load"),
                    };
                }
                code if code == libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K => {
                    position += if accumulator == instruction.k {
                        instruction.jt
                    } else {
                        instruction.jf
                    } as usize;
                }
                code if code == libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K => {
                    position += if accumulator & instruction.k != 0 {
                        instruction.jt
                    } else {
                        instruction.jf
                    } as usize;
                }
                code if code == libc::BPF_RET | libc::BPF_K => return instruction.k,
                _ => panic!("unknown instruction"),
            }
            position += 1;
        }
    }

    fn arguments(command: &Command) -> Vec<OsString> {
        command.get_args().map(OsString::from).collect()
    }

    fn contains(arguments: &[OsString], sequence: &[&std::ffi::OsStr]) -> bool {
        arguments
            .windows(sequence.len())
            .any(|window| window.iter().zip(sequence).all(|(a, b)| a == b))
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Result<Self> {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let directory = std::env::temp_dir().join(format!(
                "corgi-linux-sandbox-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&directory)?;
            Ok(Self(fs::canonicalize(directory)?))
        }

        fn directory(&self, name: &str) -> Result<PathBuf> {
            let directory = self.0.join(name);
            fs::create_dir_all(&directory)?;
            Ok(directory)
        }

        fn sandbox(&self) -> LinuxSandbox {
            LinuxSandbox {
                executable: PathBuf::from("/bwrap"),
                runtime_paths: Vec::new(),
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).expect("remove sandbox fixture");
        }
    }
}
