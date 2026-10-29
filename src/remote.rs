//! Optional R2 transport. Object expiry (30 days) is a bucket lifecycle policy:
//! this module neither touches objects nor performs remote garbage collection.
use crate::store::{sha256_file, Store};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::Duration,
};

pub const NAMESPACE: &str = "corgi/v2";
pub const GET_CONCURRENCY: usize = 4;
const MAX_RECORD: u64 = 4 * 1024 * 1024;
const MAX_BLOB: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// Public URL, or an HTTPS R2 S3 bucket URL matched in ~/.config/corgi/credentials.
    pub read_url: String,
    #[serde(default)]
    pub macos_cache_path: Option<PathBuf>,
    #[serde(default)]
    pub op_access_key_id: Option<String>,
    #[serde(default)]
    pub op_secret_access_key: Option<String>,
}

pub fn configured_cache(dir: &Path) -> Result<Option<Config>> {
    #[derive(Deserialize)]
    struct CacheOnly {
        cache: Option<Config>,
    }

    let Some(path) = crate::build::find_corgi_toml(dir) else {
        return Ok(None);
    };
    let source =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed: CacheOnly =
        toml::from_str(&source).with_context(|| format!("parsing {}", path.display()))?;
    Ok(parsed.cache)
}

#[derive(Clone)]
pub struct Cache {
    config: Config,
    read_access: ReadAccess,
}

pub struct BlobRef<'a> {
    pub hash: &'a str,
    pub path: &'a Path,
}

#[derive(Debug)]
pub struct ReadCancelled;

impl std::fmt::Display for ReadCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("remote request cancelled")
    }
}

impl std::error::Error for ReadCancelled {}

#[derive(Debug)]
struct TransientRequestFailure;

impl std::fmt::Display for TransientRequestFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("temporary R2 request failure")
    }
}

impl std::error::Error for TransientRequestFailure {}

impl Config {
    pub fn ready_macos_cache_path(&self) -> Result<Option<PathBuf>> {
        if !cfg!(target_os = "macos") {
            return Ok(None);
        }
        let Some(path) = &self.macos_cache_path else {
            return Ok(None);
        };
        ensure!(path.is_absolute(), "macos-cache-path must be absolute");
        if !path.is_dir() || fs::canonicalize(path)? != *path {
            return Ok(None);
        }
        validate_base_url(&self.read_url)?;
        if matches!(ReadAccess::for_url(&self.read_url)?, ReadAccess::Disabled) {
            return Ok(None);
        }
        Ok(Some(path.clone()))
    }
}

fn check_cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ReadCancelled.into());
    }
    Ok(())
}

impl Cache {
    pub fn new(config: Config) -> Result<Self> {
        validate_base_url(&config.read_url)?;
        let read_access = ReadAccess::for_url(&config.read_url)?;
        if matches!(read_access, ReadAccess::Disabled) {
            eprintln!("Warning no credentials for remote cache configured");
        }
        Ok(Self {
            config,
            read_access,
        })
    }

    pub fn can_read(&self) -> bool {
        !matches!(self.read_access, ReadAccess::Disabled)
    }

    fn read_credentials(&self) -> Option<&Credentials> {
        match &self.read_access {
            ReadAccess::Authenticated(credentials) => Some(credentials),
            ReadAccess::Public | ReadAccess::Disabled => None,
        }
    }

    /// Returns None for HTTP 404; other failures retain their diagnostic cause.
    /// Callers treat errors as cache misses and validate the record's schema.
    pub fn fetch_record(&self, key: &str, cancel: &AtomicBool) -> Result<Option<Vec<u8>>> {
        ensure!(self.can_read(), "remote cache reads disabled");
        validate_hash(key)?;
        let tmp = Temporary::new(&std::env::temp_dir())?;
        let status = self.request(
            "GET",
            &self.read_url(&format!("actions/{key}.json")),
            &tmp.0,
            None,
            self.read_credentials(),
            MAX_RECORD,
            cancel,
            &[],
        )?;
        if status == 404 {
            return Ok(None);
        }
        ensure!(status == 200, "R2 record returned HTTP {status}");
        let bytes = fs::read(&tmp.0)?;
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .context("invalid remote record JSON")?;
        Ok(Some(bytes))
    }

