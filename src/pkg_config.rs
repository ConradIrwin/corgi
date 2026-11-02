//! A hermetic `pkg-config` that answers from a library sysroot.
//!
//! The Zig wrapper directory links `pkg-config` to the Corgi driver, and build
//! scripts receive it as `PKG_CONFIG`. It reads `.pc` files from the sysroot
//! recorded beside it (see [`crate::libraries`]) and from any `PKG_CONFIG_PATH`
//! a build script sets for files it generated, so a probe never sees host
//! libraries. `-I` and `-L` paths are prefixed with the sysroot, as
//! `PKG_CONFIG_SYSROOT_DIR` does for other implementations.
//!
//! It implements what build scripts use: `--libs`, `--cflags` and their
//! `-only-` variants, `--static`, `--modversion`, `--exists`, `--variable`,
//! version constraints, and `Requires`. Like pkgconf, a missing
//! `Requires.private` module only matters for `--static`.

use anyhow::{Context, Result};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The wrapper-directory name under which Corgi acts as `pkg-config`.
pub const EXECUTABLE: &str = "pkg-config";

/// Wrapper-directory file naming the library sysroot, when there is one.
pub const SYSROOT_FILE: &str = "library-sysroot";

/// The version reported by `--version`, matching the reference implementation
/// whose behavior this follows.
const COMPATIBLE_VERSION: &str = "0.29.2";

/// Whether Corgi was invoked through a wrapper directory's `pkg-config` link.
pub fn is_invocation(arguments: &[OsString]) -> bool {
    let Some(executable) = arguments.first().map(Path::new) else {
        return false;
    };
    executable.file_name().and_then(|name| name.to_str()) == Some(EXECUTABLE)
        && executable
            .parent()
            .is_some_and(|parent| parent.join("target").is_file())
}

/// Answer a `pkg-config` invocation and exit with its status.
pub fn run_invocation(arguments: &[OsString]) -> Result<()> {
    let wrapper = Path::new(&arguments[0])
        .parent()
        .context("pkg-config wrapper has no directory")?;
    let target = std::fs::read_to_string(wrapper.join("target"))
        .context("reading the wrapper's Zig target")?;
    let sysroot = std::fs::read_to_string(wrapper.join(SYSROOT_FILE))
        .ok()
        .map(PathBuf::from);
    let mut search_path = std::env::var_os("PKG_CONFIG_PATH")
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    search_path.retain(|directory| !directory.as_os_str().is_empty());
    if let Some(root) = &sysroot {
        search_path.extend(crate::libraries::pkg_config_directories(
            root,
            crate::libraries::multiarch(target.trim()),
        ));
    }
    let arguments = arguments[1..]
        .iter()
        .map(|argument| {
            argument
                .to_str()
                .map(str::to_owned)
                .context("pkg-config argument is not UTF-8")
        })
        .collect::<Result<Vec<_>>>()?;
    let output = query(&arguments, &search_path, sysroot.as_deref());
    print!("{}", output.stdout);
    eprint!("{}", output.stderr);
    std::process::exit(output.status)
}

/// The result of one `pkg-config` invocation.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub status: i32,
}

#[derive(Default)]
struct Options {
    libs_only_l: bool,
    libs_only_search: bool,
    libs_only_other: bool,
    cflags_only_include: bool,
    cflags_only_other: bool,
    is_static: bool,
    modversion: bool,
    print_requires: bool,
    print_requires_private: bool,
    variable: Option<String>,
    constraint: Option<(String, String)>,
    show_version: bool,
}

