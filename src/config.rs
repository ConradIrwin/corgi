//! Workspace `.cargo/config.toml` resolution.
//!
//! dcargo honors the narrow slice of cargo configuration that changes what
//! gets compiled: `build.rustflags`, `target.<spec>.rustflags`, and `[env]`.
//! Everything else that would alter build semantics is a hard error, so a
//! config the tool cannot faithfully reproduce never builds silently wrong.
//! Only the workspace's own `.cargo/config.toml` is read: configs in parent
//! directories or CARGO_HOME are machine-local state and stay invisible.

use anyhow::{bail, Context, Result};
use std::path::Path;

pub struct CargoConfig {
    build_rustflags: Vec<String>,
    /// Raw `target.<spec>` rustflags entries, spec as written.
    target_rustflags: Vec<(String, Vec<String>)>,
    /// `[env]` entries, sorted by name.
    pub env: Vec<(String, String)>,
}

impl CargoConfig {
    /// Rustflags for a compilation target triple, with cargo's precedence:
    /// all matching `target.<triple>` and `target.'cfg(...)'` entries are
    /// concatenated; `build.rustflags` applies only when no target entry
    /// matched. Cargo's join order for multiple matches is incidental, so
    /// dcargo pins one: literal triple entries first, then cfg entries in
    /// lexicographic spec order.
    pub fn rustflags_for(&self, triple: &str) -> Result<Vec<String>> {
        let mut flags = Vec::new();
        let mut any_match = false;
        for (spec, entry) in &self.target_rustflags {
            if spec == triple {
                any_match = true;
                flags.extend(entry.iter().cloned());
            }
        }
        let mut cfg_entries: Vec<&(String, Vec<String>)> =
            self.target_rustflags.iter().filter(|(s, _)| s.starts_with("cfg(")).collect();
        cfg_entries.sort_by(|a, b| a.0.cmp(&b.0));
        if !cfg_entries.is_empty() {
            let info = TripleInfo::of(triple)?;
            for (spec, entry) in cfg_entries {
                if eval_cfg(&parse_cfg(spec)?, &info)? {
                    any_match = true;
                    flags.extend(entry.iter().cloned());
                }
            }
        }
        if !any_match {
            flags = self.build_rustflags.clone();
        }
        Ok(flags)
    }
}

/// Walk up from the invocation directory, like cargo, to the nearest
/// `.cargo/config.toml` (or legacy `.cargo/config`). Returns the parsed
/// config and the directory it was found in; the caller verifies that hit
/// is the workspace root once cargo metadata reveals it, so member-level
/// or machine-local configs are hard errors instead of silent inputs.
pub fn discover(start: &Path) -> Result<(CargoConfig, Option<std::path::PathBuf>)> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        for name in [".cargo/config.toml", ".cargo/config"] {
            let p = d.join(name);
            if p.is_file() {
                let text = std::fs::read_to_string(&p)
                    .with_context(|| format!("reading {}", p.display()))?;
                let config =
                    parse(&text).with_context(|| format!("parsing {}", p.display()))?;
                return Ok((config, Some(d.to_path_buf())));
            }
        }
        dir = d.parent();
    }
    Ok((
        CargoConfig { build_rustflags: Vec::new(), target_rustflags: Vec::new(), env: Vec::new() },
        None,
    ))
}

