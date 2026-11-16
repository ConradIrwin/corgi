//! Pinned Zig cross-linker support.
//!
//! This module owns the Zig release pins and the compatibility entry point used
//! by Corgi's compiler-driver symlinks. Build planning and sandbox integration
//! remain in the build module.

use anyhow::{bail, Context, Result};
use cargo_zigbuild::Zig;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

pub const VERSION: &str = "0.16.0";
pub const DRIVER_VERSION: &str = "cargo-zigbuild-0.23.0+corgi.10";

/// Wrapper-directory file naming the pinned C++ runtime's sysroot.
pub const CXX_RUNTIME_FILE: &str = "cxx-runtime";
/// Wrapper subdirectory linking the C++ runtime's libraries under new names.
///
/// Zig resolves `-lstdc++` to its own libc++, even given a path to
/// libstdc++, and cargo-zigbuild rewrites `-lgcc_s` to Zig's libunwind. Names
/// neither recognizes make the linker use the pinned libraries.
pub const CXX_LINK_DIRECTORY: &str = "cxx-link";
pub const LIBSTDCXX_LINK_FILE: &str = "libcorgi_libstdcxx.so";
pub const LIBGCC_S_LINK_FILE: &str = "libcorgi_libgcc_s.so";
const LIBSTDCXX_LINK_ARGUMENT: &str = "-lcorgi_libstdcxx";
const LIBGCC_S_LINK_ARGUMENT: &str = "-lcorgi_libgcc_s";

/// The pinned GNU C++ runtime a glibc wrapper compiles and links C++ against.
struct CxxRuntime {
    root: PathBuf,
    link_directory: PathBuf,
    zig_path: PathBuf,
    zig_target: String,
}

pub struct Asset {
    pub platform: &'static str,
    pub sha256: &'static str,
}

pub struct Target<'a> {
    pub rust: &'a str,
    pub zig: String,
    pub cmake_processor: &'static str,
    pub use_zig_as_rust_linker: bool,
}

pub fn asset(host: &str) -> Result<Asset> {
    let asset = match host {
        "aarch64-apple-darwin" => Asset {
            platform: "aarch64-macos",
            sha256: "b23d70deaa879b5c2d486ed3316f7eaa53e84acf6fc9cc747de152450d401489",
        },
        "x86_64-apple-darwin" => Asset {
            platform: "x86_64-macos",
            sha256: "0387557ed1877bc6a2e1802c8391953baddba76081876301c522f52977b52ba7",
        },
        "aarch64-unknown-linux-gnu" => Asset {
            platform: "aarch64-linux",
            sha256: "ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17",
        },
        "x86_64-unknown-linux-gnu" => Asset {
            platform: "x86_64-linux",
            sha256: "70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00",
        },
        _ => bail!("Zig {VERSION} has no pinned Corgi asset for host `{host}`"),
    };
    Ok(asset)
}

pub fn url(asset: &Asset) -> String {
    format!(
        "https://ziglang.org/download/{VERSION}/zig-{}-{VERSION}.tar.xz",
        asset.platform
    )
}

pub fn archive_root(asset: &Asset) -> String {
    format!("zig-{}-{VERSION}", asset.platform)
}

pub fn target(target: &str) -> Result<Option<Target<'_>>> {
    let (rust_target, suffix) = match target.split_once('.') {
        Some((rust_target, suffix)) => (rust_target, Some(suffix)),
        None => (target, None),
    };
    let (zig_target, libc, cmake_processor, use_zig_as_rust_linker) = match rust_target {
        "x86_64-unknown-linux-gnu" => ("x86_64-linux-gnu", Some("gnu"), "x86_64", true),
        "aarch64-unknown-linux-gnu" => ("aarch64-linux-gnu", Some("gnu"), "aarch64", true),
        "x86_64-unknown-linux-musl" => ("x86_64-linux-musl", Some("musl"), "x86_64", true),
        "aarch64-unknown-linux-musl" => ("aarch64-linux-musl", Some("musl"), "aarch64", true),
        "wasm32-unknown-unknown" => ("wasm32-freestanding", None, "wasm32", false),
        _ => return Ok(None),
    };
    if let Some(suffix) = suffix {
        if libc != Some("gnu") {
            bail!("Zig target ABI suffixes are only supported for glibc targets: `{target}`");
        }
        let mut components = suffix.split('.');
        let valid = components.next().is_some_and(|component| {
            !component.is_empty()
                && component
                    .chars()
                    .all(|character| character.is_ascii_digit())
        }) && components.next().is_some_and(|component| {
            !component.is_empty()
                && component
                    .chars()
                    .all(|character| character.is_ascii_digit())
        }) && components.next().is_none();
        if !valid {
            bail!("malformed Zig target ABI suffix in `{target}`");
        }
    }
    let zig = match (suffix, libc) {
        (Some(suffix), _) => format!("{zig_target}.{suffix}"),
        (None, Some("gnu")) => format!("{zig_target}.2.{DEFAULT_GLIBC_MINOR}"),
        (None, _) => zig_target.to_string(),
    };
    Ok(Some(Target {
        rust: rust_target,
        zig,
        cmake_processor,
        use_zig_as_rust_linker,
    }))
}