/// Answer `arguments` from the `.pc` files in `search_path`, prefixing
/// absolute `-I`/`-L` paths with `sysroot`.
pub fn query(arguments: &[String], search_path: &[PathBuf], sysroot: Option<&Path>) -> Output {
    let mut options = Options::default();
    let mut positional = Vec::new();
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        let (flag, inline_value) = match argument.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_string())),
            _ => (argument.as_str(), None),
        };
        let mut value = || inline_value.clone().or_else(|| arguments.next().cloned());
        match flag {
            "--libs" => {
                options.libs_only_l = true;
                options.libs_only_search = true;
                options.libs_only_other = true;
            }
            "--libs-only-l" => options.libs_only_l = true,
            "--libs-only-L" => options.libs_only_search = true,
            "--libs-only-other" => options.libs_only_other = true,
            "--cflags" => {
                options.cflags_only_include = true;
                options.cflags_only_other = true;
            }
            "--cflags-only-I" => options.cflags_only_include = true,
            "--cflags-only-other" => options.cflags_only_other = true,
            "--static" => options.is_static = true,
            "--modversion" => options.modversion = true,
            "--print-requires" => options.print_requires = true,
            "--print-requires-private" => options.print_requires_private = true,
            "--version" => options.show_version = true,
            "--variable" => options.variable = value(),
            "--atleast-version" => options.constraint = value().map(|v| (">=".into(), v)),
            "--exact-version" => options.constraint = value().map(|v| ("=".into(), v)),
            "--max-version" => options.constraint = value().map(|v| ("<=".into(), v)),
            // Accept a minimum pkg-config version without checking it.
            "--atleast-pkgconfig-version" => {
                value();
            }
            // Error-reporting and system-path switches do not change answers.
            _ if flag.starts_with("--") => {}
            _ => positional.push(argument.clone()),
        }
    }

    let mut output = Output::default();
    if options.show_version {
        output.stdout = format!("{COMPATIBLE_VERSION}\n");
        return output;
    }
    let mut requests = parse_requirements(&positional.join(" "));
    if let Some((operator, version)) = options.constraint.take() {
        for request in &mut requests {
            request.constraint = Some((operator.clone(), version.clone()));
        }
    }
    if requests.is_empty() {
        output.stderr = "Must specify package names on the command line\n".into();
        output.status = 1;
        return output;
    }

    let mut resolver = Resolver {
        search_path,
        sysroot,
        loaded: HashMap::new(),
    };
    let mut roots = Vec::new();
    for request in &requests {
        match resolver.require(request, "") {
            Ok(name) => roots.push(name),
            Err(message) => {
                output.stderr.push_str(&message);
                output.status = 1;
            }
        }
    }
    if output.status != 0 {
        return output;
    }

    if options.modversion {
        for name in &roots {
            let _ = writeln!(output.stdout, "{}", resolver.loaded[name].field("Version"));
        }
    }
    if let Some(variable) = &options.variable {
        let values = roots
            .iter()
            .map(|name| resolver.loaded[name].variable(variable))
            .collect::<Vec<_>>();
        let _ = writeln!(output.stdout, "{}", values.join(" "));
    }
    if options.print_requires || options.print_requires_private {
        let field = if options.print_requires {
            "Requires"
        } else {
            "Requires.private"
        };
        for name in &roots {
            for requirement in parse_requirements(&resolver.loaded[name].field(field)) {
                let _ = writeln!(output.stdout, "{}", requirement.name);
            }
        }
    }

    let wants_cflags = options.cflags_only_include || options.cflags_only_other;
    let wants_libs = options.libs_only_l || options.libs_only_search || options.libs_only_other;
    if !(wants_cflags || wants_libs) {
        return output;
    }
    // Compile flags always follow private requirements, as in pkg-config;
    // link flags only do for static links.
    let mut flags = Vec::new();
    if wants_cflags {
        match resolver.closure(&roots, true, false) {
            Ok(order) => {
                let cflags = order
                    .iter()
                    .flat_map(|name| resolver.loaded[name].flags("Cflags", sysroot))
                    .collect::<Vec<_>>();
                flags.extend(select_flags(
                    deduplicate(cflags),
                    &[
                        ("-I", options.cflags_only_include),
                        ("", options.cflags_only_other),
                    ],
                ));
            }
            Err(message) => {
                output.stderr.push_str(&message);
                output.status = 1;
                return output;
            }
        }
    }
    if wants_libs {
        match resolver.closure(&roots, options.is_static, options.is_static) {
            Ok(order) => {
                let libs = order
                    .iter()
                    .flat_map(|name| {
                        let package = &resolver.loaded[name];
                        let mut flags = package.flags("Libs", sysroot);
                        if options.is_static {
                            flags.extend(package.flags("Libs.private", sysroot));
                        }
                        flags
                    })
                    .collect::<Vec<_>>();
                flags.extend(select_flags(
                    deduplicate(libs),
                    &[
                        ("-L", options.libs_only_search),
                        ("-l", options.libs_only_l),
                        ("", options.libs_only_other),
                    ],
                ));
            }
            Err(message) => {
                output.stderr.push_str(&message);
                output.status = 1;
                return output;
            }
        }
    }
    let _ = writeln!(output.stdout, "{}", flags.join(" "));
    output
}

/// A requested module and its optional `(operator, version)` constraint.
struct Requirement {
    name: String,
    constraint: Option<(String, String)>,
}

