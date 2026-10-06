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
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

pub const NAMESPACE: &str = "corgi/v1";
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
    pub fn fetch_record(&self, key: &str, cancel: &Arc<AtomicBool>) -> Result<Option<Vec<u8>>> {
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

    /// Only verified bytes are atomically renamed into the local CAS.
    /// Existing local bytes are checked too, so a damaged CAS entry is repaired.
    pub fn fetch_blob(
        &self,
        store: &Store,
        hash: &str,
        cancel: &Arc<AtomicBool>,
    ) -> Result<Option<PathBuf>> {
        validate_hash(hash)?;
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let dest = store.cache_path(hash);
        if sha256_file(&dest).ok().as_deref() == Some(hash) {
            return Ok(Some(dest));
        }
        let tmp = Temporary::new(&store.root.join("tmp"))?;
        let status = self.request(
            "GET",
            &self.read_url(&format!("blobs/{hash}")),
            &tmp.0,
            None,
            None,
            MAX_BLOB,
            cancel,
            self.read_timeout,
        );
        if !matches!(status, Ok(200))
            || sha256_file(&tmp.0).ok().as_deref() != Some(hash)
            || cancel.load(Ordering::Relaxed)
        {
            return Ok(None);
        }
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::rename(&tmp.0, &dest).context("publishing verified remote blob into CAS")?;
        Ok(Some(dest))
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
        let cancel = Arc::new(AtomicBool::new(false));
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
        cancel: &Arc<AtomicBool>,
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
        cancel: &Arc<AtomicBool>,
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
        cancel: &Arc<AtomicBool>,
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
        cancel: &Arc<AtomicBool>,
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
                    .write_all(credentials.config().as_bytes())?;
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

    fn config(&self) -> String {
        fn escape(s: &str) -> String {
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t")
        }
        format!(
            "user = \"{}:{}\"\n",
            escape(&self.access),
            escape(&self.secret)
        )
    }
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
        sync::{Barrier, Mutex},
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
        let mut line = String::new();
        reader.read_line(&mut line)?;
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
        let (status, response, delay, etag, barrier) = {
            let mut state = state.lock().unwrap();
            state.requests.push((method.clone(), auth));
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
                    Some(bytes) => (200, bytes.clone()),
                    None => (404, Vec::new()),
                }
            };
            let etag = state
                .objects
                .get(&path)
                .map(|b| format!("ETag: \"{}\"\r\n", sha256_hex(b)))
                .unwrap_or_default();
            let barrier = if method == "GET" && path.contains("/actions/") && state.gated_gets < 2 {
                state.gated_gets += 1;
                state.record_get_barrier.clone()
            } else {
                None
            };
            (status, bytes, state.delay, etag, barrier)
        };
        if let Some(barrier) = barrier {
            barrier.wait();
        }
        thread::sleep(delay);
        write!(
            reader.get_mut(),
            "HTTP/1.1 {status} Response\r\n{etag}Content-Length: {}\r\nConnection: close\r\n\r\n",
            response.len()
        )?;
        if method != "HEAD" {
            reader.get_mut().write_all(&response)?;
        }
        Ok(())
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
        assert!(cache
            .fetch_blob(&local.0, &hash, &cancel())
            .unwrap()
            .is_none());
        server.set(&name, b"wrong bytes");
        assert!(cache
            .fetch_blob(&local.0, &hash, &cancel())
            .unwrap()
            .is_none());
        assert!(!local.0.cache_path(&hash).exists());
        server.set(&name, b"correct bytes");
        let path = cache
            .fetch_blob(&local.0, &hash, &cancel())
            .unwrap()
            .unwrap();
        fs::write(&path, b"damaged").unwrap();
        assert_eq!(
            cache.fetch_blob(&local.0, &hash, &cancel()).unwrap(),
            Some(path.clone())
        );
        assert_eq!(fs::read(path).unwrap(), b"correct bytes");
        assert_eq!(fs::read_dir(local.0.root.join("tmp")).unwrap().count(), 0);
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