fn parse(text: &str) -> Result<CargoConfig> {
    let root: toml::Value = toml::from_str(text).context("invalid TOML")?;
    let table = root.as_table().context("config root is not a table")?;
    let mut build_rustflags = Vec::new();
    let mut target_rustflags = Vec::new();
    let mut env = Vec::new();
    for (section, value) in table {
        match section.as_str() {
            "build" => {
                let entries = value.as_table().context("[build] is not a table")?;
                for (key, v) in entries {
                    match key.as_str() {
                        "rustflags" => {
                            build_rustflags = flag_list(v).context("build.rustflags")?;
                        }
                        // Parallelism and layout preferences: dcargo owns
                        // both, and rustdoc is not run.
                        "jobs" | "target-dir" | "incremental" | "rustdocflags" => {}
                        other => bail!("unsupported .cargo/config key build.{other}"),
                    }
                }
            }
            "target" => {
                let entries = value.as_table().context("[target] is not a table")?;
                for (spec, sub) in entries {
                    if spec.starts_with("cfg(") {
                        // Validate now so a broken config fails on load,
                        // not on first use of an unusual triple.
                        validate_cfg(&parse_cfg(spec)?)?;
                    }
                    let sub_entries = sub
                        .as_table()
                        .with_context(|| format!("[target.{spec}] is not a table"))?;
                    for (key, v) in sub_entries {
                        match key.as_str() {
                            "rustflags" => target_rustflags.push((
                                spec.clone(),
                                flag_list(v).with_context(|| format!("target.{spec}.rustflags"))?,
                            )),
                            "rustdocflags" => {}
                            other => bail!("unsupported .cargo/config key target.{spec}.{other}"),
                        }
                    }
                }
            }
            "env" => {
                let entries = value.as_table().context("[env] is not a table")?;
                for (name, v) in entries {
                    let val = v.as_str().with_context(|| {
                        format!("env.{name}: only plain string values are supported")
                    })?;
                    let managed = matches!(
                        name.as_str(),
                        "TMPDIR" | "PATH" | "HOME" | "OUT_DIR" | "SDKROOT" | "RUSTFLAGS"
                    ) || name.starts_with("CARGO_")
                        || name.starts_with("DCARGO_");
                    if managed {
                        bail!("env.{name} collides with a tool-managed variable");
                    }
                    env.push((name.clone(), val.to_string()));
                }
            }
            // Command aliases and network/UI preferences never change what
            // gets compiled.
            "alias" | "net" | "http" | "term" | "registries" | "registry" | "cargo-new"
            | "future-incompat-report" | "cache" | "install" | "doc" => {}
            other => bail!("unsupported .cargo/config section [{other}]"),
        }
    }
    env.sort();
    Ok(CargoConfig { build_rustflags, target_rustflags, env })
}

/// Cargo accepts rustflags as an array of strings or one space-separated
/// string.
fn flag_list(value: &toml::Value) -> Result<Vec<String>> {
    match value {
        toml::Value::String(s) => Ok(s.split_whitespace().map(str::to_string).collect()),
        toml::Value::Array(items) => items
            .iter()
            .map(|i| {
                i.as_str().map(str::to_string).context("rustflags entries must be strings")
            })
            .collect(),
        _ => bail!("rustflags must be a string or an array of strings"),
    }
}

/// The target-triple facts simple `cfg()` predicates can ask about.
struct TripleInfo {
    arch: String,
    vendor: String,
    os: String,
    env: String,
    family: &'static str,
}

impl TripleInfo {
    fn of(triple: &str) -> Result<TripleInfo> {
        let parts: Vec<&str> = triple.split('-').collect();
        let arch = parts[0].to_string();
        let (vendor, os, env, family): (&str, &str, &str, &str) = match &parts[1..] {
            ["apple", "darwin"] => ("apple", "macos", "", "unix"),
            ["unknown", "linux", environment] => ("unknown", "linux", *environment, "unix"),
            ["unknown", "linux"] => ("unknown", "linux", "", "unix"),
            ["pc", "windows", environment] => ("pc", "windows", *environment, "windows"),
            ["unknown", "unknown"] => ("unknown", "unknown", "", ""),
            ["wasip1"] => ("unknown", "wasi", "p1", "wasm"),
            ["wasip2"] => ("unknown", "wasi", "p2", "wasm"),
            ["wasi"] => ("unknown", "wasi", "", "wasm"),
            _ => bail!("unrecognized target triple {triple}"),
        };
        Ok(TripleInfo {
            arch,
            vendor: vendor.to_string(),
            os: os.to_string(),
            env: env.to_string(),
            family,
        })
    }
}

enum CfgExpr {
    All(Vec<CfgExpr>),
    Any(Vec<CfgExpr>),
    Not(Box<CfgExpr>),
    Name(String),
    KeyValue(String, String),
}

fn parse_cfg(spec: &str) -> Result<CfgExpr> {
    let inner = spec
        .strip_prefix("cfg(")
        .and_then(|s| s.strip_suffix(')'))
        .with_context(|| format!("malformed cfg spec {spec}"))?;
    let tokens = tokenize(inner).with_context(|| format!("in {spec}"))?;
    let mut pos = 0;
    let expr = parse_expr(&tokens, &mut pos).with_context(|| format!("in {spec}"))?;
    if pos != tokens.len() {
        bail!("trailing tokens in {spec}");
    }
    Ok(expr)
}

