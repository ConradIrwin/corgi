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
        active_gets: usize,
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
            })
            .unwrap()
        }

        fn write_config(&self) -> WriteConfig {
            WriteConfig {
                url: format!("{}/bucket", self.base),
                credentials: credentials(),
            }
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
            let (status, response, delay, barrier, length) = {
                let mut state = state.lock().unwrap();
                state.requests.push((method.clone(), auth));
                if method == "GET" {
                    state.active_gets += 1;
                }
                let (status, bytes) = if method == "PUT" {
                    assert!(auth);
                    let is_record = path.contains("/actions/");
                    if is_record {
                        assert_eq!(if_none_match.as_deref(), Some("*"));
                        if let Some(winner) = state.competing_record.take() {
                            state.objects.insert(path.clone(), winner);
                        }
                    }
                    let conflict = (is_record && state.always_conflict)
                        || (if_none_match.as_deref() == Some("*")
                            && state.objects.contains_key(&path));
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
                let barrier =
                    if method == "GET" && path.contains("/actions/") && state.gated_gets < 2 {
                        state.gated_gets += 1;
                        state.record_get_barrier.clone()
                    } else {
                        None
                    };
                let length = state.read_length.unwrap_or(bytes.len() as u64);
                (status, bytes, state.delay, barrier, length)
            };
            if let Some(barrier) = barrier {
                barrier.wait();
            }
            thread::sleep(delay);
            let result = (|| {
                write!(
            reader.get_mut(),
            "HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
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
        assert!(cache
            .fetch_record(&key(), &cancel())
            .unwrap_err()
            .to_string()
            .contains("invalid remote record JSON"));
        server.set(&name, &vec![b' '; MAX_RECORD as usize + 1]);
        assert!(cache.fetch_record(&key(), &cancel()).is_err());
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
        assert!(cache.fetch_record(&key(), &cancel()).is_err());
    }

    #[test]
    fn downloads_verified_blobs_repairs_local_corruption_and_cleans_temps() {
        let server = Server::new();
        let cache = server.cache();
        let local = Local::new();
        let hash = sha256_hex(b"correct bytes");
        let name = format!("blobs/{hash}");
        assert!(cache
            .fetch_blob(&local.0, &hash, &cancel())
            .unwrap_err()
            .to_string()
            .contains("404"));
        server.set(&name, b"wrong bytes");
        assert!(cache
            .fetch_blob(&local.0, &hash, &cancel())
            .unwrap_err()
            .to_string()
            .contains("SHA-256"));
        assert!(!local.0.cache_path(&hash).exists());
        server.set(&name, b"correct bytes");
        cache.fetch_blob(&local.0, &hash, &cancel()).unwrap();
        let path = local.0.cache_path(&hash);
        fs::write(&path, b"damaged").unwrap();
        cache.fetch_blob(&local.0, &hash, &cancel()).unwrap();
        assert_eq!(fs::read(path).unwrap(), b"correct bytes");
        assert_eq!(fs::read_dir(local.0.root.join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn blob_downloads_support_long_quoted_paths_and_reuse_verified_local_bytes() {
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
        let hash = sha256_hex(b"blob");
        server.set(&format!("blobs/{hash}"), b"blob");
        for _ in 0..2 {
            server.cache().fetch_blob(&store, &hash, &cancel()).unwrap();
            assert_eq!(sha256_file(&store.cache_path(&hash)).unwrap(), hash);
        }
        assert_eq!(server.state.lock().unwrap().requests.len(), 1);
        assert_eq!(fs::read_dir(store.root.join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn blob_transfer_timeout_cleans_up() {
        let server = Server::new();
        let local = Local::new();
        let mut cache = server.cache();
        let hash = sha256_hex(b"blob");
        server.set(&format!("blobs/{hash}"), b"blob");
        server.state.lock().unwrap().delay = Duration::from_secs(2);
        cache.read_timeout = Duration::from_millis(80);
        assert!(cache.fetch_blob(&local.0, &hash, &cancel()).is_err());
        assert!(!local.0.cache_path(&hash).exists());
        assert_eq!(fs::read_dir(local.0.root.join("tmp")).unwrap().count(), 0);
    }

    #[test]
    fn blob_missing_corrupt_unavailable_and_cancelled_are_misses() {
        let server = Server::new();
        let cache = server.cache();
        let local = Local::new();
        let bad = sha256_hex(b"bad");
        assert!(cache.fetch_blob(&local.0, &bad, &cancel()).is_err());
        server.set(&format!("blobs/{bad}"), b"corrupt");
        assert!(cache.fetch_blob(&local.0, &bad, &cancel()).is_err());
        assert!(!local.0.cache_path(&bad).exists());
        assert_eq!(fs::read_dir(local.0.root.join("tmp")).unwrap().count(), 0);
        assert!(cache.fetch_blob(&local.0, "../escape", &cancel()).is_err());

        server.state.lock().unwrap().delay = Duration::from_secs(2);
        let flag = cancel();
        thread::scope(|scope| {
            let worker = scope.spawn(|| cache.fetch_blob(&local.0, &bad, &flag));
            let deadline = Instant::now() + Duration::from_secs(2);
            while server.state.lock().unwrap().active_gets == 0 {
                assert!(Instant::now() < deadline, "curl never started");
                thread::sleep(Duration::from_millis(5));
            }
            let start = Instant::now();
            flag.store(true, Ordering::Relaxed);
            assert!(worker.join().unwrap().unwrap_err().is::<ReadCancelled>());
            assert!(start.elapsed() < Duration::from_secs(1));
        });
        assert!(!local.0.cache_path(&bad).exists());
        assert_eq!(fs::read_dir(local.0.root.join("tmp")).unwrap().count(), 0);
        assert!(cache
            .fetch_blob(&local.0, &bad, &flag)
            .unwrap_err()
            .is::<ReadCancelled>());
        drop(server);
        assert!(cache.fetch_blob(&local.0, &bad, &cancel()).is_err());
    }

    #[test]
    fn blob_rejects_non_200_oversized_and_incomplete_responses() {
        let server = Server::new();
        let local = Local::new();
        let mut cache = server.cache();
        cache.read_timeout = Duration::from_millis(100);
        let hash = sha256_hex(b"body");
        server.set(&format!("blobs/{hash}"), b"body");
        for status in [206, 302, 503] {
            server.state.lock().unwrap().read_status = Some(status);
            assert!(cache
                .fetch_blob(&local.0, &hash, &cancel())
                .unwrap_err()
                .to_string()
                .contains(&status.to_string()));
            assert!(!local.0.cache_path(&hash).exists());
        }
        server.state.lock().unwrap().read_status = None;
        for length in [MAX_BLOB + 1, 5] {
            server.state.lock().unwrap().read_length = Some(length);
            assert!(cache.fetch_blob(&local.0, &hash, &cancel()).is_err());
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
        assert!(cache.fetch_record(&key(), &cancel()).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        cache.read_timeout = Duration::from_secs(10);
        let flag = cancel();
        let flag2 = flag.clone();
        let worker = thread::spawn(move || cache.fetch_record(&key(), &flag2));
        thread::sleep(Duration::from_millis(40));
        let start = Instant::now();
        flag.store(true, Ordering::Relaxed);
        assert!(worker.join().unwrap().unwrap_err().is::<ReadCancelled>());
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
            .publish_with_config(&key(), &bytes, &refs, &server.write_config())
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
            .publish_with_config(&key(), &record(&[]), &refs, &server.write_config())
            .unwrap();
        {
            let mut state = server.state.lock().unwrap();
            assert_eq!(state.puts, [blob_name]);
            assert_eq!(state.objects[&action_name], bytes);
            state.puts.clear();
        }
        cache
            .publish_with_config(&key(), &record(&[]), &[], &server.write_config())
            .unwrap();
        assert!(server.state.lock().unwrap().puts.is_empty());
    }

    #[test]
    fn malformed_records_are_reported_without_writes() {
        let server = Server::new();
        let cache = server.cache();
        let name = format!("actions/{}.json", key());
        for bytes in [
            b"bad json".as_slice(),
            br#"{"blobs": ["bad hash"]}"#,
            br#"{}"#,
        ] {
            server.set(&name, bytes);
            let error = cache
                .publish_with_config(&key(), &record(&[]), &[], &server.write_config())
                .unwrap_err();
            assert!(format!("{error:#}").contains(&format!("invalid remote record {name}")));
            let state = server.state.lock().unwrap();
            assert!(state.puts.is_empty());
            assert_eq!(state.objects[&format!("/bucket/{NAMESPACE}/{name}")], bytes);
        }
    }

    #[test]
    fn upload_errors_do_not_publish_record() {
        let server = Server::new();
        let cache = server.cache();
        let local = Local::new();
        let hash = local.0.insert_bytes(b"blob").unwrap();
        let path = local.0.cache_path(&hash);
        let refs = [BlobRef {
            hash: &hash,
            path: &path,
        }];
        server.state.lock().unwrap().fail_put = true;
        assert!(cache
            .publish_with_config(&key(), &record(&[&hash]), &refs, &server.write_config())
            .is_err());
        assert!(server.state.lock().unwrap().puts.is_empty());
        server.state.lock().unwrap().fail_put = false;
        cache
            .publish_with_config(&key(), &record(&[&hash]), &refs, &server.write_config())
            .unwrap();
        assert_eq!(server.state.lock().unwrap().puts.len(), 2);
        server
            .state
            .lock()
            .unwrap()
            .objects
            .remove(&format!("/bucket/{NAMESPACE}/blobs/{hash}"));
        assert!(cache
            .publish_with_config(&key(), &record(&[]), &[], &server.write_config())
            .is_err());
    }

    #[test]
    fn concurrent_publishers_preserve_first_record() {
        let server = Server::new();
        let name = format!("actions/{}.json", key());
        server.state.lock().unwrap().record_get_barrier = Some(Arc::new(Barrier::new(2)));
        let mut workers = Vec::new();
        for id in 0..2 {
            let cache = server.cache();
            let config = server.write_config();
            workers.push(thread::spawn(move || {
                let bytes =
                    serde_json::to_vec(&serde_json::json!({"blobs": [], "publisher": id})).unwrap();
                cache
                    .publish_with_config(&key(), &bytes, &[], &config)
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
            .publish_with_config(
                &key(),
                &record(&[]),
                &[BlobRef {
                    hash: &hash,
                    path: &path,
                }],
                &server.write_config(),
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
            .publish_with_config(&key(), &record(&[]), &[], &server.write_config())
            .is_err());
        assert_eq!(server.state.lock().unwrap().precondition_failures, 3);
    }

    #[test]
    fn write_url_contains_the_bucket_but_not_the_cache_namespace() {
        let base = "https://account.r2.cloudflarestorage.com/my-bucket";
        for url in [base.to_string(), format!("{base}/")] {
            validate_write_url(&url).unwrap();
            let config = WriteConfig {
                url,
                credentials: credentials(),
            };
            assert_eq!(
                config.object_url("actions/key.json"),
                format!("{base}/{NAMESPACE}/actions/key.json")
            );
        }
        validate_write_url("http://127.0.0.1:1234/bucket").unwrap();
        for url in [
            "",
            "file:///bucket",
            "https:///bucket",
            "https://account.r2.cloudflarestorage.com",
            "https://account.r2.cloudflarestorage.com/",
            "https://account.r2.cloudflarestorage.com/bucket/corgi/v1",
            "https://account.r2.cloudflarestorage.com/bucket?query",
            "https://account.r2.cloudflarestorage.com/bucket#fragment",
            "https://secret@account.r2.cloudflarestorage.com/bucket",
            "https://account.r2.cloudflarestorage.com/bad bucket",
        ] {
            assert!(validate_write_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn publication_uses_the_write_url_not_the_public_read_url() {
        let reader = Server::new();
        let writer = Server::new();
        reader
            .cache()
            .publish_with_config(&key(), &record(&[]), &[], &writer.write_config())
            .unwrap();
        assert!(reader.state.lock().unwrap().requests.is_empty());
        assert_eq!(
            writer.state.lock().unwrap().puts,
            [format!("/bucket/{NAMESPACE}/actions/{}.json", key())]
        );
    }

    #[test]
    fn read_only_config_and_unsafe_keys() {
        let config: Config = toml::from_str("read-url = 'https://cache.example'").unwrap();
        let cache = Cache::new(config).unwrap();
        assert!(cache.fetch_record("../escape", &cancel()).is_err());
        for field in ["secret", "endpoint", "bucket", "write-url"] {
            assert!(toml::from_str::<Config>(&format!(
                "read-url = 'https://cache.example'\n{field} = 'not project config'"
            ))
            .is_err());
        }
    }
}
