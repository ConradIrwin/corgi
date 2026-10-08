//! Small host-platform differences shared by planning and managed tools.

use std::path::Path;

/// The filename of a native executable: `rustc` or `rustc.exe`.
pub fn executable_name(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

/// Create a symlink to a file. Windows needs Developer Mode or elevation.
pub fn symlink(target: impl AsRef<Path>, link: impl AsRef<Path>) -> std::io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    return std::os::windows::fs::symlink_file(target, link);
}