    /// Verify a blob, downloading and atomically installing it only if needed.
    pub fn fetch_blob(&self, store: &Store, hash: &str, cancel: &AtomicBool) -> Result<()> {
        ensure!(self.can_read(), "remote cache reads disabled");
        validate_hash(hash)?;
        check_cancelled(cancel)?;
        let destination = store.cache_path(hash);
        if sha256_file(&destination).ok().as_deref() == Some(hash) {
            return Ok(());
        }
        let temporary = Temporary::new(&store.root.join("tmp"))?;
        let status = self.request(
            "GET",
            &self.read_url(&format!("blobs/{hash}")),
            &temporary.0,
            None,
            self.read_credentials(),
            MAX_BLOB,
            cancel,
            &[],
        )?;
        ensure!(status == 200, "remote blob {hash} returned HTTP {status}");
        check_cancelled(cancel)?;
        ensure!(
            sha256_file(&temporary.0)?.as_str() == hash,
            "remote blob {hash} failed SHA-256 verification"
        );
        check_cancelled(cancel)?;
        fs::create_dir_all(destination.parent().unwrap())?;
        fs::rename(&temporary.0, destination)
            .context("publishing verified remote blob into CAS")?;
        Ok(())
    }

    /// Validate explicit write opt-in before the caller schedules any work.
    pub fn validate_write_config(&self) -> Result<()> {
        WriteConfig::from_env()?;
        Ok(())
    }