#[derive(PartialEq, Debug)]
enum Token {
    Ident(String),
    Str(String),
    LParen,
    RParen,
    Comma,
    Eq,
}

fn tokenize(text: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            ',' => {
                chars.next();
                tokens.push(Token::Comma);
            }
            '=' => {
                chars.next();
                tokens.push(Token::Eq);
            }
            '"' => {
                chars.next();
                let mut value = String::new();
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => bail!("escape sequences in cfg strings are not supported"),
                        Some(ch) => value.push(ch),
                        None => bail!("unterminated string"),
                    }
                }
                tokens.push(Token::Str(value));
            }
            ch if ch.is_ascii_alphanumeric() || ch == '_' => {
                let mut ident = String::new();
                while let Some(&ch) = chars.peek() {
                    if ch.is_ascii_alphanumeric() || ch == '_' {
                        ident.push(ch);
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Ident(ident));
            }
            other => bail!("unexpected character {other:?} in cfg expression"),
        }
    }
    Ok(tokens)
}

fn parse_expr(tokens: &[Token], pos: &mut usize) -> Result<CfgExpr> {
    let Some(Token::Ident(name)) = tokens.get(*pos) else {
        bail!("expected identifier");
    };
    *pos += 1;
    match tokens.get(*pos) {
        Some(Token::LParen) => {
            *pos += 1;
            let mut items = Vec::new();
            if tokens.get(*pos) != Some(&Token::RParen) {
                loop {
                    items.push(parse_expr(tokens, pos)?);
                    match tokens.get(*pos) {
                        Some(Token::Comma) => *pos += 1,
                        _ => break,
                    }
                }
            }
            if tokens.get(*pos) != Some(&Token::RParen) {
                bail!("expected closing parenthesis");
            }
            *pos += 1;
            match name.as_str() {
                "all" => Ok(CfgExpr::All(items)),
                "any" => Ok(CfgExpr::Any(items)),
                "not" => {
                    if items.len() != 1 {
                        bail!("not() takes exactly one predicate");
                    }
                    Ok(CfgExpr::Not(Box::new(items.into_iter().next().unwrap())))
                }
                other => bail!("unsupported cfg operator {other}"),
            }
        }
        Some(Token::Eq) => {
            *pos += 1;
            let Some(Token::Str(value)) = tokens.get(*pos) else {
                bail!("expected string after =");
            };
            *pos += 1;
            Ok(CfgExpr::KeyValue(name.clone(), value.clone()))
        }
        _ => Ok(CfgExpr::Name(name.clone())),
    }
}

/// Reject predicates dcargo cannot evaluate, up front and regardless of
/// short-circuiting, so unsupported configs fail on load.
fn validate_cfg(expr: &CfgExpr) -> Result<()> {
    match expr {
        CfgExpr::All(items) | CfgExpr::Any(items) => items.iter().try_for_each(validate_cfg),
        CfgExpr::Not(inner) => validate_cfg(inner),
        CfgExpr::Name(name) => match name.as_str() {
            "unix" | "windows" => Ok(()),
            other => bail!("unsupported cfg predicate {other}"),
        },
        CfgExpr::KeyValue(key, _) => match key.as_str() {
            "target_os" | "target_arch" | "target_env" | "target_vendor" | "target_family" => {
                Ok(())
            }
            other => bail!("unsupported cfg predicate {other}"),
        },
    }
}