/// Parse `"a >= 1.0, b c"` into requirements.
fn parse_requirements(text: &str) -> Vec<Requirement> {
    let tokens = text
        .split(|character: char| character.is_whitespace() || character == ',')
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let mut requirements = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let name = tokens[index].to_string();
        index += 1;
        let constraint = match tokens.get(index) {
            Some(&operator) if matches!(operator, "<" | "<=" | "=" | "!=" | ">=" | ">") => {
                let version = tokens.get(index + 1).map(|version| version.to_string());
                index += 2;
                version.map(|version| (operator.to_string(), version))
            }
            _ => None,
        };
        requirements.push(Requirement { name, constraint });
    }
    requirements
}

struct Resolver<'a> {
    search_path: &'a [PathBuf],
    sysroot: Option<&'a Path>,
    loaded: HashMap<String, Package>,
}

impl Resolver<'_> {
    /// Load `requirement` and check its version, returning its module name.
    fn require(&mut self, requirement: &Requirement, required_by: &str) -> Result<String, String> {
        let name = &requirement.name;
        if !self.loaded.contains_key(name) {
            let package = self
                .search_path
                .iter()
                .map(|directory| directory.join(format!("{name}.pc")))
                .find(|path| path.is_file())
                .and_then(|path| Package::load(&path, self.sysroot).ok());
            let Some(package) = package else {
                let required_by = if required_by.is_empty() {
                    String::new()
                } else {
                    format!(", required by '{required_by}',")
                };
                return Err(format!(
                    "Package '{name}'{required_by} not found\n\
                     Corgi's pkg-config only sees libraries declared in corgi.toml: \
                     add a [libraries] entry whose archive provides {name}.pc\n"
                ));
            };
            self.loaded.insert(name.clone(), package);
        }
        if let Some((operator, wanted)) = &requirement.constraint {
            let actual = self.loaded[name].field("Version");
            let ordering = compare_versions(&actual, wanted);
            let satisfied = match operator.as_str() {
                "<" => ordering == Ordering::Less,
                "<=" => ordering != Ordering::Greater,
                "=" => ordering == Ordering::Equal,
                "!=" => ordering != Ordering::Equal,
                ">=" => ordering != Ordering::Less,
                ">" => ordering == Ordering::Greater,
                _ => false,
            };
            if !satisfied {
                return Err(format!(
                    "Requested '{name} {operator} {wanted}' but version of {} is {actual}\n",
                    self.loaded[name].field("Name")
                ));
            }
        }
        Ok(name.clone())
    }

    /// Every module `roots` needs, ordered so each precedes its requirements.
    ///
    /// `private` follows `Requires.private`; a missing private module is an
    /// error only when `private_required`.
    fn closure(
        &mut self,
        roots: &[String],
        private: bool,
        private_required: bool,
    ) -> Result<Vec<String>, String> {
        let mut finished = Vec::new();
        let mut visiting = Vec::new();
        for root in roots {
            self.visit(
                root,
                private,
                private_required,
                &mut visiting,
                &mut finished,
            )?;
        }
        finished.reverse();
        Ok(finished)
    }

    fn visit(
        &mut self,
        name: &str,
        private: bool,
        private_required: bool,
        visiting: &mut Vec<String>,
        finished: &mut Vec<String>,
    ) -> Result<(), String> {
        if finished.iter().any(|done| done == name) || visiting.iter().any(|open| open == name) {
            return Ok(());
        }
        visiting.push(name.to_string());
        let package = &self.loaded[name];
        let mut requirements = parse_requirements(&package.field("Requires"))
            .into_iter()
            .map(|requirement| (requirement, true))
            .collect::<Vec<_>>();
        if private {
            requirements.extend(
                parse_requirements(&package.field("Requires.private"))
                    .into_iter()
                    .map(|requirement| (requirement, private_required)),
            );
        }
        for (requirement, required) in requirements {
            match self.require(&requirement, name) {
                Ok(dependency) => {
                    self.visit(&dependency, private, private_required, visiting, finished)?
                }
                Err(message) if required => return Err(message),
                Err(_) => {}
            }
        }
        visiting.pop();
        finished.push(name.to_string());
        Ok(())
    }
}

/// A parsed `.pc` file with variables already expanded into its fields.
struct Package {
    variables: HashMap<String, String>,
    fields: HashMap<String, String>,
}