    /// Records have a top-level `blobs: Vec<String>` field. Complete existing
    /// records are left unchanged, even when their bytes differ. Missing old
    /// blobs are repaired from supplied refs; an unavailable old blob is an
    /// error, not permission to replace the record.
    pub fn publish(&self, key: &str, record: &[u8], blobs: &[BlobRef<'_>]) -> Result<()> {
        validate_hash(key)?;
        ensure!(
            record.len() as u64 <= MAX_RECORD,
            "remote record exceeds size limit"
        );
        record_hashes(record)?;
        let config = WriteConfig::from_env()?;
        for attempt in 0..3 {
            match self.publish_with_config(key, record, blobs, &config) {
                Err(error) if attempt < 2 && error.is::<TransientRequestFailure>() => {
                    eprintln!("corgi warning: retrying cache publication: {error:#}");
                    thread::sleep(Duration::from_millis(250 * (attempt + 1)));
                }
                result => return result,
            }
        }
        unreachable!()
    }

    fn publish_with_config(
        &self,
        key: &str,
        record: &[u8],
        blobs: &[BlobRef<'_>],
        config: &WriteConfig,
    ) -> Result<()> {
        let credentials = &config.credentials;
        let cancel = AtomicBool::new(false);
        let output = Temporary::new(&std::env::temp_dir())?;
        let action = config.object_url(&format!("actions/{key}.json"));
        for blob in blobs {
            validate_hash(blob.hash)?;
        }
        // A lost conditional write restarts inspection, including repair of
        // the winner's references. Bound contention rather than waiting forever.
        for _ in 0..3 {
            let status = self.request(
                "GET",
                &action,
                &output.0,
                None,
                Some(credentials),
                MAX_RECORD,
                &cancel,
                &[],
            )?;
            ensure!(
                status == 200 || status == 404,
                "R2 record inspection returned HTTP {status}"
            );
            if status == 200 {
                let hashes = record_hashes(&fs::read(&output.0)?).with_context(|| {
                    format!(
                        "invalid remote record actions/{key}.json; repair it or wait for expiry"
                    )
                })?;
                self.repair_blobs(&hashes, blobs, &output.0, config, &cancel)?;
                return Ok(());
            }
            self.repair_blobs(&record_hashes(record)?, blobs, &output.0, config, &cancel)?;
            let input = Temporary::new(&std::env::temp_dir())?;
            fs::write(&input.0, record)?;
            let status = self.request(
                "PUT",
                &action,
                &output.0,
                Some(&input.0),
                Some(credentials),
                MAX_RECORD,
                &cancel,
                &[("If-None-Match", "*")],
            )?;
            if status == 412 {
                continue;
            }
            ensure!(
                (200..300).contains(&status),
                "R2 record upload returned HTTP {status}"
            );
            return Ok(());
        }
        bail!("R2 record publication exceeded conditional-write retry limit")
    }

    fn repair_blobs(
        &self,
        hashes: &[String],
        blobs: &[BlobRef<'_>],
        output: &Path,
        config: &WriteConfig,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let credentials = &config.credentials;
        let mut missing = Vec::new();
        for hash in hashes {
            let url = config.object_url(&format!("blobs/{hash}"));
            let status = self.request(
                "HEAD",
                &url,
                output,
                None,
                Some(credentials),
                MAX_BLOB,
                cancel,
                &[],
            )?;
            match status {
                200 => {}
                404 => {
                    let blob = blobs
                        .iter()
                        .find(|blob| blob.hash == hash)
                        .context("missing remote blob is not available in supplied refs")?;
                    ensure!(
                        fs::metadata(blob.path)?.len() <= MAX_BLOB,
                        "remote blob exceeds size limit"
                    );
                    ensure!(
                        sha256_file(blob.path)? == blob.hash,
                        "local blob hash mismatch"
                    );
                    missing.push((url, blob.path));
                }
                _ => bail!("R2 blob inspection returned HTTP {status}"),
            }
        }
        for (url, path) in missing {
            self.put(&url, path, output, credentials, cancel)?;
        }
        Ok(())
    }

    fn put(
        &self,
        url: &str,
        input: &Path,
        output: &Path,
        credentials: &Credentials,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let compressed = Temporary::new(&std::env::temp_dir())?;
        let status = std::process::Command::new("gzip")
            .args(["-n", "-c", "--"])
            .arg(input)
            .stdout(fs::File::create(&compressed.0)?)
            .stderr(Stdio::null())
            .status()
            .context("compressing remote blob with gzip")?;
        ensure!(status.success(), "gzip compression failed ({status})");
        let status = self.request(
            "PUT",
            url,
            output,
            Some(&compressed.0),
            Some(credentials),
            MAX_RECORD,
            cancel,
            &[
                ("Content-Type", "application/octet-stream"),
                ("Content-Encoding", "gzip"),
            ],
        )?;
        ensure!(
            (200..300).contains(&status),
            "R2 upload returned HTTP {status}"
        );
        Ok(())
    }

    fn read_url(&self, object: &str) -> String {
        format!(
            "{}/{NAMESPACE}/{object}",
            self.config.read_url.trim_end_matches('/')
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn request(
        &self,
        method: &str,
        url: &str,
        output: &Path,
        input: Option<&Path>,
        credentials: Option<&Credentials>,
        limit: u64,
        cancel: &AtomicBool,
        headers: &[(&str, &str)],
    ) -> Result<u16> {
        check_cancelled(cancel)?;
        // -q must be first: never inherit ~/.curlrc credentials or redirects.
        // Do not follow redirects; public reads must stay unauthenticated and
        // signed requests must never send credentials to another endpoint.
        let mut command = crate::curl();
        command
            .args([
                "-q",
                "--silent",
                "--globoff",
                "--proto",
                "=http,https",
                "--connect-timeout",
                "5",
                "--speed-limit",
                "1024",
                "--speed-time",
                "30",
                "--max-filesize",
                &limit.to_string(),
                "--output",
            ])
            .arg(output)
            .args(["--write-out", "%{http_code}", "--url", url]);
        for (name, value) in headers {
            command.args(["--header", &format!("{name}: {value}")]);
        }
        if method == "GET" {
            command.arg("--compressed");
        }
        if method == "HEAD" {
            command.arg("--head");
        }
        if let Some(input) = input {
            command.arg("--upload-file").arg(input);
        }
        if credentials.is_some() {
            command.args(["--aws-sigv4", "aws:amz:auto:s3", "--config", "-"]);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().context("starting R2 curl transport")?;
        let result = (|| {
            if let Some(credentials) = credentials {
                // Secrets only enter curl through stdin, never argv or errors.
                child
                    .stdin
                    .as_mut()
                    .unwrap()
                    .write_all(&credentials.config())?;
            }
            drop(child.stdin.take());
            loop {
                check_cancelled(cancel)?;
                if fs::metadata(output).is_ok_and(|m| m.len() > limit) {
                    bail!("remote request exceeded response size limits");
                }
                if child.try_wait()?.is_some() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return result.map(|_| 0);
        }
        let response = child.wait_with_output()?;
        if matches!(
            response.status.code(),
            Some(5 | 6 | 7 | 18 | 28 | 52 | 55 | 56 | 92)
        ) {
            return Err(anyhow::Error::new(TransientRequestFailure)).with_context(|| {
                format!(
                    "{method} {url}: R2 curl transport failed ({})",
                    response.status
                )
            });
        }
        ensure!(
            response.status.success(),
            "{method} {url}: R2 curl transport failed ({})",
            response.status
        );
        // Content-Length describes compressed bytes; bound the decoded file too,
        // including writes between the last polling check and curl's exit.
        ensure!(
            fs::metadata(output)?.len() <= limit,
            "remote request exceeded response size limits"
        );
        let status = std::str::from_utf8(&response.stdout)?.parse()?;
        if status == 429 || (500..600).contains(&status) {
            return Err(anyhow::Error::new(TransientRequestFailure))
                .with_context(|| format!("{method} {url}: HTTP {status}"));
        }
        Ok(status)
    }
}

fn validate_hash(hash: &str) -> Result<()> {
    ensure!(
        hash.len() == 64
            && hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "remote keys and hashes must be lowercase SHA-256 hex"
    );
    Ok(())
}

fn record_hashes(bytes: &[u8]) -> Result<Vec<String>> {
    #[derive(Deserialize)]
    struct References {
        blobs: Vec<String>,
    }
    let record: References =
        serde_json::from_slice(bytes).context("invalid remote record envelope")?;
    for hash in &record.blobs {
        validate_hash(hash)?;
    }
    Ok(record.blobs)
}

fn validate_base_url(url: &str) -> Result<()> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));
    ensure!(
        rest.is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'))
            && !url.contains(['?', '#', '@', '\r', '\n'])
            && !url.chars().any(char::is_whitespace),
        "R2 URLs must be HTTP(S) base URLs without credentials, query or fragment"
    );
    Ok(())
}

fn validate_write_url(url: &str) -> Result<()> {
    validate_base_url(url)?;
    let (_, address) = url.split_once("://").context("missing URL scheme")?;
    let (_, bucket) = address
        .trim_end_matches('/')
        .split_once('/')
        .context("expected an R2 account URL followed by /<bucket>")?;
    ensure!(
        !bucket.is_empty()
            && bucket
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        "expected a single R2 bucket name, without an object path"
    );
    Ok(())
}

/// Publisher-only settings; ordinary cache reads never inspect this environment.
struct WriteConfig {
    url: String,
    credentials: Credentials,
}

impl WriteConfig {
    fn from_env() -> Result<Self> {
        let url = std::env::var("CORGI_R2_WRITE_URL").context("missing CORGI_R2_WRITE_URL")?;
        validate_write_url(&url).context("invalid CORGI_R2_WRITE_URL")?;
        Ok(Self {
            url,
            credentials: Credentials::from_env()?,
        })
    }

    fn object_url(&self, object: &str) -> String {
        format!("{}/{NAMESPACE}/{object}", self.url.trim_end_matches('/'))
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    #[serde(rename = "access-key-id")]
    access: String,
    #[serde(rename = "secret-access-key")]
    secret: String,
}

#[derive(Clone)]
enum ReadAccess {
    Public,
    Authenticated(Credentials),
    Disabled,
}

impl ReadAccess {
    /// Read credentials only for R2's S3 endpoints, never arbitrary public URLs.
    fn for_url(url: &str) -> Result<Self> {
        let (_, address) = url.split_once("://").context("missing URL scheme")?;
        let host = address.split('/').next().unwrap_or_default();
        if !host.ends_with(".r2.cloudflarestorage.com") {
            return Ok(Self::Public);
        }
        ensure!(
            url.starts_with("https://"),
            "authenticated R2 reads require HTTPS"
        );
        validate_write_url(url).context("invalid authenticated R2 read URL")?;
        let path = dirs::home_dir()
            .context("cannot locate home directory for R2 credentials")?
            .join(".config/corgi/credentials");
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::Disabled)
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("opening R2 credentials file {}", path.display()))
            }
        };
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() <= 64 * 1024,
            "R2 credentials must be a regular file of at most 64 KiB"
        );
        use std::io::Read;
        let mut contents = String::new();
        file.take(64 * 1024 + 1)
            .read_to_string(&mut contents)
            .with_context(|| format!("reading R2 credentials file {}", path.display()))?;
        ensure!(
            contents.len() <= 64 * 1024,
            "R2 credentials file exceeds 64 KiB"
        );
        // TOML errors can contain the offending source line, including secrets.
        let entries: std::collections::BTreeMap<String, Credentials> = toml::from_str(&contents)
            .map_err(|_| anyhow::anyhow!("invalid R2 credentials file {}; expected bucket URL tables with access-key-id and secret-access-key", path.display()))?;
        let mut matching = entries
            .into_iter()
            .filter(|(endpoint, _)| endpoint.trim_end_matches('/') == url.trim_end_matches('/'));
        let Some((_, credentials)) = matching.next() else {
            return Ok(Self::Disabled);
        };
        ensure!(
            matching.next().is_none(),
            "duplicate R2 credential entries for {url}"
        );
        credentials.validate()?;
        Ok(Self::Authenticated(credentials))
    }
}

impl Credentials {
    fn from_env() -> Result<Self> {
        let access =
            std::env::var("CORGI_R2_ACCESS_KEY_ID").context("missing CORGI_R2_ACCESS_KEY_ID")?;
        let secret = std::env::var("CORGI_R2_SECRET_ACCESS_KEY")
            .context("missing CORGI_R2_SECRET_ACCESS_KEY")?;
        let credentials = Self { access, secret };
        credentials.validate()?;
        Ok(credentials)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.access.is_empty()
                && !self.secret.is_empty()
                && self.access.len() + self.secret.len() < 4096,
            "invalid R2 credentials"
        );
        Ok(())
    }