/// The glibc minor version Corgi targets when a triple has no `.2.N` suffix.
///
/// Zig itself defaults to 2.31. Corgi builds for the machines it runs on, and
/// 2.35 (Ubuntu 22.04) is the oldest glibc its pinned C++ runtime supports;
/// see [`crate::libraries::CXX_RUNTIME_MIN_GLIBC_MINOR`]. A target that names
/// an older glibc, such as `x86_64-unknown-linux-gnu.2.31`, still gets it.
pub const DEFAULT_GLIBC_MINOR: &str = "35";

/// The glibc minor version a Zig target such as `x86_64-linux-gnu.2.35` names.
pub fn glibc_minor(zig_target: &str) -> Option<u32> {
    let (triple, version) = zig_target.split_once('.')?;
    triple.ends_with("-linux-gnu").then_some(())?;
    version.strip_prefix("2.")?.parse().ok()
}

/// Clang flags that make a separate header parser (bindgen's libclang) see the
/// libc headers `zig cc` compiles against for a glibc `zig_target`.
///
/// `zig cc` passes these itself: `-nostdlibinc` hides the host's
/// `/usr/include`, and the libc directories follow Clang's builtin headers.
/// Returns None for targets other than Linux glibc.
pub fn glibc_header_arguments(zig_executable: &Path, zig_target: &str) -> Option<Vec<String>> {
    let (triple, version) = match zig_target.split_once('.') {
        Some((triple, version)) => (triple, Some(version)),
        None => (zig_target, None),
    };
    let architecture = triple.strip_suffix("-linux-gnu")?;
    let family = match architecture {
        "x86_64" => "x86",
        "aarch64" => "aarch64",
        _ => return None,
    };
    let minor = version
        .and_then(|version| version.split('.').nth(1))
        .unwrap_or(DEFAULT_GLIBC_MINOR);
    let include = zig_executable.parent()?.join("lib/libc/include");
    let mut arguments = vec![
        "-nostdlibinc".to_string(),
        format!("-D__GLIBC_MINOR__={minor}"),
    ];
    for directory in [
        format!("{family}-linux-gnu"),
        "generic-glibc".to_string(),
        format!("{family}-linux-any"),
        "any-linux-any".to_string(),
    ] {
        arguments.push("-idirafter".to_string());
        arguments.push(include.join(directory).display().to_string());
    }
    Some(arguments)
}

pub fn rust_target(target_name: &str) -> Result<&str> {
    Ok(target(target_name)?
        .map(|target| target.rust)
        .unwrap_or(target_name))
}

pub fn target_requires_zig(host: &str, target_name: &str) -> Result<bool> {
    let Some(target) = target(target_name)? else {
        return Ok(false);
    };
    Ok(target.rust != host || target_name != target.rust)
}

pub fn is_linker_invocation(arguments: &[std::ffi::OsString]) -> bool {
    let executable_path = arguments.first().map(PathBuf::from);
    let executable = executable_path
        .as_deref()
        .and_then(Path::file_name)
        .map(OsStr::to_owned);
    let direct_wrapper = executable_path
        .as_deref()
        .and_then(Path::parent)
        .is_some_and(|parent| parent.join("target").is_file());
    matches!(
        executable.as_deref().and_then(OsStr::to_str),
        Some("ar" | "lib" | "dlltool" | "ar.exe" | "lib.exe" | "dlltool.exe")
    ) || (direct_wrapper
        && matches!(
            executable.as_deref().and_then(OsStr::to_str),
            Some("cc" | "c++")
        ))
        || arguments
            .get(1)
            .is_some_and(|argument| argument == "zig" || argument == "__zig-driver")
}

