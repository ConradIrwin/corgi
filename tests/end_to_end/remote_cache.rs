//! Remote build and test-result workflows against a loopback object store.
use super::*;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::{symlink, PermissionsExt},
    sync::{atomic::AtomicBool, Arc, Mutex},
    thread::{self, JoinHandle},
    time::Instant,
};

#[test]
fn remote_build_cache_lifecycle() {
    let fixture = Fixture::new("remote-build");
    fixture.run(&["cache", "build"]);
    let (record_path, record) = fixture
        .remote
        .record(|record| record["spec"]["target"]["name"] == "app");
    fixture.remote.retain_record(&record_path, &record);
    fixture.clear_results();
    // An unrelated nested build output must not change the remote source key.
    fixture.write("fixtures/target/app", "not an input");
    fs::remove_file(fixture.release_path()).unwrap();
    fixture.remote.objects.lock().unwrap().compiler_gate =
        Some((record_path.clone(), fixture.store.clone()));

    let output = fixture.run(&["build"]);
    assert!(output.contains("Downloaded"), "{output}");
    assert!(output.contains("— replaced local compilation"), "{output}");
    let report = fixture.report();
    let root = report_unit(&report, &fixture.directory.package_name, "compile");
    let download = report["remote_downloads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|download| download["remote_key"] == record["remote_key"])
        .unwrap();
    assert_eq!(root["cache"]["probe"], "remote");
    assert_eq!(root["key"]["hash"], record["action_key"]);
    assert!(download["superseded_local_ns"].as_u64().unwrap() > 0);
    assert!(!compiler_running(&fixture.store));
    fixture.assert_binary_output("42\n");

    fixture.remote.objects.lock().unwrap().requests.clear();
    fixture.run(&["build"]);
    assert!(
        fixture.remote.objects.lock().unwrap().requests.is_empty(),
        "an imported result must be reusable without network discovery"
    );

    let binary_hash = record["result"]["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|output| output["exe"] == true)
        .unwrap()["hash"]
        .as_str()
        .unwrap();
    let blob_path = format!("corgi/v2/blobs/{binary_hash}");
    let original = fixture
        .remote
        .objects
        .lock()
        .unwrap()
        .bytes
        .remove(&blob_path)
        .unwrap();
    assert!(original.starts_with(&[0x1f, 0x8b]), "blob must be gzipped");
    fixture.run(&["cache", "build"]);
    {
        let objects = fixture.remote.objects.lock().unwrap();
        assert_eq!(
            objects.bytes[&blob_path], original,
            "publication must repair missing blobs"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&objects.bytes[&record_path]).unwrap(),
            record
        );
        assert!(
            !objects
                .requests
                .contains(&("PUT".into(), record_path.clone())),
            "repair must leave the existing record unchanged"
        );
    }

    fixture.release_compiler();
    fixture.remote.objects.lock().unwrap().compiler_gate = None;
    let corrupt = fixture.store.join("corrupt");
    fs::write(&corrupt, b"corrupt").unwrap();
    let compressed = Command::new("gzip")
        .args(["-n", "-c"])
        .arg(&corrupt)
        .output()
        .unwrap();
    assert_success(&compressed, "gzip corrupt content");
    fixture
        .remote
        .objects
        .lock()
        .unwrap()
        .bytes
        .insert(blob_path.clone(), compressed.stdout);
    fixture.clear_results();
    fixture.run(&["build"]);
    let report = fixture.report();
    assert_unit_cache(
        &report,
        &fixture.directory.package_name,
        "compile",
        "app",
        "miss",
    );
    assert!(report["remote_downloads"]
        .as_array()
        .unwrap()
        .iter()
        .any(|download| download["remote_key"] == record["remote_key"]
            && download["outcome"] == "failed"
            && download["detail"].as_str().unwrap().contains("SHA-256")));
    fixture.assert_binary_output("42\n");

    fixture
        .remote
        .objects
        .lock()
        .unwrap()
        .bytes
        .insert(blob_path, original);
    fixture.write("dependency/src/lib.rs", "pub fn value() -> u32 { 43 }\n");
    fixture.clear_results();
    fixture.run(&["build"]);
    let report = fixture.report();
    assert_ne!(
        report_unit(&report, &fixture.directory.package_name, "compile")["key"]["hash"],
        record["action_key"],
        "dependency changes must invalidate the root"
    );
    fixture.assert_binary_output("43\n");
}

#[test]
fn remote_test_action_lifecycle() {
    let fixture = Fixture::new("remote-tests");
    fixture.write(
        "tests/cached.rs",
        r#"
#[test]
fn cached() {
    assert!(std::path::Path::new(env!("CARGO_BIN_EXE_app")).is_file());
    std::fs::write("cached-ran", "ran").unwrap();
    assert!(std::env::var_os("CORGI_TEST_FAIL").is_none());
}
"#,
    );
    fixture.write(
        "tests/uncached.rs",
        r#"
#[test]
fn uncached() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_app")).output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"42\n");
    std::fs::write("uncached-ran", "ran").unwrap();
}
"#,
    );
    fixture.run(&["cache", "test", "--test", "cached"]);
    let (record_path, record) = fixture
        .remote
        .record(|record| record["spec"]["action"] == "test_run");
    assert_eq!(record["blobs"], serde_json::json!([]));
    assert_eq!(record["result"]["test_result"]["test_count"], 1);
    fixture.remote.retain_record(&record_path, &record);
    fs::remove_file(fixture.workspace.join("cached-ran")).unwrap();
    fixture.clear_results();
    // Prevent the binary prerequisite from finishing before the cached test
    // action is installed. This controls the race without a timing assumption.
    let key = record["action_key"].as_str().unwrap();
    let result_path = fixture
        .store
        .join("cache")
        .join(&key[..2])
        .join(format!("{key}.json"));
    fs::remove_file(fixture.release_path()).unwrap();
    symlink(&result_path, fixture.release_path()).unwrap();

    let output = fixture.run(&["test", "--test", "cached"]);
    assert!(output.contains("Downloaded"), "{output}");
    assert!(output.contains("(run tests \"cached\")"), "{output}");
    assert!(!output.contains("— replaced local compilation"), "{output}");
    assert!(!fixture.workspace.join("cached-ran").exists());
    assert!(!fixture.workspace.join("target/debug/app").exists());
    let report = fixture.report();
    assert_eq!(
        report_unit(&report, &fixture.directory.package_name, "test_run")["cache"]["probe"],
        "remote"
    );
    assert_eq!(
        report["test_harnesses"][0]["cache"]["action_key"],
        record["action_key"]
    );

    fixture.clear_results();
    fixture.run(&["test"]);
    assert!(!fixture.workspace.join("cached-ran").exists());
    assert!(fixture.workspace.join("uncached-ran").exists());
    fixture.assert_binary_output("42\n");
    let report = fixture.report();
    assert_unit_cache(
        &report,
        &fixture.directory.package_name,
        "test_run",
        "cached",
        "hit",
    );
    assert_unit_cache(
        &report,
        &fixture.directory.package_name,
        "test_run",
        "uncached",
        "miss",
    );
    assert_unit_cache(
        &report,
        &fixture.directory.package_name,
        "compile",
        "app",
        "miss",
    );

    fixture.release_compiler();
    fixture.remote.objects.lock().unwrap().bytes.clear();
    let failed = fixture
        .command(&["cache", "test", "--test", "cached", "--force"])
        .env("CORGI_TEST_FAIL", "1")
        .output()
        .unwrap();
    assert_failure(
        &failed,
        "forced failure must not reuse or republish the old success",
    );
    assert!(fixture.workspace.join("cached-ran").exists());
    assert!(!result_path.exists());
    assert!(!fixture
        .remote
        .objects
        .lock()
        .unwrap()
        .bytes
        .contains_key(&record_path));
    fs::remove_file(fixture.workspace.join("cached-ran")).unwrap();
    let failed_again = fixture
        .command(&["test", "--test", "cached"])
        .env("CORGI_TEST_FAIL", "1")
        .output()
        .unwrap();
    assert_failure(
        &failed_again,
        "the next invocation must not reuse the invalidated result",
    );
    assert!(fixture.workspace.join("cached-ran").exists());
}