    fn config(&self) -> Vec<u8> {
        [
            b"user = \"".as_slice(),
            &curl_config_escape(self.access.as_bytes()),
            b":",
            &curl_config_escape(self.secret.as_bytes()),
            b"\"\n",
        ]
        .concat()
    }
}

/// Set up an authenticated remote cache without changing a project file.
pub fn setup_cache(config: &Config) -> Result<()> {
    validate_base_url(&config.read_url)?;
    ensure!(
        config.read_url.starts_with("https://")
            && config.read_url.contains(".r2.cloudflarestorage.com/"),
        "cache setup requires a private HTTPS R2 read-url"
    );
    #[cfg(target_os = "macos")]
    {
        let path = config
            .macos_cache_path
            .as_ref()
            .context("cache setup requires macos-cache-path in corgi.toml")?;
        ensure!(path.is_absolute(), "macos-cache-path must be absolute");
        ensure_macos_cache_path(path)?;
    }
    let access = read_one_password_field(
        config
            .op_access_key_id
            .as_deref()
            .context("cache setup requires op-access-key-id in corgi.toml")?,
    )?;
    let secret = read_one_password_field(
        config
            .op_secret_access_key
            .as_deref()
            .context("cache setup requires op-secret-access-key in corgi.toml")?,
    )?;
    let credentials = Credentials { access, secret };
    credentials.validate()?;
    let home = dirs::home_dir().context("cannot locate home directory for R2 credentials")?;
    save_credentials(
        &home.join(".config/corgi/credentials"),
        &config.read_url,
        &credentials,
    )?;
    eprintln!("Configured remote cache credentials");
    Ok(())
}