pub fn run_linker_invocation(arguments: &[std::ffi::OsString]) -> Result<()> {
    let executable_path = arguments
        .first()
        .map(PathBuf::from)
        .context("Zig linker wrapper has no executable path")?;
    let executable = executable_path
        .file_name()
        .map(OsStr::to_owned)
        .context("Zig linker wrapper has no executable name")?;
    let direct_wrapper_directory = executable_path
        .parent()
        .filter(|parent| parent.join("target").is_file());
    let (command, argument_start) = if arguments
        .get(1)
        .is_some_and(|argument| argument == "zig" || argument == "__zig-driver")
    {
        (
            arguments
                .get(2)
                .and_then(|argument| argument.to_str())
                .context("Zig linker wrapper has no command")?,
            if arguments.get(3).is_some_and(|argument| argument == "--") {
                4
            } else {
                3
            },
        )
    } else {
        (
            executable
                .to_str()
                .context("Zig linker wrapper executable is not UTF-8")?
                .trim_end_matches(".exe"),
            1,
        )
    };
    let mut args = Vec::new();
    let mut library_directories = Vec::new();
    let mut cxx_runtime = None;
    let mut links = false;
    let has_pinned_target = direct_wrapper_directory.is_some();
    if let Some(wrapper_directory) = direct_wrapper_directory {
        let zig_path = std::fs::read_to_string(wrapper_directory.join("zig-path"))
            .context("reading pinned Zig path")?;
        // SAFETY: Linker dispatch happens at process startup before Corgi
        // creates any threads, and cargo-zigbuild only reads this value.
        unsafe {
            std::env::set_var("CARGO_ZIGBUILD_ZIG_PATH", &zig_path);
        }
        if matches!(command, "cc" | "c++") && arguments[argument_start..] != ["--version"] {
            let target = std::fs::read_to_string(wrapper_directory.join("target"))
                .context("reading Zig linker target")?;
            let manifest_directory =
                std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR is not set")?;
            args.extend([
                "-g".to_string(),
                "-fno-sanitize=all".to_string(),
                format!("-fdebug-prefix-map={manifest_directory}=."),
                "-fdebug-compilation-dir=.".to_string(),
                "-target".to_string(),
                target.clone(),
            ]);
            links = !arguments[argument_start..]
                .iter()
                .any(|argument| matches!(argument.to_str(), Some("-c" | "-S" | "-E")));
            if let Ok(root) = std::fs::read_to_string(wrapper_directory.join(CXX_RUNTIME_FILE)) {
                cxx_runtime = Some(CxxRuntime {
                    root: PathBuf::from(root),
                    link_directory: wrapper_directory.join(CXX_LINK_DIRECTORY),
                    zig_path: PathBuf::from(&zig_path),
                    zig_target: target.clone(),
                });
            }
            if links {
                std::env::set_current_dir("/").context("changing Zig linker working directory")?;
                // Declared libraries resolve `-l` from the pinned sysroot.
                // They follow the caller's own search paths, so a library a
                // build script compiled into OUT_DIR still wins.
                if let Ok(sysroot) =
                    std::fs::read_to_string(wrapper_directory.join(crate::pkg_config::SYSROOT_FILE))
                {
                    library_directories = crate::libraries::library_directories(
                        Path::new(&sysroot),
                        crate::libraries::multiarch(&target),
                    );
                }
            }
        }
    }
    let mut supplied_arguments = arguments[argument_start..].iter();
    while let Some(argument) = supplied_arguments.next() {
        let argument = argument
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("Zig linker argument is not UTF-8"))?;
        if has_pinned_target && matches!(argument, "-target" | "--target") {
            supplied_arguments.next();
            continue;
        }
        if has_pinned_target
            && (argument.starts_with("-target=") || argument.starts_with("--target="))
        {
            continue;
        }
        args.push(argument.to_string());
    }
    if let Some(runtime) = &cxx_runtime {
        if command == "c++" && !links {
            return compile_against_libstdcxx(runtime, args);
        }
        if command == "c++" {
            args = compile_sources_for_link(runtime, args)?;
        }
        args = link_libstdcxx(args, command == "c++", &runtime.link_directory)?;
        for directory in library_directories {
            args.push(format!("-L{}", directory.display()));
        }
        return Zig::Cc { args }.execute();
    }
    if command == "c++" {
        args = compile_c_inputs_as_cxx(args);
    }
    for directory in library_directories {
        args.push(format!("-L{}", directory.display()));
    }
    let zig = match command {
        "cc" => Zig::Cc { args },
        "c++" => Zig::Cxx { args },
        "ar" => Zig::Ar { args },
        "ranlib" => Zig::Ranlib { args },
        "lib" => Zig::Lib { args },
        "dlltool" => Zig::Dlltool { args },
        _ => bail!("unsupported Zig linker command `{command}`"),
    };
    zig.execute()
}

