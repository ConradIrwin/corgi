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
    time::{Duration, Instant},
};

pub const NAMESPACE: &str = "corgi/v1";
pub const GET_CONCURRENCY: usize = 4;
const MAX_RECORD: u64 = 4 * 1024 * 1024;
const MAX_BLOB: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    pub read_url: String,
}

#[derive(Clone)]
pub struct Cache {
    config: Config,
    read_timeout: Duration,
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

fn check_cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ReadCancelled.into());
    }
    Ok(())
}

impl Cache {
    pub fn new(config: Config) -> Result<Self> {
        validate_base_url(&config.read_url)?;
        Ok(Self {
            config,
            read_timeout: Duration::from_secs(10),
        })
    }

    /// Returns None for HTTP 404; other failures retain their diagnostic cause.
    /// Callers treat errors as cache misses and validate the record's schema.
    pub fn fetch_record(&self, key: &str, cancel: &AtomicBool) -> Result<Option<Vec<u8>>> {
        validate_hash(key)?;
        let tmp = Temporary::new(&std::env::temp_dir())?;
        let status = self.request(
            "GET",
            &self.read_url(&format!("actions/{key}.json")),
            &tmp.0,
            None,
            None,
            MAX_RECORD,
            cancel,
            self.read_timeout,
            false,
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
            None,
            MAX_BLOB,
            cancel,
            self.read_timeout,
            false,
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
        self.publish_with_config(key, record, blobs, &config)
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
                self.read_timeout,
                false,
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
                Duration::from_secs(30),
                true,
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
                self.read_timeout,
                false,
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
        let status = self.request(
            "PUT",
            url,
            output,
            Some(input),
            Some(credentials),
            MAX_RECORD,
            cancel,
            Duration::from_secs(30),
            false,
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
        timeout: Duration,
        create_only: bool,
    ) -> Result<u16> {
        check_cancelled(cancel)?;
        // -q must be first: never inherit ~/.curlrc credentials or redirects.
        // Do not follow redirects; public reads must stay unauthenticated and
        // signed writes must never send credentials to another endpoint.
        let mut command = crate::curl();
        command
            .args([
                "-q",
                "--silent",
                "--globoff",
                "--proto",
                "=http,https",
                "--connect-timeout",
                "3",
                "--max-time",
                &timeout.as_secs_f64().to_string(),
                "--max-filesize",
                &limit.to_string(),
                "--output",
            ])
            .arg(output)
            .args(["--write-out", "%{http_code}", "--url", url]);
        if create_only {
            command.args(["--header", "If-None-Match: *"]);
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
        let start = Instant::now();
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
                ensure!(
                    start.elapsed() <= timeout,
                    "remote request deadline exceeded"
                );
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
        ensure!(
            response.status.success(),
            "R2 curl transport failed ({})",
            response.status
        );
        Ok(std::str::from_utf8(&response.stdout)?.parse()?)
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

struct Credentials {
    access: String,
    secret: String,
}

impl Credentials {
    fn from_env() -> Result<Self> {
        let access =
            std::env::var("CORGI_R2_ACCESS_KEY_ID").context("missing CORGI_R2_ACCESS_KEY_ID")?;
        let secret = std::env::var("CORGI_R2_SECRET_ACCESS_KEY")
            .context("missing CORGI_R2_SECRET_ACCESS_KEY")?;
        ensure!(
            !access.is_empty() && !secret.is_empty() && access.len() + secret.len() < 4096,
            "invalid R2 credentials"
        );
        Ok(Self { access, secret })
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