fn read_one_password_field(reference: &str) -> Result<String> {
    ensure!(
        reference.starts_with("op://"),
        "1Password fields must use op:// secret references"
    );
    let output = std::process::Command::new("op")
        .args(["read", "--no-newline", reference])
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()
        .context("starting 1Password CLI; install `op` and sign in")?;
    ensure!(
        output.status.success(),
        "1Password could not read a cache credential; sign in to `op` and check access to the configured item"
    );
    String::from_utf8(output.stdout).context("1Password returned a non-UTF-8 cache credential")
}

fn save_credentials(path: &Path, url: &str, credentials: &Credentials) -> Result<()> {
    fs::create_dir_all(path.parent().context("credentials file has no parent")?)?;
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).context("reading existing Corgi credentials"),
    };
    let mut document: toml_edit::DocumentMut = source
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid existing Corgi credentials file"))?;
    let table = document
        .as_table_mut()
        .entry(url)
        .or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
    table["access-key-id"] = toml_edit::value(&credentials.access);
    table["secret-access-key"] = toml_edit::value(&credentials.secret);
    let temporary = Temporary::new(path.parent().unwrap())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary.0, fs::Permissions::from_mode(0o600))?;
    }
    fs::write(&temporary.0, document.to_string())?;
    fs::rename(&temporary.0, path).context("installing private Corgi credentials")
}