/// Compile C++ against the pinned libstdc++ instead of Zig's libc++.
///
/// This runs Zig's Clang directly, as the macOS driver does, because `zig c++`
/// always adds libc++'s headers and defines. Clang's g++ driver mode also
/// compiles `.c` inputs as C++, as `g++` and `clang++` do.
fn compile_against_libstdcxx(runtime: &CxxRuntime, args: Vec<String>) -> Result<()> {
    let status = std::process::Command::new(&runtime.zig_path)
        .args(libstdcxx_compile_arguments(runtime)?)
        .args(without_target(args))
        .status()
        .context("running Zig's Clang")?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn libstdcxx_compile_arguments(runtime: &CxxRuntime) -> Result<Vec<String>> {
    let multiarch = crate::libraries::multiarch(&runtime.zig_target);
    let (architecture, system) = multiarch
        .split_once('-')
        .with_context(|| format!("malformed Zig target `{}`", runtime.zig_target))?;
    let resource_directory = runtime
        .zig_path
        .parent()
        .context("Zig executable has no parent")?
        .join("lib");
    let mut arguments = vec![
        "clang".to_string(),
        "--no-default-config".to_string(),
        "--driver-mode=g++".to_string(),
        "-target".to_string(),
        format!("{architecture}-unknown-{system}"),
        "-resource-dir".to_string(),
        resource_directory.display().to_string(),
        // `zig cc` defaults to position-independent code; keep objects
        // linkable into the same executables and shared libraries.
        "-fPIC".to_string(),
        "-nostdinc++".to_string(),
    ];
    for directory in crate::libraries::cxx_runtime_include_directories(&runtime.root, multiarch) {
        arguments.push("-isystem".to_string());
        arguments.push(directory.display().to_string());
    }
    arguments.extend(
        glibc_header_arguments(&runtime.zig_path, &runtime.zig_target)
            .with_context(|| format!("no glibc headers for Zig target `{}`", runtime.zig_target))?,
    );
    Ok(arguments)
}

fn without_target(args: Vec<String>) -> Vec<String> {
    let mut result = Vec::with_capacity(args.len());
    let mut arguments = args.into_iter();
    while let Some(argument) = arguments.next() {
        if argument == "-target" {
            arguments.next();
            continue;
        }
        result.push(argument);
    }
    result
}

/// Options whose separate value is not an input file.
const OPTIONS_WITH_VALUES: &[&str] = &[
    "-o", "-MF", "-MT", "-MQ", "-include", "-imacros", "-isystem", "-I", "-L", "-x", "-target",
    "-Xlinker", "-u", "-T", "-z", "-e",
];

fn is_source_input(argument: &str) -> bool {
    !argument.starts_with('-')
        && [".c", ".cc", ".cp", ".cpp", ".cxx", ".c++", ".C"]
            .iter()
            .any(|extension| argument.ends_with(extension))
}

/// Compile the sources of a compile-and-link `c++` invocation to objects.
///
/// CMake's compiler checks link test programs straight from source. Each
/// source compiles against libstdc++ first, so the link that follows sees
/// only objects and `zig cc` never compiles C++ against libc++ itself.
fn compile_sources_for_link(runtime: &CxxRuntime, args: Vec<String>) -> Result<Vec<String>> {
    let mut sources = Vec::new();
    let mut previous_takes_value = false;
    for (index, argument) in args.iter().enumerate() {
        if !previous_takes_value && is_source_input(argument) {
            sources.push(index);
        }
        previous_takes_value = OPTIONS_WITH_VALUES.contains(&argument.as_str());
    }
    if sources.is_empty() {
        return Ok(args);
    }
    let output = args
        .iter()
        .position(|argument| argument == "-o")
        .and_then(|index| args.get(index + 1))
        .map_or_else(|| PathBuf::from("a.out"), PathBuf::from);
    let mut compile_options = Vec::new();
    let mut skip_value = false;
    for (index, argument) in args.iter().enumerate() {
        if skip_value {
            skip_value = false;
            continue;
        }
        if sources.contains(&index) || argument.starts_with("-l") || argument.starts_with("-Wl,") {
            continue;
        }
        if matches!(
            argument.as_str(),
            "-o" | "-L" | "-Xlinker" | "-T" | "-u" | "-e" | "-z"
        ) {
            skip_value = true;
            continue;
        }
        if argument.starts_with("-L") {
            continue;
        }
        compile_options.push(argument.clone());
    }
    let mut result = args.clone();
    for (position, &index) in sources.iter().enumerate() {
        let object = PathBuf::from(format!("{}.corgi-{position}.o", output.display()));
        let status = std::process::Command::new(&runtime.zig_path)
            .args(libstdcxx_compile_arguments(runtime)?)
            .args(without_target(compile_options.clone()))
            .arg("-c")
            .arg(&args[index])
            .arg("-o")
            .arg(&object)
            .status()
            .context("running Zig's Clang")?;
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
        result[index] = object.display().to_string();
    }
    Ok(result)
}

/// Route a link's C++ runtime to the pinned libstdc++ and libgcc_s.
///
/// Links that use libstdc++ also take the unwinder it is built against, so a
/// binary carries one unwinder rather than Zig's libunwind beside libgcc_s.
/// Links without C++ are left alone. rustc passes long argument lists through
/// a response file, which is rewritten the same way.
fn link_libstdcxx(
    args: Vec<String>,
    cxx_driver: bool,
    link_directory: &Path,
) -> Result<Vec<String>> {
    let mut uses_libstdcxx = cxx_driver;
    for argument in &args {
        if argument == "-lstdc++" {
            uses_libstdcxx = true;
        } else if let Some(path) = argument.strip_prefix('@') {
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("reading linker response file {path}"))?;
            uses_libstdcxx |= contents.lines().any(|line| line == "-lstdc++");
        }
    }
    if !uses_libstdcxx {
        return Ok(args);
    }
    let rewrite = |argument: &str| match argument {
        "-lstdc++" => LIBSTDCXX_LINK_ARGUMENT.to_string(),
        "-lgcc_s" => LIBGCC_S_LINK_ARGUMENT.to_string(),
        other => other.to_string(),
    };
    let mut result = Vec::with_capacity(args.len() + 5);
    for argument in &args {
        if let Some(path) = argument.strip_prefix('@') {
            let path = Path::new(path);
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("reading linker response file {}", path.display()))?;
            let rewritten = contents.lines().map(rewrite).collect::<Vec<_>>().join("\n");
            // cargo-zigbuild only processes response files with this suffix.
            let file_name = path
                .file_name()
                .context("linker response file has no name")?
                .to_string_lossy();
            let rewritten_path = path.with_file_name(format!("corgi-cxx-{file_name}"));
            std::fs::write(&rewritten_path, rewritten + "\n").with_context(|| {
                format!("writing linker response file {}", rewritten_path.display())
            })?;
            result.push(format!("@{}", rewritten_path.display()));
        } else {
            result.push(rewrite(argument));
        }
    }
    result.push(format!("-L{}", link_directory.display()));
    if cxx_driver {
        result.extend([
            LIBSTDCXX_LINK_ARGUMENT.to_string(),
            LIBGCC_S_LINK_ARGUMENT.to_string(),
            "-lm".to_string(),
        ]);
    }
    Ok(result)
}