impl Package {
    fn load(path: &Path, sysroot: Option<&Path>) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let mut variables = HashMap::new();
        let directory = path.parent().context(".pc file has no directory")?;
        variables.insert("pcfiledir".to_string(), directory.display().to_string());
        variables.insert(
            "pc_sysrootdir".to_string(),
            sysroot.map_or("/".to_string(), |root| root.display().to_string()),
        );
        let mut fields = HashMap::new();
        for line in text.replace("\\\n", " ").lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let field = line.find(':');
            let assignment = line.find('=');
            match (field, assignment) {
                (Some(colon), equals) if equals.is_none_or(|equals| colon < equals) => {
                    let value = expand(line[colon + 1..].trim(), &variables);
                    fields.insert(line[..colon].trim().to_string(), value);
                }
                (_, Some(equals)) => {
                    let value = expand(line[equals + 1..].trim(), &variables);
                    variables.insert(line[..equals].trim().to_string(), value);
                }
                _ => {}
            }
        }
        Ok(Self { variables, fields })
    }

    fn field(&self, name: &str) -> String {
        self.fields.get(name).cloned().unwrap_or_default()
    }

    fn variable(&self, name: &str) -> String {
        self.variables.get(name).cloned().unwrap_or_default()
    }

    /// The field split into flags, with absolute `-I`/`-L` paths moved into
    /// `sysroot`.
    fn flags(&self, field: &str, sysroot: Option<&Path>) -> Vec<String> {
        split_flags(&self.field(field))
            .into_iter()
            .map(|flag| match sysroot {
                Some(root) => {
                    for prefix in ["-I", "-L"] {
                        if let Some(path) = flag.strip_prefix(prefix) {
                            if path.starts_with('/') && !Path::new(path).starts_with(root) {
                                return format!("{prefix}{}{path}", root.display());
                            }
                        }
                    }
                    flag
                }
                None => flag,
            })
            .collect()
    }
}

/// Expand `${name}` references; `$$` is a literal `$`.
fn expand(value: &str, variables: &HashMap<String, String>) -> String {
    let mut expanded = String::new();
    let mut rest = value;
    while let Some(start) = rest.find('$') {
        expanded.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(after) = after.strip_prefix('$') {
            expanded.push('$');
            rest = after;
        } else if let Some((name, after)) = after
            .strip_prefix('{')
            .and_then(|inner| inner.split_once('}'))
        {
            expanded.push_str(variables.get(name).map_or("", String::as_str));
            rest = after;
        } else {
            expanded.push('$');
            rest = after;
        }
    }
    expanded.push_str(rest);
    expanded
}

/// Split flags on whitespace, honoring quotes and backslash escapes.
fn split_flags(text: &str) -> Vec<String> {
    let mut flags = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        match (character, quote) {
            ('\\', _) => current.extend(characters.next()),
            ('"' | '\'', None) => quote = Some(character),
            (character, Some(open)) if character == open => quote = None,
            (character, None) if character.is_whitespace() => {
                if !current.is_empty() {
                    flags.push(std::mem::take(&mut current));
                }
            }
            (character, _) => current.push(character),
        }
    }
    if !current.is_empty() {
        flags.push(current);
    }
    flags
}

/// Drop repeated flags: search paths keep their first occurrence, everything
/// else its last, so a library still follows every library that needs it.
fn deduplicate(flags: Vec<String>) -> Vec<String> {
    let is_path = |flag: &str| flag.starts_with("-I") || flag.starts_with("-L");
    let mut kept = Vec::new();
    for (index, flag) in flags.iter().enumerate() {
        let duplicate = if is_path(flag) {
            flags[..index].contains(flag)
        } else {
            flags[index + 1..].contains(flag)
        };
        if !duplicate {
            kept.push(flag.clone());
        }
    }
    kept
}

/// Keep the flags whose class is selected; `""` stands for "anything else".
fn select_flags(flags: Vec<String>, classes: &[(&str, bool)]) -> Vec<String> {
    flags
        .into_iter()
        .filter(|flag| {
            let class = classes
                .iter()
                .find(|(prefix, _)| !prefix.is_empty() && flag.starts_with(prefix))
                .or_else(|| classes.iter().find(|(prefix, _)| prefix.is_empty()));
            class.is_some_and(|(_, selected)| *selected)
        })
        .collect()
}

