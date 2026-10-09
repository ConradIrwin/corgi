//! Optional R2 transport. Object expiry (30 days) is a bucket lifecycle policy:
//! this module neither touches objects nor performs remote garbage collection.
use crate::store::{sha256_file, Store};
use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use std::{
    collections::HashSet,
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
    pub endpoint: Option<String>,
    pub bucket: Option<String>,
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

impl Cache {
    pub fn new(config: Config) -> Result<Self> {
        for url in std::iter::once(&config.read_url).chain(config.endpoint.iter()) {
            ensure!(
                (url.starts_with("https://") || url.starts_with("http://"))
                    && !url.contains(['?', '#', '@', '\r', '\n'])
                    && !url.chars().any(char::is_whitespace),
                "R2 URLs must be HTTP(S) base URLs without credentials, query or fragment"
            );
        }
        ensure!(
            config
                .bucket
                .as_ref()
                .is_none_or(|bucket| !bucket.is_empty()
                    && bucket
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')),
            "invalid R2 bucket"
        );
        Ok(Self {
            config,
            read_timeout: Duration::from_secs(10),
        })
    }

    /// Transport errors, cancellation and malformed JSON are cache misses.
    /// The caller must validate the record's schema and referenced hashes.
    pub fn fetch_record(&self, key: &str, cancel: &AtomicBool) -> Result<Option<Vec<u8>>> {
        validate_hash(key)?;
        let result: Result<Option<Vec<u8>>> = (|| {
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
            )?;
            if status != 200 {
                return Ok(None);
            }
            let bytes = fs::read(&tmp.0)?;
            if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
                return Ok(None);
            }
            Ok(Some(bytes))
        })();
        Ok(result.unwrap_or(None))
    }

    /// Returns true only when every requested blob is locally available and verified.
    ///
    /// Transport errors and cancellation are misses; invalid hashes are errors.
    /// Only verified bytes are atomically renamed into the local CAS.
    /// Existing local bytes are checked too, so a damaged CAS entry is repaired.
    pub fn fetch_blobs(
        &self,
        store: &Store,
        hashes: &[String],
        cancel: &AtomicBool,
    ) -> Result<bool> {
        for hash in hashes {
            validate_hash(hash)?;
        }
        let result = (|| -> Result<bool> {
            let mut seen = HashSet::new();
            let mut missing = Vec::new();
            for hash in hashes {
                ensure!(!cancel.load(Ordering::Relaxed), "remote request cancelled");
                if seen.insert(hash)
                    && sha256_file(&store.cache_path(hash)).ok().as_deref() != Some(hash)
                {
                    missing.push((hash, Temporary::new(&store.root.join("tmp"))?));
                }
            }
            if missing.is_empty() {
                return Ok(!cancel.load(Ordering::Relaxed));
            }

            // File-backed metadata cannot fill a pipe while we wait for curl.
            // Each transfer emits one indexed status, in completion order.
            let statuses = Temporary::new(&store.root.join("tmp"))?;
            let status_limit = (missing.len() * (missing.len().to_string().len() + 9)) as u64;
            // Large incremental actions can exceed ARG_MAX, especially with
            // long store paths. Keep per-transfer options out of argv.
            let config = Temporary::new(&store.root.join("tmp"))?;
            let mut config_file = OpenOptions::new().write(true).open(&config.0)?;
            let mut command = crate::curl();
            command.args([
                "-q",
                "--parallel",
                "--parallel-max",
                &GET_CONCURRENCY.to_string(),
            ]);
            command.arg("--config").arg(&config.0);
            for (index, (hash, temporary)) in missing.iter().enumerate() {
                if index > 0 {
                    writeln!(config_file, "next")?;
                }
                writeln!(
                    config_file,
                    "silent\ngloboff\nproto = \"=http,https\"\nconnect-timeout = 3"
                )?;
                for (option, value) in [
                    ("max-time", self.read_timeout.as_secs_f64().to_string()),
                    ("max-filesize", MAX_BLOB.to_string()),
                    (
                        "write-out",
                        format!("{index} %{{http_code}} %{{exitcode}}\n"),
                    ),
                    ("url", self.read_url(&format!("blobs/{hash}"))),
                ] {
                    write!(config_file, "{option} = \"")?;
                    config_file.write_all(&curl_config_escape(value.as_bytes()))?;
                    writeln!(config_file, "\"")?;
                }
                write!(config_file, "output = \"")?;
                config_file.write_all(&curl_config_escape(
                    temporary.0.as_os_str().as_encoded_bytes(),
                ))?;
                writeln!(config_file, "\"")?;
            }
            drop(config_file);
            command
                .stdin(Stdio::null())
                .stdout(OpenOptions::new().write(true).open(&statuses.0)?)
                .stderr(Stdio::null());
            let mut child = command
                .spawn()
                .context("starting parallel R2 curl transport")?;
            let start = Instant::now();
            // --max-time applies to individual transfers, not time in the queue.
            let waves = missing.len().div_ceil(GET_CONCURRENCY);
            let deadline = self
                .read_timeout
                .saturating_mul(u32::try_from(waves).unwrap_or(u32::MAX))
                .saturating_add(Duration::from_secs(1));
            let wait = (|| -> Result<()> {
                loop {
                    ensure!(
                        !cancel.load(Ordering::Relaxed)
                            && start.elapsed() <= deadline
                            && fs::metadata(&statuses.0)?.len() <= status_limit
                            && missing.iter().all(|(_, temporary)| {
                                fs::metadata(&temporary.0).is_ok_and(|m| m.len() <= MAX_BLOB)
                            }),
                        "remote batch cancelled or exceeded its bounds"
                    );
                    if let Some(status) = child.try_wait()? {
                        ensure!(status.success(), "parallel R2 curl transport failed");
                        return Ok(());
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            })();
            if let Err(error) = wait {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            ensure!(
                fs::metadata(&statuses.0)?.len() <= status_limit,
                "oversized curl metadata"
            );
            let mut completed = HashSet::new();
            for line in fs::read_to_string(&statuses.0)?.lines() {
                let (index, status) = line.split_once(' ').context("invalid curl status")?;
                let index: usize = index.parse()?;
                ensure!(
                    status == "200 0" && index < missing.len() && completed.insert(index),
                    "incomplete remote batch"
                );
            }
            ensure!(completed.len() == missing.len(), "incomplete remote batch");
            for (hash, temporary) in &missing {
                ensure!(
                    !cancel.load(Ordering::Relaxed)
                        && fs::metadata(&temporary.0)?.len() <= MAX_BLOB
                        && sha256_file(&temporary.0)?.as_str() == hash.as_str()
                        && !cancel.load(Ordering::Relaxed),
                    "unverified remote blob"
                );
                let destination = store.cache_path(hash);
                fs::create_dir_all(destination.parent().unwrap())?;
                fs::rename(&temporary.0, destination)
                    .context("publishing verified remote blob into CAS")?;
            }
            Ok(!cancel.load(Ordering::Relaxed))
        })();
        Ok(result.unwrap_or(false))
    }

    /// Validate explicit write opt-in before the caller schedules any work.
    pub fn validate_write_config(&self) -> Result<()> {
        ensure!(self.config.endpoint.is_some(), "R2 writes require endpoint");
        ensure!(self.config.bucket.is_some(), "R2 writes require bucket");
        Credentials::from_env()?;
        Ok(())
    }

    /// Records have a top-level `blobs: Vec<String>` field. Complete existing
    /// records are left unchanged, even when their bytes differ. Missing old
    /// blobs are repaired from supplied refs; an unavailable old blob is an
    /// error, not permission to replace the record.
    pub fn publish(&self, key: &str, record: &[u8], blobs: &[BlobRef<'_>]) -> Result<()> {
        validate_hash(key)?;
        self.validate_write_config()?;
        ensure!(
            record.len() as u64 <= MAX_RECORD,
            "remote record exceeds size limit"
        );
        record_hashes(record)?;
        let credentials = Credentials::from_env()?;
        self.publish_with_credentials(key, record, blobs, &credentials)
    }

    fn publish_with_credentials(
        &self,
        key: &str,
        record: &[u8],
        blobs: &[BlobRef<'_>],
        credentials: &Credentials,
    ) -> Result<()> {
        let cancel = AtomicBool::new(false);
        let output = Temporary::new(&std::env::temp_dir())?;
        let action = self.write_url(&format!("actions/{key}.json"));
        for blob in blobs {
            validate_hash(blob.hash)?;
        }
        // A lost conditional write restarts inspection, including repair of
        // the winner's references. Bound contention rather than waiting forever.
        for _ in 0..3 {
            let (status, etag) = self.request_with_condition(
                "GET",
                &action,
                &output.0,
                None,
                Some(credentials),
                MAX_RECORD,
                &cancel,
                self.read_timeout,
                None,
            )?;
            ensure!(
                status == 200 || status == 404,
                "R2 record inspection returned HTTP {status}"
            );
            let existing = if status == 200 {
                record_hashes(&fs::read(&output.0)?).ok()
            } else {
                None
            };
            if let Some(hashes) = existing {
                self.repair_blobs(&hashes, blobs, &output.0, credentials, &cancel)?;
                return Ok(());
            }
            let condition = if status == 404 {
                "If-None-Match: *".to_string()
            } else {
                let etag = etag
                    .context("R2 invalid record lacks ETag; refusing unconditional replacement")?;
                ensure!(
                    etag.starts_with('"')
                        && etag.ends_with('"')
                        && etag.bytes().all(|b| (0x21..=0x7e).contains(&b)),
                    "R2 record has an unsafe or weak ETag"
                );
                format!("If-Match: {etag}")
            };
            self.repair_blobs(
                &record_hashes(record)?,
                blobs,
                &output.0,
                credentials,
                &cancel,
            )?;
            let input = Temporary::new(&std::env::temp_dir())?;
            fs::write(&input.0, record)?;
            let (status, _) = self.request_with_condition(
                "PUT",
                &action,
                &output.0,
                Some(&input.0),
                Some(credentials),
                MAX_RECORD,
                &cancel,
                Duration::from_secs(30),
                Some(&condition),
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
        credentials: &Credentials,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut missing = Vec::new();
        for hash in hashes {
            let url = self.write_url(&format!("blobs/{hash}"));
            let status = self.request(
                "HEAD",
                &url,
                output,
                None,
                Some(credentials),
                MAX_BLOB,
                cancel,
                self.read_timeout,
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

    fn write_url(&self, object: &str) -> String {
        format!(
            "{}/{}/{NAMESPACE}/{object}",
            self.config
                .endpoint
                .as_deref()
                .unwrap_or_default()
                .trim_end_matches('/'),
            self.config.bucket.as_deref().unwrap_or_default()
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
    ) -> Result<u16> {
        self.request_with_condition(
            method,
            url,
            output,
            input,
            credentials,
            limit,
            cancel,
            timeout,
            None,
        )
        .map(|(status, _)| status)
    }

    #[allow(clippy::too_many_arguments)]
    fn request_with_condition(
        &self,
        method: &str,
        url: &str,
        output: &Path,
        input: Option<&Path>,
        credentials: Option<&Credentials>,
        limit: u64,
        cancel: &AtomicBool,
        timeout: Duration,
        condition: Option<&str>,
    ) -> Result<(u16, Option<String>)> {
        ensure!(!cancel.load(Ordering::Relaxed), "remote request cancelled");
        let headers = Temporary::new(&std::env::temp_dir())?;
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
            .arg("--dump-header")
            .arg(&headers.0)
            .args(["--write-out", "%{http_code}", "--url", url]);
        if let Some(condition) = condition {
            command.arg("--header").arg(condition);
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
                if cancel.load(Ordering::Relaxed)
                    || start.elapsed() > timeout
                    || fs::metadata(output).is_ok_and(|m| m.len() > limit)
                    || fs::metadata(&headers.0).is_ok_and(|m| m.len() > 64 * 1024)
                {
                    bail!("remote request cancelled or exceeded its bounds");
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
            return result.map(|_| (0, None));
        }
        let response = child.wait_with_output()?;
        ensure!(
            response.status.success(),
            "R2 curl transport failed ({})",
            response.status
        );
        let mut etag = None;
        // Ignore interim/proxy response headers; only the final response's
        // validator may authorize replacement of the downloaded record.
        for line in fs::read_to_string(&headers.0)?.lines() {
            if line.starts_with("HTTP/") {
                etag = None;
            } else if let Some((name, value)) = line.split_once(':') {
                if name.eq_ignore_ascii_case("etag") {
                    etag = Some(value.trim().to_owned());
                }
            }
        }
        Ok((std::str::from_utf8(&response.stdout)?.parse()?, etag))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sha256_hex;
    use std::{
        collections::HashMap,
        io::{BufRead, BufReader, Read},
        net::{TcpListener, TcpStream},
        sync::{Arc, Barrier, Mutex},
    };

    #[derive(Default)]
    struct State {
        objects: HashMap<String, Vec<u8>>,
        puts: Vec<String>,
        requests: Vec<(String, bool)>,
        fail_put: bool,
        delay: Duration,
        record_get_barrier: Option<Arc<Barrier>>,
        gated_gets: usize,
        competing_record: Option<Vec<u8>>,
        always_conflict: bool,
        precondition_failures: usize,
        connections: usize,
        active_gets: usize,
        peak_gets: usize,
        read_status: Option<u16>,
        read_length: Option<u64>,
    }

    struct Server {
        base: String,
        state: Arc<Mutex<State>>,
        stop: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl Server {
        fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let state = Arc::new(Mutex::new(State::default()));
            let stop = Arc::new(AtomicBool::new(false));
            let s = state.clone();
            let done = stop.clone();
            let worker = thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            s.lock().unwrap().connections += 1;
                            let s = s.clone();
                            thread::spawn(move || {
                                // Reset/broken-pipe is expected when testing
                                // cancellation and response-size bounds.
                                let _ = serve(stream, s);
                            });
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
            });
            Self {
                base,
                state,
                stop,
                worker: Some(worker),
            }
        }

        fn cache(&self) -> Cache {
            Cache::new(Config {
                read_url: format!("{}/bucket", self.base),
                endpoint: Some(self.base.clone()),
                bucket: Some("bucket".into()),
            })
            .unwrap()
        }

        fn set(&self, name: &str, bytes: &[u8]) {
            self.state
                .lock()
                .unwrap()
                .objects
                .insert(format!("/bucket/{NAMESPACE}/{name}"), bytes.to_vec());
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn serve(stream: TcpStream, state: Arc<Mutex<State>>) -> std::io::Result<()> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut reader = BufReader::new(stream);
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let words: Vec<_> = line.split_whitespace().collect();
            let (method, path) = (words[0].to_string(), words[1].to_string());
            let mut length = 0;
            let mut auth = false;
            let mut if_match = None;
            let mut if_none_match = None;
            loop {
                line.clear();
                reader.read_line(&mut line)?;
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                let lower = line.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
                if lower.starts_with("authorization:") {
                    auth = true;
                    assert!(line.contains("/auto/s3/aws4_request"));
                }
                if lower.starts_with("if-match:") {
                    if_match = Some(line.split_once(':').unwrap().1.trim().to_owned());
                }
                if lower.starts_with("if-none-match:") {
                    if_none_match = Some(line.split_once(':').unwrap().1.trim().to_owned());
                }
                if lower.starts_with("expect:") {
                    reader
                        .get_mut()
                        .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body)?;
            let (status, response, delay, etag, barrier, length) = {
                let mut state = state.lock().unwrap();
                state.requests.push((method.clone(), auth));
                if method == "GET" {
                    state.active_gets += 1;
                    state.peak_gets = state.peak_gets.max(state.active_gets);
                }
                let (status, bytes) = if method == "PUT" {
                    assert!(auth);
                    let is_record = path.contains("/actions/");
                    if is_record {
                        assert!(if_match.is_some() || if_none_match.is_some());
                        if let Some(winner) = state.competing_record.take() {
                            state.objects.insert(path.clone(), winner);
                        }
                    }
                    let current_etag = state
                        .objects
                        .get(&path)
                        .map(|b| format!("\"{}\"", sha256_hex(b)));
                    let conflict = (is_record && state.always_conflict)
                        || (if_none_match.as_deref() == Some("*") && current_etag.is_some())
                        || if_match
                            .as_ref()
                            .is_some_and(|tag| current_etag.as_ref() != Some(tag));
                    if conflict {
                        state.precondition_failures += 1;
                        (412, Vec::new())
                    } else if state.fail_put {
                        (503, Vec::new())
                    } else {
                        state.puts.push(path.clone());
                        state.objects.insert(path.clone(), body);
                        (200, Vec::new())
                    }
                } else {
                    match state.objects.get(&path) {
                        Some(bytes) => (state.read_status.unwrap_or(200), bytes.clone()),
                        None => (404, Vec::new()),
                    }
                };
                let etag = state
                    .objects
                    .get(&path)
                    .map(|b| format!("ETag: \"{}\"\r\n", sha256_hex(b)))
                    .unwrap_or_default();
                let barrier =
                    if method == "GET" && path.contains("/actions/") && state.gated_gets < 2 {
                        state.gated_gets += 1;
                        state.record_get_barrier.clone()
                    } else {
                        None
                    };
                let length = state.read_length.unwrap_or(bytes.len() as u64);
                (status, bytes, state.delay, etag, barrier, length)
            };
            if let Some(barrier) = barrier {
                barrier.wait();
            }
            thread::sleep(delay);
            let result = (|| {
                write!(
            reader.get_mut(),
            "HTTP/1.1 {status} Response\r\n{etag}Content-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            length
        )?;
                if method != "HEAD" {
                    reader.get_mut().write_all(&response)?;
                }
                std::io::Result::Ok(())
            })();
            if method == "GET" {
                state.lock().unwrap().active_gets -= 1;
            }
            result?;
        }
    }

    fn credentials() -> Credentials {
        Credentials {
            access: "fixture-access".into(),
            secret: "fixture-secret".into(),
        }
    }

    fn key() -> String {
        sha256_hex(b"action")
    }
    fn cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }
    fn record(hashes: &[&str]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"blobs": hashes})).unwrap()
    }

    struct Local(Store);
    impl Local {
        fn new() -> Self {
            let marker = Temporary::new(&std::env::temp_dir()).unwrap();
            Self(Store::new(marker.0.with_extension("store")).unwrap())
        }
    }
    impl Drop for Local {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0.root);
        }
    }

    #[test]
    fn reads_miss_on_missing_corrupt_unavailable_and_oversized_records() {
        let server = Server::new();
        let cache = server.cache();
        let name = format!("actions/{}.json", key());
        assert!(cache.fetch_record(&key(), &cancel()).unwrap().is_none());
        server.set(&name, b"broken");
        assert!(cache.fetch_record(&key(), &cancel()).unwrap().is_none());
        server.set(&name, &vec![b' '; MAX_RECORD as usize + 1]);
        assert!(cache.fetch_record(&key(), &cancel()).unwrap().is_none());
        server.set(&name, &record(&[]));
        assert_eq!(
            cache.fetch_record(&key(), &cancel()).unwrap().unwrap(),
            record(&[])
        );
        assert!(server
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|(_, auth)| !auth));
        drop(server);
        assert!(cache.fetch_record(&key(), &cancel()).unwrap().is_none());
    }

    #[test]
    fn downloads_verified_blobs_repairs_local_corruption_and_cleans_temps() {
        let server = Server::new();
        let cache = server.cache();
        let local = Local::new();
        let hash = sha256_hex(b"correct bytes");
        let name = format!("blobs/{hash}");
        assert!(!cache
            .fetch_blobs(&local.0, std::slice::from_ref(&hash), &cancel())
            .unwrap());
        server.set(&name, b"wrong bytes");
        assert!(!cache
            .fetch_blobs(&local.0, std::slice::from_ref(&hash), &cancel())
            .unwrap());
        assert!(!local.0.cache_path(&hash).exists());
        server.set(&name, b"correct bytes");
        assert!(cache
            .fetch_blobs(&local.0, std::slice::from_ref(&hash), &cancel())
            .unwrap());
        let path = local.0.cache_path(&hash);
        fs::write(&path, b"damaged").unwrap();
        assert!(cache
            .fetch_blobs(&local.0, std::slice::from_ref(&hash), &cancel())
            .unwrap());
        assert_eq!(fs::read(path).unwrap(), b"correct bytes");
        assert_eq!(fs::read_dir(local.0.root.join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn batch_downloads_overlap_with_bounded_concurrency_and_reuse_connections() {
        let server = Server::new();
        server.state.lock().unwrap().delay = Duration::from_millis(120);
        let cache = server.cache();
        let local = Local::new();
        let mut hashes = Vec::new();
        for index in 0..GET_CONCURRENCY * 3 + 1 {
            let bytes = format!("blob {index}");
            let hash = sha256_hex(bytes.as_bytes());
            server.set(&format!("blobs/{hash}"), bytes.as_bytes());
            hashes.push(hash);
        }
        let count = hashes.len();
        hashes.extend(hashes.clone());
        assert!(cache.fetch_blobs(&local.0, &hashes, &cancel()).unwrap());
        for hash in &hashes {
            assert_eq!(sha256_file(&local.0.cache_path(hash)).unwrap(), *hash);
        }
        {
            let state = server.state.lock().unwrap();
            assert_eq!(
                state.requests.len(),
                count,
                "duplicate hashes must be deduplicated"
            );
            assert!(state.peak_gets > 1, "GETs should overlap");
            assert!(state.peak_gets <= GET_CONCURRENCY);
            assert!(
                state.connections < count,
                "curl should reuse keepalive connections"
            );
            assert!(state
                .requests
                .iter()
                .all(|(method, auth)| method == "GET" && !auth));
        }
        assert!(cache.fetch_blobs(&local.0, &hashes, &cancel()).unwrap());
        assert_eq!(
            server.state.lock().unwrap().requests.len(),
            count,
            "verified local blobs should not be requested"
        );
        assert_eq!(fs::read_dir(local.0.root.join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn large_batch_uses_config_with_long_quoted_paths() {
        let server = Server::new();
        let local = Local::new();
        let component = format!(
            "quoted-\"-backslash-\\-tab-\t-newline-\n-{}",
            "x".repeat(140)
        );
        let store = Store::new(
            local
                .0
                .root
                .join(&component)
                .join(&component)
                .join(&component),
        )
        .unwrap();
        let hashes: Vec<_> = (0..512)
            .map(|index| {
                let bytes = format!("large batch {index}");
                let hash = sha256_hex(bytes.as_bytes());
                server.set(&format!("blobs/{hash}"), bytes.as_bytes());
                hash
            })
            .collect();
        // Output paths alone would exceed macOS's 256 KiB argv limit.
        assert!(store.root.as_os_str().len() * hashes.len() > 256 * 1024);
        assert!(server
            .cache()
            .fetch_blobs(&store, &hashes, &cancel())
            .unwrap());
        for hash in &hashes {
            assert_eq!(sha256_file(&store.cache_path(hash)).unwrap(), *hash);
        }
        assert_eq!(server.state.lock().unwrap().requests.len(), hashes.len());
        assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn batch_deadline_allows_queued_waves_and_transfer_timeout_cleans_up() {
        let server = Server::new();
        let local = Local::new();
        let mut cache = server.cache();
        cache.read_timeout = Duration::from_millis(500);
        server.state.lock().unwrap().delay = Duration::from_millis(180);
        let hashes: Vec<_> = (0..GET_CONCURRENCY * 4)
            .map(|index| {
                let bytes = format!("wave {index}");
                let hash = sha256_hex(bytes.as_bytes());
                server.set(&format!("blobs/{hash}"), bytes.as_bytes());
                hash
            })
            .collect();
        let start = Instant::now();
        assert!(cache.fetch_blobs(&local.0, &hashes, &cancel()).unwrap());
        assert!(start.elapsed() > cache.read_timeout);

        let other = Local::new();
        server.state.lock().unwrap().delay = Duration::from_secs(2);
        cache.read_timeout = Duration::from_millis(80);
        assert!(!cache.fetch_blobs(&other.0, &hashes, &cancel()).unwrap());
        assert!(hashes.iter().all(|hash| !other.0.cache_path(hash).exists()));
        assert_eq!(fs::read_dir(other.0.root.join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn batch_missing_corrupt_unavailable_and_cancelled_are_misses() {
        let server = Server::new();
        let cache = server.cache();
        let local = Local::new();
        let good = sha256_hex(b"good");
        let bad = sha256_hex(b"bad");
        let hashes = vec![good.clone(), bad.clone()];
        server.set(&format!("blobs/{good}"), b"good");
        assert!(!cache.fetch_blobs(&local.0, &hashes, &cancel()).unwrap());
        server.set(&format!("blobs/{bad}"), b"corrupt");
        assert!(!cache.fetch_blobs(&local.0, &hashes, &cancel()).unwrap());
        assert!(!local.0.cache_path(&bad).exists());
        assert_eq!(fs::read_dir(local.0.root.join("tmp")).unwrap().count(), 0);
        assert!(cache
            .fetch_blobs(&local.0, &["../escape".into()], &cancel())
            .is_err());
        assert!(cache.fetch_blobs(&local.0, &[], &cancel()).unwrap());

        server.state.lock().unwrap().delay = Duration::from_secs(2);
        let flag = cancel();
        thread::scope(|scope| {
            let worker = scope.spawn(|| cache.fetch_blobs(&local.0, &hashes, &flag).unwrap());
            let deadline = Instant::now() + Duration::from_secs(2);
            while server.state.lock().unwrap().active_gets == 0 {
                assert!(Instant::now() < deadline, "curl never started");
                thread::sleep(Duration::from_millis(5));
            }
            let start = Instant::now();
            flag.store(true, Ordering::Relaxed);
            assert!(!worker.join().unwrap());
            assert!(start.elapsed() < Duration::from_secs(1));
        });
        assert!(!local.0.cache_path(&bad).exists());
        assert_eq!(fs::read_dir(local.0.root.join("tmp")).unwrap().count(), 0);
        assert!(!cache.fetch_blobs(&local.0, &[], &flag).unwrap());
        drop(server);
        assert!(!cache.fetch_blobs(&local.0, &hashes, &cancel()).unwrap());
    }

    #[test]
    fn batch_rejects_non_200_oversized_and_incomplete_responses() {
        let server = Server::new();
        let local = Local::new();
        let mut cache = server.cache();
        cache.read_timeout = Duration::from_millis(100);
        let hash = sha256_hex(b"body");
        let hashes = vec![hash.clone()];
        server.set(&format!("blobs/{hash}"), b"body");
        for status in [206, 302, 503] {
            server.state.lock().unwrap().read_status = Some(status);
            assert!(!cache.fetch_blobs(&local.0, &hashes, &cancel()).unwrap());
            assert!(!local.0.cache_path(&hash).exists());
        }
        server.state.lock().unwrap().read_status = None;
        for length in [MAX_BLOB + 1, 5] {
            server.state.lock().unwrap().read_length = Some(length);
            assert!(!cache.fetch_blobs(&local.0, &hashes, &cancel()).unwrap());
            assert!(!local.0.cache_path(&hash).exists());
            assert_eq!(fs::read_dir(local.0.root.join("tmp")).unwrap().count(), 0);
        }
    }

    #[test]
    fn timeout_and_cancellation_kill_in_flight_reads_promptly() {
        let server = Server::new();
        server.state.lock().unwrap().delay = Duration::from_secs(2);
        let mut cache = server.cache();
        cache.read_timeout = Duration::from_millis(80);
        let start = Instant::now();
        assert!(cache.fetch_record(&key(), &cancel()).unwrap().is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
        cache.read_timeout = Duration::from_secs(10);
        let flag = cancel();
        let flag2 = flag.clone();
        let worker = thread::spawn(move || cache.fetch_record(&key(), &flag2).unwrap());
        thread::sleep(Duration::from_millis(40));
        let start = Instant::now();
        flag.store(true, Ordering::Relaxed);
        assert!(worker.join().unwrap().is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn publication_orders_blobs_then_record_and_repairs_without_touching_record() {
        let server = Server::new();
        let cache = server.cache();
        let local = Local::new();
        let hash = local.0.insert_bytes(b"blob").unwrap();
        let path = local.0.cache_path(&hash);
        let refs = [BlobRef {
            hash: &hash,
            path: &path,
        }];
        let bytes = record(&[&hash]);
        cache
            .publish_with_credentials(&key(), &bytes, &refs, &credentials())
            .unwrap();
        let blob_name = format!("/bucket/{NAMESPACE}/blobs/{hash}");
        let action_name = format!("/bucket/{NAMESPACE}/actions/{}.json", key());
        {
            let mut state = server.state.lock().unwrap();
            assert_eq!(state.puts, [blob_name.clone(), action_name.clone()]);
            state.objects.remove(&blob_name);
            state.puts.clear();
        }
        // Even a differing proposed record must repair/preserve the existing one.
        cache
            .publish_with_credentials(&key(), &record(&[]), &refs, &credentials())
            .unwrap();
        {
            let mut state = server.state.lock().unwrap();
            assert_eq!(state.puts, [blob_name]);
            assert_eq!(state.objects[&action_name], bytes);
            state.puts.clear();
        }
        cache
            .publish_with_credentials(&key(), &record(&[]), &[], &credentials())
            .unwrap();
        assert!(server.state.lock().unwrap().puts.is_empty());
    }

    #[test]
    fn malformed_record_is_replaced_but_upload_errors_do_not_publish_record() {
        let server = Server::new();
        let cache = server.cache();
        let local = Local::new();
        let hash = local.0.insert_bytes(b"blob").unwrap();
        let path = local.0.cache_path(&hash);
        let refs = [BlobRef {
            hash: &hash,
            path: &path,
        }];
        let name = format!("actions/{}.json", key());
        server.set(&name, b"bad json");
        server.state.lock().unwrap().fail_put = true;
        assert!(cache
            .publish_with_credentials(&key(), &record(&[&hash]), &refs, &credentials())
            .is_err());
        assert!(server.state.lock().unwrap().puts.is_empty());
        server.state.lock().unwrap().fail_put = false;
        cache
            .publish_with_credentials(&key(), &record(&[&hash]), &refs, &credentials())
            .unwrap();
        assert_eq!(server.state.lock().unwrap().puts.len(), 2);
        server
            .state
            .lock()
            .unwrap()
            .objects
            .remove(&format!("/bucket/{NAMESPACE}/blobs/{hash}"));
        assert!(cache
            .publish_with_credentials(&key(), &record(&[]), &[], &credentials())
            .is_err());
    }

    #[test]
    fn concurrent_publishers_preserve_winner_for_absent_and_invalid_records() {
        for old in [None, Some(b"invalid".as_slice())] {
            let server = Server::new();
            let name = format!("actions/{}.json", key());
            if let Some(old) = old {
                server.set(&name, old);
            }
            server.state.lock().unwrap().record_get_barrier = Some(Arc::new(Barrier::new(2)));
            let mut workers = Vec::new();
            for id in 0..2 {
                let cache = server.cache();
                workers.push(thread::spawn(move || {
                    let bytes =
                        serde_json::to_vec(&serde_json::json!({"blobs": [], "publisher": id}))
                            .unwrap();
                    cache
                        .publish_with_credentials(&key(), &bytes, &[], &credentials())
                        .unwrap();
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
            let state = server.state.lock().unwrap();
            assert_eq!(state.puts.len(), 1);
            assert_eq!(state.precondition_failures, 1);
            assert!(record_hashes(&state.objects[&format!("/bucket/{NAMESPACE}/{name}")]).is_ok());
        }
    }

    #[test]
    fn lost_conditional_write_repairs_winning_record_and_retries_are_bounded() {
        let server = Server::new();
        let cache = server.cache();
        let local = Local::new();
        let hash = local.0.insert_bytes(b"winner blob").unwrap();
        let path = local.0.cache_path(&hash);
        let winner = record(&[&hash]);
        server.state.lock().unwrap().competing_record = Some(winner.clone());
        cache
            .publish_with_credentials(
                &key(),
                &record(&[]),
                &[BlobRef {
                    hash: &hash,
                    path: &path,
                }],
                &credentials(),
            )
            .unwrap();
        {
            let state = server.state.lock().unwrap();
            assert_eq!(state.precondition_failures, 1);
            assert_eq!(state.puts, [format!("/bucket/{NAMESPACE}/blobs/{hash}")]);
            assert_eq!(
                state.objects[&format!("/bucket/{NAMESPACE}/actions/{}.json", key())],
                winner
            );
        }
        let server = Server::new();
        server.state.lock().unwrap().always_conflict = true;
        assert!(server
            .cache()
            .publish_with_credentials(&key(), &record(&[]), &[], &credentials())
            .is_err());
        assert_eq!(server.state.lock().unwrap().precondition_failures, 3);
    }

    #[test]
    fn read_only_config_and_unsafe_keys() {
        let config: Config = toml::from_str("read-url = 'https://cache.example'").unwrap();
        let cache = Cache::new(config).unwrap();
        assert!(cache.validate_write_config().is_err());
        assert!(cache.fetch_record("../escape", &cancel()).is_err());
        assert!(
            toml::from_str::<Config>("read-url = 'https://cache.example'\nsecret = 'no'").is_err()
        );
    }
}