/// Mark `.c` inputs to `c++` as C++, as `clang++` and `g++` treat them.
///
/// `zig c++` compiles a `.c` file as C, so a build script that hands `.c`
/// sources to its C++ build fails once that build passes a C++-only flag such
/// as `-std=c++20` (webrtc-sys does). An explicit `-x` from the caller already
/// says how to read its inputs, so it is left alone.
fn compile_c_inputs_as_cxx(args: Vec<String>) -> Vec<String> {
    if args.iter().any(|argument| argument.starts_with("-x")) {
        return args;
    }
    // Options whose separate value could itself end in `.c`.
    const TAKES_VALUE: &[&str] = &["-o", "-MF", "-MT", "-MQ", "-include", "-imacros"];
    let mut result = Vec::with_capacity(args.len());
    let mut previous_takes_value = false;
    for argument in args {
        let is_c_input =
            !previous_takes_value && !argument.starts_with('-') && argument.ends_with(".c");
        previous_takes_value = TAKES_VALUE.contains(&argument.as_str());
        if is_c_input {
            result.extend(["-x".to_string(), "c++".to_string(), argument]);
            result.extend(["-x".to_string(), "none".to_string()]);
        } else {
            result.push(argument);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(arguments: &[&str]) -> Vec<String> {
        arguments
            .iter()
            .map(|argument| argument.to_string())
            .collect()
    }

    fn test_runtime() -> CxxRuntime {
        CxxRuntime {
            root: PathBuf::from("/store/sysroots/cxx"),
            link_directory: PathBuf::from("/store/zig-wrappers/w/cxx-link"),
            zig_path: PathBuf::from("/store/tools/zig/zig"),
            zig_target: "x86_64-linux-gnu.2.35".to_string(),
        }
    }

    #[test]
    fn cxx_compiles_use_clang_with_libstdcxx_headers() {
        let arguments = libstdcxx_compile_arguments(&test_runtime()).unwrap();
        assert_eq!(
            arguments[..8],
            strings(&[
                "clang",
                "--no-default-config",
                "--driver-mode=g++",
                "-target",
                "x86_64-unknown-linux-gnu",
                "-resource-dir",
                "/store/tools/zig/lib",
                "-fPIC",
            ])
        );
        let headers = arguments
            .windows(2)
            .filter(|pair| pair[0] == "-isystem")
            .map(|pair| pair[1].as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            headers,
            [
                "/store/sysroots/cxx/usr/include/c++/12",
                "/store/sysroots/cxx/usr/include/x86_64-linux-gnu/c++/12",
                "/store/sysroots/cxx/usr/include/c++/12/backward",
            ]
        );
        assert!(arguments.iter().any(|argument| argument == "-nostdinc++"));
        assert!(arguments
            .iter()
            .any(|argument| argument == "-D__GLIBC_MINOR__=35"));
        assert_eq!(
            without_target(strings(&["-target", "x86_64-linux-gnu.2.31", "-c", "a.cc"])),
            strings(&["-c", "a.cc"])
        );
    }

    #[test]
    fn links_using_libstdcxx_take_the_pinned_runtime() {
        let link_directory = Path::new("/store/cxx-link");
        assert_eq!(
            link_libstdcxx(
                strings(&["main.o", "-lstdc++", "-lgcc_s", "-lc"]),
                false,
                link_directory
            )
            .unwrap(),
            strings(&[
                "main.o",
                "-lcorgi_libstdcxx",
                "-lcorgi_libgcc_s",
                "-lc",
                "-L/store/cxx-link",
            ])
        );
        assert_eq!(
            link_libstdcxx(strings(&["main.o"]), true, link_directory).unwrap(),
            strings(&[
                "main.o",
                "-L/store/cxx-link",
                "-lcorgi_libstdcxx",
                "-lcorgi_libgcc_s",
                "-lm",
            ])
        );
    }

    #[test]
    fn links_without_libstdcxx_keep_zigs_runtime() {
        let arguments = strings(&["main.o", "-lgcc_s", "-lc"]);
        assert_eq!(
            link_libstdcxx(arguments.clone(), false, Path::new("/store/cxx-link")).unwrap(),
            arguments
        );
    }

    #[test]
    fn response_files_are_rewritten_for_libstdcxx() {
        let directory =
            std::env::temp_dir().join(format!("corgi-zig-response-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let response = directory.join("linker-arguments");
        std::fs::write(&response, "main.o\n-lstdc++\n-lgcc_s\n").unwrap();
        let result = link_libstdcxx(
            vec![format!("@{}", response.display())],
            false,
            Path::new("/store/cxx-link"),
        )
        .unwrap();
        let rewritten = directory.join("corgi-cxx-linker-arguments");
        assert_eq!(
            result,
            vec![
                format!("@{}", rewritten.display()),
                "-L/store/cxx-link".to_string(),
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&rewritten).unwrap(),
            "main.o\n-lcorgi_libstdcxx\n-lcorgi_libgcc_s\n"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn c_inputs_to_cxx_compile_as_cxx() {
        assert_eq!(
            compile_c_inputs_as_cxx(strings(&[
                "-std=c++20",
                "-o",
                "out/lib.so.init.o",
                "-c",
                "src/lib.so.init.c",
            ])),
            strings(&[
                "-std=c++20",
                "-o",
                "out/lib.so.init.o",
                "-c",
                "-x",
                "c++",
                "src/lib.so.init.c",
                "-x",
                "none",
            ])
        );
    }

    #[test]
    fn other_inputs_and_option_values_are_unchanged() {
        let arguments = strings(&["-c", "src/tramp.S", "-o", "out/generated.c", "src/a.cpp"]);
        assert_eq!(compile_c_inputs_as_cxx(arguments.clone()), arguments);
    }

    #[test]
    fn an_explicit_language_is_respected() {
        let arguments = strings(&["-x", "c", "-c", "src/a.c"]);
        assert_eq!(compile_c_inputs_as_cxx(arguments.clone()), arguments);
    }

    #[test]
    fn release_assets_are_fully_pinned() {
        for host in [
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "aarch64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
        ] {
            let asset = asset(host).unwrap();
            assert_eq!(asset.sha256.len(), 64);
            assert!(url(&asset).contains(VERSION));
            assert!(archive_root(&asset).contains(asset.platform));
        }
    }

    #[test]
    fn unsupported_hosts_are_rejected() {
        assert!(asset("powerpc-unknown-aix").is_err());
    }

    #[test]
    fn linux_targets_map_to_zig() {
        for (rust, zig, processor) in [
            (
                "x86_64-unknown-linux-gnu.2.31",
                "x86_64-linux-gnu.2.31",
                "x86_64",
            ),
            (
                "aarch64-unknown-linux-gnu.2.31",
                "aarch64-linux-gnu.2.31",
                "aarch64",
            ),
            (
                "x86_64-unknown-linux-gnu",
                "x86_64-linux-gnu.2.35",
                "x86_64",
            ),
            ("x86_64-unknown-linux-musl", "x86_64-linux-musl", "x86_64"),
            (
                "aarch64-unknown-linux-musl",
                "aarch64-linux-musl",
                "aarch64",
            ),
        ] {
            let target = target(rust).unwrap().unwrap();
            assert_eq!(target.zig, zig);
            assert_eq!(target.cmake_processor, processor);
            assert!(target.use_zig_as_rust_linker);
        }
    }

    #[test]
    fn glibc_minor_reads_the_target_suffix() {
        assert_eq!(glibc_minor("x86_64-linux-gnu.2.35"), Some(35));
        assert_eq!(glibc_minor("aarch64-linux-gnu.2.31"), Some(31));
        assert_eq!(glibc_minor("x86_64-linux-musl"), None);
        assert_eq!(glibc_minor("x86_64-linux-gnu"), None);
    }

    #[test]
    fn wasm_target_maps_to_zig_freestanding() {
        let target = target("wasm32-unknown-unknown").unwrap().unwrap();

        assert_eq!(target.rust, "wasm32-unknown-unknown");
        assert_eq!(target.zig, "wasm32-freestanding");
        assert_eq!(target.cmake_processor, "wasm32");
        assert!(!target.use_zig_as_rust_linker);
    }

    #[test]
    fn abi_suffix_requires_glibc_version() {
        assert!(target("x86_64-unknown-linux-gnu.future").is_err());
        assert!(target("x86_64-unknown-linux-gnu.2").is_err());
        assert!(target("x86_64-unknown-linux-musl.1.2").is_err());
    }
}