/// Compare versions segment by segment, as pkg-config's rpmvercmp does:
/// digit runs numerically, letter runs lexically, digits beating letters.
fn compare_versions(left: &str, right: &str) -> Ordering {
    let segments = |version: &str| {
        let mut segments = Vec::new();
        let mut characters = version.chars().peekable();
        while let Some(&character) = characters.peek() {
            if !character.is_ascii_alphanumeric() {
                characters.next();
                continue;
            }
            let numeric = character.is_ascii_digit();
            let mut segment = String::new();
            while let Some(&next) = characters.peek() {
                if next.is_ascii_alphanumeric() && next.is_ascii_digit() == numeric {
                    segment.push(next);
                    characters.next();
                } else {
                    break;
                }
            }
            segments.push((numeric, segment));
        }
        segments
    };
    let (left, right) = (segments(left), segments(right));
    for ((left_numeric, left), (right_numeric, right)) in left.iter().zip(&right) {
        let ordering = match (left_numeric, right_numeric) {
            (true, true) => {
                let (left, right) = (left.trim_start_matches('0'), right.trim_start_matches('0'));
                left.len().cmp(&right.len()).then_with(|| left.cmp(right))
            }
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn libs_follow_public_requirements_into_the_sysroot() {
        let (sysroot, search_path) = fixture();
        let output = run(
            &["--libs", "--cflags", "fontconfig"],
            &search_path,
            &sysroot,
        );
        let root = sysroot.display();
        assert_eq!(output.status, 0, "{}", output.stderr);
        // The missing private `uuid` only matters for --static.
        assert_eq!(
            output.stdout,
            format!(
                "-I{root}/usr/include -I{root}/usr/include/freetype2 \
                 -L{root}/usr/lib/x86_64-linux-gnu -lfontconfig -lfreetype\n"
            )
        );
        let output = run(
            &["--libs", "--static", "fontconfig"],
            &search_path,
            &sysroot,
        );
        assert_eq!(output.status, 1);
        assert!(output
            .stderr
            .contains("Package 'uuid', required by 'fontconfig', not found"));
    }

    #[test]
    fn version_constraints_and_missing_packages_fail() {
        let (sysroot, search_path) = fixture();
        // The pkg-config crate passes constraints as one argument.
        let output = run(
            &["--modversion", "freetype2 >= 24.3.18"],
            &search_path,
            &sysroot,
        );
        assert_eq!(output.status, 1);
        assert_eq!(
            output.stderr,
            "Requested 'freetype2 >= 24.3.18' but version of FreeType 2 is 23.4.17\n"
        );
        let output = run(
            &["--modversion", "freetype2 >= 21.0.15"],
            &search_path,
            &sysroot,
        );
        assert_eq!(output.stdout, "23.4.17\n");
        let output = run(&["--libs", "xcb"], &search_path, &sysroot);
        assert_eq!(output.status, 1);
        assert!(output
            .stderr
            .contains("add a [libraries] entry whose archive provides xcb.pc"));
        assert_eq!(compare_versions("1.10", "1.9"), Ordering::Greater);
        assert_eq!(compare_versions("1.0a", "1.0.1"), Ordering::Less);
    }

    fn run(arguments: &[&str], search_path: &[PathBuf], sysroot: &Path) -> Output {
        let arguments = arguments
            .iter()
            .map(|argument| argument.to_string())
            .collect::<Vec<_>>();
        query(&arguments, search_path, Some(sysroot))
    }

    /// A sysroot laid out like Debian's fontconfig and freetype packages.
    fn fixture() -> (PathBuf, Vec<PathBuf>) {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, AtomicOrdering::Relaxed);
        let sysroot = std::env::temp_dir().join(format!(
            "corgi-pkg-config-test-{}-{sequence}",
            std::process::id()
        ));
        let directory = sysroot.join("usr/lib/x86_64-linux-gnu/pkgconfig");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("fontconfig.pc"),
            "prefix=/usr\nlibdir=${prefix}/lib/x86_64-linux-gnu\n\
             Name: Fontconfig\nVersion: 2.13.1\n\
             Requires:  freetype2 >= 21.0.15\nRequires.private:  uuid expat\n\
             Libs: -L${libdir} -lfontconfig\nCflags: -I${prefix}/include\n",
        )
        .unwrap();
        std::fs::write(
            directory.join("freetype2.pc"),
            "prefix=/usr\nincludedir=${prefix}/include\n\
             Name: FreeType 2\nVersion: 23.4.17\n\
             Libs: -L${prefix}/lib/x86_64-linux-gnu -lfreetype\nCflags: -I${includedir}/freetype2\n",
        )
        .unwrap();
        (sysroot, vec![directory])
    }
}