fn eval_cfg(expr: &CfgExpr, info: &TripleInfo) -> Result<bool> {
    Ok(match expr {
        CfgExpr::All(items) => {
            for item in items {
                if !eval_cfg(item, info)? {
                    return Ok(false);
                }
            }
            true
        }
        CfgExpr::Any(items) => {
            for item in items {
                if eval_cfg(item, info)? {
                    return Ok(true);
                }
            }
            false
        }
        CfgExpr::Not(inner) => !eval_cfg(inner, info)?,
        CfgExpr::Name(name) => match name.as_str() {
            "unix" => info.family == "unix",
            "windows" => info.family == "windows",
            other => bail!("unsupported cfg predicate {other}"),
        },
        CfgExpr::KeyValue(key, value) => match key.as_str() {
            "target_os" => info.os == *value,
            "target_arch" => info.arch == *value,
            "target_env" => info.env == *value,
            "target_vendor" => info.vendor == *value,
            "target_family" => info.family == *value,
            other => bail!("unsupported cfg predicate {other}"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_rustflags_used_when_no_target_entry_matches() {
        let config = parse(
            r#"
            [build]
            rustflags = ["-C", "symbol-mangling-version=v0", "--cfg", "tokio_unstable"]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.rustflags_for("aarch64-apple-darwin").unwrap(),
            vec!["-C", "symbol-mangling-version=v0", "--cfg", "tokio_unstable"]
        );
    }

    #[test]
    fn matching_target_entries_replace_build_rustflags() {
        // Zed's real config shape: a global mangling/cfg pair, a windows
        // cfg section, and a literal linux triple section.
        let config = parse(
            r#"
            [build]
            rustflags = ["-C", "symbol-mangling-version=v0", "--cfg", "tokio_unstable"]

            [target.'cfg(target_os = "windows")']
            rustflags = ["--cfg", "windows_slim_errors", "-C", "target-feature=+crt-static"]

            [target.aarch64-unknown-linux-gnu]
            rustflags = ["-C", "link-arg=-fuse-ld=lld"]
            "#,
        )
        .unwrap();
        // No target entry matches on macOS: the build flags apply.
        assert_eq!(
            config.rustflags_for("aarch64-apple-darwin").unwrap(),
            vec!["-C", "symbol-mangling-version=v0", "--cfg", "tokio_unstable"]
        );
        // A matching cfg entry replaces the build flags entirely.
        assert_eq!(
            config.rustflags_for("x86_64-pc-windows-msvc").unwrap(),
            vec!["--cfg", "windows_slim_errors", "-C", "target-feature=+crt-static"]
        );
        // A matching literal triple does the same.
        assert_eq!(
            config.rustflags_for("aarch64-unknown-linux-gnu").unwrap(),
            vec!["-C", "link-arg=-fuse-ld=lld"]
        );
    }

    #[test]
    fn triple_and_cfg_matches_concatenate_in_pinned_order() {
        let config = parse(
            r#"
            [build]
            rustflags = ["--cfg", "never_applies"]

            [target.'cfg(unix)']
            rustflags = ["--cfg", "from_cfg"]

            [target.aarch64-apple-darwin]
            rustflags = ["--cfg", "from_triple"]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.rustflags_for("aarch64-apple-darwin").unwrap(),
            vec!["--cfg", "from_triple", "--cfg", "from_cfg"]
        );
    }

    #[test]
    fn cfg_operators_and_string_form_flags() {
        let config = parse(
            r#"
            [target.'cfg(all(unix, not(target_os = "macos")))']
            rustflags = "-C link-arg=-fuse-ld=lld"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.rustflags_for("x86_64-unknown-linux-gnu").unwrap(),
            vec!["-C", "link-arg=-fuse-ld=lld"]
        );
        assert!(config.rustflags_for("aarch64-apple-darwin").unwrap().is_empty());
    }

    #[test]
    fn env_entries_are_parsed_and_sorted() {
        let config = parse(
            r#"
            [env]
            ZED_B = "two"
            MACOSX_DEPLOYMENT_TARGET = "10.15.7"
            "#,
        )
        .unwrap();
        assert_eq!(
            config.env,
            vec![
                ("MACOSX_DEPLOYMENT_TARGET".to_string(), "10.15.7".to_string()),
                ("ZED_B".to_string(), "two".to_string()),
            ]
        );
    }

    #[test]
    fn semantics_bearing_config_is_a_hard_error() {
        // Alternate compilers, linkers, profiles, forced env shapes, and
        // predicates dcargo cannot evaluate must refuse to build rather
        // than silently diverge from cargo.
        for bad in [
            "[build]\nrustc = \"my-rustc\"",
            "[target.aarch64-apple-darwin]\nlinker = \"lld\"",
            "[profile.dev]\nopt-level = 3",
            "[env]\nFOO = { value = \"x\", force = true }",
            "[env]\nCARGO_TERM_COLOR = \"always\"",
            "[target.'cfg(feature = \"x\")']\nrustflags = [\"--cfg\", \"y\"]",
        ] {
            assert!(parse(bad).is_err(), "expected hard error for: {bad}");
        }
        // Aliases and network settings never change a build: ignored.
        parse("[alias]\nxtask = \"run --package xtask --\"\n[net]\ngit-fetch-with-cli = true").unwrap();
    }
}