#[cfg(target_os = "macos")]
fn ensure_macos_cache_path(path: &Path) -> Result<()> {
    use std::io::IsTerminal;

    if let Err(error) = fs::create_dir_all(path) {
        if error.kind() != std::io::ErrorKind::PermissionDenied || !std::io::stdin().is_terminal() {
            return Err(error).with_context(|| format!("creating {}; run `corgi cache setup` in an interactive terminal to authorize sudo", path.display()));
        }
        eprintln!("Creating {} requires administrator access", path.display());
        let status = std::process::Command::new("sudo")
            .args(["mkdir", "-p", "--"])
            .arg(path)
            .status()
            .context("starting sudo to create the Corgi store")?;
        ensure!(status.success(), "sudo could not create {}", path.display());
        // SAFETY: getuid and getgid have no preconditions.
        let owner = unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) };
        let status = std::process::Command::new("sudo")
            .args(["chown", &owner, "--"])
            .arg(path)
            .status()
            .context("starting sudo to own the Corgi store")?;
        ensure!(
            status.success(),
            "sudo could not set ownership of {}",
            path.display()
        );
    }
    ensure!(
        fs::canonicalize(path)? == path,
        "macos-cache-path must be a real path, not a symlink: {}",
        path.display()
    );
    Temporary::new(path).with_context(|| format!("{} must be writable", path.display()))?;
    Ok(())
}

fn curl_config_escape(value: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(value.len());
    for &byte in value {
        match byte {
            b'\\' => escaped.extend_from_slice(b"\\\\"),
            b'"' => escaped.extend_from_slice(b"\\\""),
            b'\n' => escaped.extend_from_slice(b"\\n"),
            b'\r' => escaped.extend_from_slice(b"\\r"),
            b'\t' => escaped.extend_from_slice(b"\\t"),
            _ => escaped.push(byte),
        }
    }
    escaped
}

struct Temporary(PathBuf);

impl Temporary {
    fn new(dir: &Path) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = dir.join(format!(
                "corgi-r2-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    drop(file);
                    return Ok(Self(path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