struct Fixture {
    remote: HttpCache,
    directory: TestDirectory,
    workspace: PathBuf,
    store: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let directory = TestDirectory::new(name);
        let fixture = Self {
            remote: HttpCache::new(),
            workspace: directory.path.join("workspace"),
            store: directory.path.join("store"),
            directory,
        };
        fs::create_dir_all(&fixture.store).unwrap();
        fixture.write(
            "Cargo.toml",
            &format!(
                "[package]\nname = {:?}\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [[bin]]\nname = \"app\"\npath = \"src/main.rs\"\ntest = false\n\
             [dependencies]\ndependency = {{ path = \"dependency\" }}\n\
             compiler_gate = {{ path = \"compiler-gate\" }}\n",
                fixture.directory.package_name
            ),
        );
        fixture.write(
            "dependency/Cargo.toml",
            "[package]\nname = \"dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        );
        fixture.write("dependency/src/lib.rs", "pub fn value() -> u32 { 42 }\n");
        fixture.write(
            "compiler-gate/Cargo.toml",
            "[package]\nname = \"compiler_gate\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [lib]\nproc-macro = true\n",
        );
        fixture.write(
            "compiler-gate/src/lib.rs",
            r#"
extern crate proc_macro;
#[proc_macro]
pub fn wait(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let release = input.to_string();
    let scratch = std::path::PathBuf::from(std::env::var_os("TMPDIR").unwrap());
    std::fs::write(scratch.join("remote-compiler-running"), "running").unwrap();
    let start = std::time::Instant::now();
    while !std::path::Path::new(release.trim_matches('"')).exists() {
        assert!(start.elapsed() < std::time::Duration::from_secs(30), "compiler was not released");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    proc_macro::TokenStream::new()
}
"#,
        );
        fixture.write("src/main.rs", &format!(
            "compiler_gate::wait!({:?});\nfn main() {{ println!(\"{{}}\", dependency::value()); }}\n",
            fixture.release_path().to_str().unwrap()));
        fixture.write(
            "corgi.toml",
            &format!("[cache]\nread-url = \"{}/read\"\n", fixture.remote.url),
        );
        fixture.write(
            "remote-curl.py",
            r#"#!/usr/bin/env python3
import os, sys
if any(arg.startswith("http://127.0.0.1:") for arg in sys.argv[1:]):
    os.execvp("curl", ["curl", "-q", *sys.argv[1:]])
tool = os.environ["CORGI_TEST_DOWNLOAD_CURL"]
os.execvp(tool, [tool, *sys.argv[1:]])
"#,
        );
        fs::set_permissions(
            fixture.workspace.join("remote-curl.py"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fixture.release_compiler();
        fixture
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.workspace.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn release_path(&self) -> PathBuf {
        self.store.join("compiler-release")
    }

    fn release_compiler(&self) {
        if fs::symlink_metadata(self.release_path()).is_ok() {
            fs::remove_file(self.release_path()).unwrap();
        }
        fs::write(self.release_path(), "ready").unwrap();
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = corgi_command();
        let download_curl = command
            .get_envs()
            .find(|(key, _)| *key == "CORGI_CURL")
            .and_then(|(_, value)| value)
            .unwrap()
            .to_owned();
        command
            .current_dir(&self.workspace)
            .args(arguments)
            .env("CORGI_STORE", &self.store)
            .env("CORGI_CURL", self.workspace.join("remote-curl.py"))
            .env("CORGI_TEST_DOWNLOAD_CURL", download_curl)
            .env("NO_PROXY", "127.0.0.1")
            .env("no_proxy", "127.0.0.1");
        for variable in [
            "CORGI_R2_WRITE_URL",
            "CORGI_R2_ACCESS_KEY_ID",
            "CORGI_R2_SECRET_ACCESS_KEY",
        ] {
            command.env_remove(variable);
        }
        if arguments.first() == Some(&"cache") {
            command
                .env("CORGI_R2_WRITE_URL", format!("{}/bucket", self.remote.url))
                .env("CORGI_R2_ACCESS_KEY_ID", "fixture-access")
                .env("CORGI_R2_SECRET_ACCESS_KEY", "fixture-secret");
        }
        command
    }

    fn run(&self, arguments: &[&str]) -> String {
        let output = self.command(arguments).output().unwrap();
        assert_success(&output, &format!("corgi {}", arguments.join(" ")));
        String::from_utf8(output.stderr).unwrap()
    }

    fn clear_results(&self) {
        // Keep toolchains and source downloads, but no compilation or test results.
        for name in [
            "cache",
            "manifests",
            "pool",
            "remote-actions",
            "outdirs",
            "incr",
        ] {
            let path = self.store.join(name);
            if path.exists() {
                fs::remove_dir_all(path).unwrap();
            }
        }
        let target = self.workspace.join("target");
        if target.exists() {
            fs::remove_dir_all(target).unwrap();
        }
    }

    fn report(&self) -> Value {
        report_for_workspace(&self.store, &self.workspace)
    }

    fn assert_binary_output(&self, expected: &str) {
        let output = Command::new(
            self.workspace
                .join("target/debug")
                .join(executable_name("app")),
        )
        .output()
        .unwrap();
        assert_success(&output, "run cached binary");
        assert_eq!(output.stdout, expected.as_bytes());
    }
}

#[derive(Default)]
struct Objects {
    bytes: BTreeMap<String, Vec<u8>>,
    requests: Vec<(String, String)>,
    compiler_gate: Option<(String, PathBuf)>,
}

struct HttpCache {
    url: String,
    objects: Arc<Mutex<Objects>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl HttpCache {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let objects = Arc::new(Mutex::new(Objects::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker = {
            let objects = objects.clone();
            let stopped = stopped.clone();
            thread::spawn(move || {
                let mut clients = Vec::new();
                while !stopped.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let objects = objects.clone();
                            let stopped = stopped.clone();
                            clients.push(thread::spawn(move || {
                                // Cancellation can close a socket before its reply.
                                let _ = serve(stream, objects, stopped);
                            }));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                }
                for client in clients {
                    client.join().unwrap();
                }
            })
        };
        Self {
            url,
            objects,
            stopped,
            worker: Some(worker),
        }
    }

    fn record(&self, matches: impl Fn(&Value) -> bool) -> (String, Value) {
        self.objects
            .lock()
            .unwrap()
            .bytes
            .iter()
            .filter(|(path, _)| path.contains("/actions/"))
            .map(|(path, bytes)| (path.clone(), serde_json::from_slice(bytes).unwrap()))
            .find(|(_, record)| matches(record))
            .expect("published action")
    }

    fn retain_record(&self, path: &str, record: &Value) {
        let mut keys = vec![path.to_owned()];
        keys.extend(
            record["blobs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|hash| format!("corgi/v2/blobs/{}", hash.as_str().unwrap())),
        );
        self.objects
            .lock()
            .unwrap()
            .bytes
            .retain(|key, _| keys.contains(key));
    }
}

impl Drop for HttpCache {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn serve(
    mut stream: TcpStream,
    objects: Arc<Mutex<Objects>>,
    stopped: Arc<AtomicBool>,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut words = line.split_whitespace();
    let method = words.next().unwrap().to_owned();
    let url = words.next().unwrap().to_owned();
    let public = url.starts_with("/read/");
    let path = url
        .strip_prefix(if public { "/read/" } else { "/bucket/" })
        .unwrap()
        .to_owned();
    let mut length = 0;
    let mut authenticated = false;
    let mut create_only = false;
    let mut encoding = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap(),
                "content-encoding" => encoding = value.trim().to_owned(),
                "authorization" => authenticated = true,
                "if-none-match" => create_only = value.trim() == "*",
                "expect" => stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?,
                _ => {}
            }
        }
    }
    assert_eq!(
        authenticated, !public,
        "reads are public; uploads use a separate authenticated URL"
    );
    if public {
        assert_eq!(method, "GET");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let gate = {
        let mut state = objects.lock().unwrap();
        state.requests.push((method.clone(), path.clone()));
        state
            .compiler_gate
            .clone()
            .filter(|(key, _)| public && key == &path)
    };
    if let Some((_, store)) = gate {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !compiler_running(&store) {
            if stopped.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return Err(std::io::ErrorKind::TimedOut.into());
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    let blob = path.contains("/blobs/");
    let (status, response) = {
        let mut state = objects.lock().unwrap();
        match method.as_str() {
            "GET" | "HEAD" => state
                .bytes
                .get(&path)
                .map(|bytes| (200, bytes.clone()))
                .unwrap_or((404, Vec::new())),
            "PUT" => {
                if blob {
                    assert_eq!(encoding, "gzip");
                    assert!(body.starts_with(&[0x1f, 0x8b]));
                } else {
                    assert!(encoding.is_empty());
                }
                if path.contains("/actions/") {
                    assert!(create_only);
                    let record: Value = serde_json::from_slice(&body).unwrap();
                    for hash in record["blobs"].as_array().unwrap() {
                        assert!(
                            state.bytes.contains_key(&format!(
                                "corgi/v2/blobs/{}",
                                hash.as_str().unwrap()
                            )),
                            "blobs must be published before records"
                        );
                    }
                }
                if create_only && state.bytes.contains_key(&path) {
                    (412, Vec::new())
                } else {
                    state.bytes.insert(path, body);
                    (200, Vec::new())
                }
            }
            _ => panic!("unexpected method {method}"),
        }
    };
    write!(
        stream,
        "HTTP/1.1 {status} Response\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
        response.len(),
        if blob && status == 200 {
            "Content-Encoding: gzip\r\n"
        } else {
            ""
        }
    )?;
    if method != "HEAD" {
        stream.write_all(&response)?;
    }
    Ok(())
}

fn compiler_running(store: &Path) -> bool {
    fn visit(path: &Path, depth: usize) -> bool {
        path.join("remote-compiler-running").is_file()
            || (depth > 0
                && fs::read_dir(path)
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .any(|entry| {
                        entry.file_type().is_ok_and(|kind| kind.is_dir())
                            && visit(&entry.path(), depth - 1)
                    }))
    }
    visit(&store.join("tmp"), 2) || visit(&store.join("incr"), 2)
}
