//! Loopback-only remote-cache contract tests, included by end_to_end.rs.
use super::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{atomic::AtomicBool, Arc, Mutex},
    thread::{self, JoinHandle},
    time::Instant,
};

const PREFIX: &str = "/bucket/corgi/v1/";

#[derive(Default)]
struct Objects {
    bytes: BTreeMap<String, Vec<u8>>,
    requests: Vec<(String, String)>,
    fail_reads: bool,
    fail_writes: bool,
    // Hold these records until the build script has actually started. This
    // establishes a scheduler state, rather than guessing with a network delay.
    gated: BTreeSet<String>,
    marker_root: Option<PathBuf>,
    gates_released: usize,
}

struct HttpCache {
    address: std::net::SocketAddr,
    objects: Arc<Mutex<Objects>>,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl HttpCache {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
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
                            clients.push(thread::spawn(move || serve(stream, objects, stopped)));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("accept remote cache request: {error}"),
                    }
                }
                for client in clients {
                    client.join().unwrap();
                }
            })
        };
        Self {
            address,
            objects,
            stopped,
            worker: Some(worker),
        }
    }

    fn configure(&self, workspace: &Path) {
        fs::write(
            workspace.join("corgi.toml"),
            format!(
                "[cache]\nread-url = \"http://{0}/bucket\"\nendpoint = \"http://{0}\"\nbucket = \"bucket\"\n",
                self.address,
            ),
        )
        .unwrap();
    }

    fn records(&self) -> Vec<(String, serde_json::Value)> {
        self.objects
            .lock()
            .unwrap()
            .bytes
            .iter()
            .filter(|(path, _)| path.starts_with(&format!("{PREFIX}actions/")))
            .map(|(path, bytes)| (path.clone(), serde_json::from_slice(bytes).unwrap()))
            .collect()
    }

    fn reset_requests(&self) {
        self.objects.lock().unwrap().requests.clear();
    }

    fn requests(&self) -> Vec<(String, String)> {
        self.objects.lock().unwrap().requests.clone()
    }
}

impl Drop for HttpCache {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn serve(mut stream: TcpStream, objects: Arc<Mutex<Objects>>, stopped: Arc<AtomicBool>) {
    // BSD sockets inherit O_NONBLOCK from the listening socket.
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut words = line.split_whitespace();
    let method = words.next().unwrap().to_owned();
    let path = words.next().unwrap().to_owned();
    let mut length = 0;
    let mut expect_continue = false;
    let mut if_none_match = None;
    let mut if_match = None;
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().unwrap();
            }
            if name.eq_ignore_ascii_case("expect") {
                expect_continue = value.trim().eq_ignore_ascii_case("100-continue");
            }
            if name.eq_ignore_ascii_case("if-none-match") {
                if_none_match = Some(value.trim().to_owned());
            }
            if name.eq_ignore_ascii_case("if-match") {
                if_match = Some(value.trim().to_owned());
            }
        }
    }
    if expect_continue && stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").is_err() {
        return;
    }
    let mut body = vec![0; length];
    if let Err(error) = reader.read_exact(&mut body) {
        eprintln!("remote fixture {method} {path}: reading {length} bytes failed: {error}");
        return;
    }
    let gate = {
        let mut state = objects.lock().unwrap();
        state.requests.push((method.clone(), path.clone()));
        if method == "GET" && state.gated.contains(&path) {
            state.marker_root.clone()
        } else {
            None
        }
    };
    if let Some(root) = gate {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !has_running_marker(&root) {
            if stopped.load(Ordering::Relaxed) || Instant::now() >= deadline {
                let _ = stream.write_all(
                    b"HTTP/1.1 504 Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        objects.lock().unwrap().gates_released += 1;
    }
    let (status, response) = {
        let mut state = objects.lock().unwrap();
        match method.as_str() {
            "GET" | "HEAD" if state.fail_reads => (503, Vec::new()),
            "PUT" if state.fail_writes => (503, Vec::new()),
            "GET" | "HEAD" => match state.bytes.get(&path) {
                Some(bytes) => (200, bytes.clone()),
                None => (404, Vec::new()),
            },
            "PUT" if if_none_match.as_deref() == Some("*") && state.bytes.contains_key(&path) => {
                (412, Vec::new())
            }
            "PUT"
                if if_match.as_ref().is_some_and(|tag| {
                    state.bytes.get(&path).map(|bytes| etag(bytes)).as_ref() != Some(tag)
                }) =>
            {
                (412, Vec::new())
            }
            "PUT" => {
                state.bytes.insert(path.clone(), body);
                (200, Vec::new())
            }
            _ => (405, Vec::new()),
        }
    };
    // Cancellation may close the connection after an action starts locally.
    let _ = write!(
        stream,
        "HTTP/1.1 {status} Response\r\nContent-Length: {}\r\nETag: {}\r\nConnection: close\r\n\r\n",
        response.len(),
        etag(&response)
    );
    if method != "HEAD" {
        let _ = stream.write_all(&response);
    }
}

fn etag(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("\"{hash}\"")
}

fn has_running_marker(root: &Path) -> bool {
    fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .any(|entry| entry.path().join("out/remote-test-running").is_file())
}

fn remote_command(workspace: &Path, store: &Path, arguments: &[&str]) -> Command {
    let mut command = corgi_command();
    let download_curl = command
        .get_envs()
        .find(|(key, _)| *key == "CORGI_CURL")
        .and_then(|(_, value)| value)
        .unwrap()
        .to_owned();
    command
        .current_dir(workspace)
        .args(arguments)
        .env("CORGI_STORE", store)
        // Keep the suite's toolchain-download fixture, but never cache the
        // loopback requests whose presence/absence these tests assert.
        .env("CORGI_CURL", store.parent().unwrap().join("remote-curl.py"))
        .env("CORGI_TEST_DOWNLOAD_CURL", download_curl)
        .env("NO_PROXY", "127.0.0.1")
        .env("no_proxy", "127.0.0.1")
        .env("CORGI_R2_ACCESS_KEY_ID", "test-access-key")
        .env("CORGI_R2_SECRET_ACCESS_KEY", "test-secret-key");
    command
}

fn remote_run(workspace: &Path, store: &Path, arguments: &[&str]) -> Output {
    let output = remote_command(workspace, store, arguments)
        .output()
        .unwrap();
    assert_success(&output, &format!("corgi {}", arguments.join(" ")));
    output
}

fn remote_fixture(directory: &TestDirectory, server: &HttpCache, slow: bool) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let curl = directory.path.join("remote-curl.py");
    fs::write(
        &curl,
        r#"#!/usr/bin/env python3
import os
import sys
args = sys.argv[1:]
if any(arg.startswith("http://127.0.0.1:") for arg in args):
    os.execvp("curl", ["curl", "-q", *args])
tool = os.environ["CORGI_TEST_DOWNLOAD_CURL"]
os.execvp(tool, [tool, *args])
"#,
    )
    .unwrap();
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o755)).unwrap();
    let workspace = directory.path.join("checkout-a");
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::create_dir_all(workspace.join("dependency/src")).unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = {:?}\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [dependencies]\nremote_dependency = {{ path = \"dependency\" }}\n",
            directory.package_name,
        ),
    )
    .unwrap();
    fs::write(
        workspace.join("dependency/Cargo.toml"),
        "[package]\nname = \"remote_dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        workspace.join("dependency/src/lib.rs"),
        "pub fn value() -> u32 { 42 }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("src/main.rs"),
        "fn main() { println!(\"{}\", remote_dependency::value()); }\n",
    )
    .unwrap();
    if slow {
        fs::write(
            workspace.join("build.rs"),
            r#"fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    std::fs::write(out.join("remote-test-running"), "started").unwrap();
    std::thread::sleep(std::time::Duration::from_secs(3));
    std::fs::write(out.join("remote-test-finished"), "finished").unwrap();
    println!("cargo::rustc-env=REMOTE_SCRIPT_FINISHED=yes");
}
"#,
        )
        .unwrap();
    }
    server.configure(&workspace);
    workspace
}

fn clear_local_results(directory: &TestDirectory, store: &Path, workspace: &Path) {
    assert!(store.starts_with(&directory.path));
    assert!(workspace.starts_with(&directory.path));
    // Never delete tools, sources, or any externally supplied toolchain.
    for name in ["cache", "manifests", "pool", "remote-actions", "outdirs"] {
        let path = store.join(name);
        if path.exists() {
            fs::remove_dir_all(&path).unwrap();
        }
        fs::create_dir_all(path).unwrap();
    }
    if workspace.join("target").exists() {
        fs::remove_dir_all(workspace.join("target")).unwrap();
    }
}

fn root_record(server: &HttpCache, package: &str) -> (String, serde_json::Value) {
    server
        .records()
        .into_iter()
        .find(|(_, record)| {
            record["spec"]["target"]["name"] == package && record["spec"]["kind"] == "compile"
        })
        .expect("uploaded binary action")
}

#[test]
fn remote_cache_publishes_local_hits_and_dependency_closure_and_repairs_blobs() {
    let directory = TestDirectory::new("remote-publish");
    let server = HttpCache::new();
    let workspace = remote_fixture(&directory, &server, true);
    let store = directory.path.join("store");
    remote_run(&workspace, &store, &["build"]);
    assert!(server.requests().iter().all(|(method, _)| method == "GET"));
    assert!(
        server.records().is_empty(),
        "ordinary build must not upload"
    );
    server.reset_requests();
    remote_run(&workspace, &store, &["cache", "build"]);
    assert_unit_cache(
        &report_for_workspace(&store, &workspace),
        &directory.package_name,
        "compile",
        &directory.package_name,
        "hit",
    );
    let records = server.records();
    assert!(
        records
            .iter()
            .any(|(_, r)| r["spec"]["package"][0] == "remote_dependency"),
        "publishing a local root hit must include pruned dependency results"
    );
    assert!(
        records
            .iter()
            .any(|(_, r)| r["spec"]["action"] == "build_script_run"),
        "publishing a local root hit must include its build script's result and OUT_DIR archive"
    );
    let (_, root) = root_record(&server, &directory.package_name);
    let hash = root["blobs"][0].as_str().expect("binary blob");
    let blob = format!("{PREFIX}blobs/{hash}");
    let original_records = {
        let mut state = server.objects.lock().unwrap();
        let bytes = state.bytes.remove(&blob).expect("uploaded blob");
        assert_eq!(etag(&bytes), format!("\"{hash}\""));
        state
            .bytes
            .iter()
            .filter(|(path, _)| path.contains("/actions/"))
            .map(|(path, bytes)| (path.clone(), bytes.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    server.reset_requests();
    remote_run(&workspace, &store, &["cache", "build"]);
    let state = server.objects.lock().unwrap();
    assert!(
        state.bytes.contains_key(&blob),
        "existing action must not hide a missing blob"
    );
    for (path, bytes) in original_records {
        assert_eq!(state.bytes[&path], bytes, "published records are immutable");
        assert!(
            !state.requests.contains(&("PUT".into(), path)),
            "do not overwrite an existing record"
        );
    }
    assert!(state.requests.contains(&("PUT".into(), blob)));
}

#[test]
fn remote_cache_keys_follow_permitted_inputs_without_broadening_local_keys() {
    let directory = TestDirectory::new("remote-inputs");
    let server = HttpCache::new();
    let workspace = remote_fixture(&directory, &server, false);
    let store = directory.path.join("store");
    fs::write(
        workspace.join("src/unused.rs"),
        "pub const UNUSED: u32 = 1;\n",
    )
    .unwrap();
    fs::write(workspace.join("declared.txt"), "initial declared input").unwrap();
    let config = workspace.join("corgi.toml");
    let mut contents = fs::read_to_string(&config).unwrap();
    contents.push_str("[extra-inputs]\nremote_dependency = [\"../declared.txt\"]\n");
    fs::write(config, contents).unwrap();
    remote_run(&workspace, &store, &["cache", "build"]);
    let (_, original) = root_record(&server, &directory.package_name);
    let original_records = server.records();

    // These files are not compiler inputs, including the nested fixture output
    // that caused a real CI/local remote-key mismatch.
    let unrelated = [
        workspace.join("notes.md"),
        workspace.join("tests/fixtures/other/target/debug/app"),
    ];
    for path in &unrelated {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
    }
    for content in [Some("new non-input"), Some("changed non-input"), None] {
        for path in &unrelated {
            if let Some(content) = content {
                fs::write(path, content).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
        }
        server.reset_requests();
        remote_run(&workspace, &store, &["cache", "build"]);
        assert_unit_cache(
            &report_for_workspace(&store, &workspace),
            &directory.package_name,
            "compile",
            &directory.package_name,
            "hit",
        );
        assert_eq!(server.records(), original_records);
        assert!(
            server.requests().iter().all(|(method, _)| method != "PUT"),
            "non-input changes must not publish new remote results"
        );
    }

    // Changing an existing unread Rust file preserves the local read-set key,
    // but changes the remote key without learning another read set.
    fs::write(
        workspace.join("src/unused.rs"),
        "pub const UNUSED: u32 = 123;\n",
    )
    .unwrap();
    server.reset_requests();
    remote_run(&workspace, &store, &["cache", "build"]);
    assert_unit_cache(
        &report_for_workspace(&store, &workspace),
        &directory.package_name,
        "compile",
        &directory.package_name,
        "hit",
    );
    let published_root = || {
        server
            .records()
            .into_iter()
            .find(|(path, record)| {
                record["spec"]["target"]["name"] == directory.package_name
                    && server.requests().contains(&("PUT".into(), path.clone()))
            })
            .expect("new root record published")
            .1
    };
    let unread_edit = published_root();
    assert_eq!(unread_edit["action_key"], original["action_key"]);
    assert_ne!(unread_edit["remote_key"], original["remote_key"]);

    // Declared inputs outside a dependency's directory are still keyed, and
    // that dependency's remote identity must propagate to its consumer.
    fs::write(workspace.join("declared.txt"), "changed declared input").unwrap();
    server.reset_requests();
    remote_run(&workspace, &store, &["cache", "build"]);
    let declared_edit = published_root();
    assert_ne!(declared_edit["action_key"], unread_edit["action_key"]);
    assert_ne!(declared_edit["remote_key"], unread_edit["remote_key"]);

    // Adding a source path retains the existing local source-layout guard.
    fs::write(
        workspace.join("src/another.rs"),
        "pub const ANOTHER: u32 = 0;\n",
    )
    .unwrap();
    server.reset_requests();
    remote_run(&workspace, &store, &["cache", "build"]);
    let added_source = published_root();
    assert_ne!(added_source["action_key"], declared_edit["action_key"]);
    assert_ne!(added_source["remote_key"], declared_edit["remote_key"]);
    assert!(
        added_source["spec"]["read_set"]
            .as_array()
            .unwrap()
            .iter()
            .all(|file| { file[0] != "src/unused.rs" && file[0] != "src/another.rs" }),
        "precise read sets must not grow to include unread sources"
    );
}

#[test]
fn remote_cache_reuses_another_checkout_in_the_same_store_then_needs_no_gets() {
    let directory = TestDirectory::new("remote-relocate");
    let server = HttpCache::new();
    let workspace = remote_fixture(&directory, &server, true);
    let store = directory.path.join("store");
    remote_run(&workspace, &store, &["cache", "build"]);
    let (path, record) = root_record(&server, &directory.package_name);
    assert!(!record["spec"]["read_set"].as_array().unwrap().is_empty());
    let other = directory.path.join("checkout-b");
    copy_directory(&workspace, &other);
    clear_local_results(&directory, &store, &workspace);
    {
        let mut state = server.objects.lock().unwrap();
        // Keep only the root and its blobs: a remote root is useful without
        // downloading its entire dependency closure.
        state
            .bytes
            .retain(|key, _| !key.contains("/actions/") || key == &path);
        state.gated.insert(path.clone());
        state.marker_root = Some(store.join("outdirs"));
    }
    server.reset_requests();
    // The same CORGI_STORE is intentional: store paths are in remote keys and
    // may also be embedded in artifacts. Only the checkout location changes.
    remote_run(&other, &store, &["build"]);
    assert!(server.objects.lock().unwrap().gates_released > 0);
    assert!(server.requests().contains(&("GET".into(), path)));
    let report = report_for_workspace(&store, &other);
    assert_unit_cache(
        &report,
        &directory.package_name,
        "compile",
        &directory.package_name,
        "hit",
    );
    assert_eq!(
        report_unit(&report, &directory.package_name, "compile")["key"]["hash"],
        record["action_key"]
    );
    let alias = store
        .join("remote-actions/corgi/v1")
        .join(format!("{}.json", record["remote_key"].as_str().unwrap()));
    let imported: serde_json::Value = serde_json::from_slice(&fs::read(&alias).unwrap()).unwrap();
    assert_eq!(
        imported["spec"], record["spec"],
        "retain precise producer keys and read set"
    );
    server.reset_requests();
    remote_run(&other, &store, &["build"]);
    assert!(
        server.requests().is_empty(),
        "an imported local hit must need zero GETs"
    );
    let local: serde_json::Value = serde_json::from_slice(&fs::read(alias).unwrap()).unwrap();
    assert_eq!(local["spec"], record["spec"]);
    assert_eq!(local["action_key"], record["action_key"]);
    let output = Command::new(
        other
            .join("target/debug")
            .join(executable_name(&directory.package_name)),
    )
    .output()
    .unwrap();
    assert_success(&output, "run remotely restored binary");
    assert_eq!(output.stdout, b"42\n");
}

#[test]
fn remote_cache_accepts_a_queued_root_but_never_replaces_a_running_build_script() {
    let directory = TestDirectory::new("remote-running");
    let server = HttpCache::new();
    let workspace = remote_fixture(&directory, &server, true);
    let store = directory.path.join("store");
    remote_run(&workspace, &store, &["cache", "build"]);
    let (root, _) = root_record(&server, &directory.package_name);
    let script = server
        .records()
        .into_iter()
        .find(|(_, record)| record["spec"]["action"] == "build_script_run")
        .expect("uploaded build-script run")
        .0;
    clear_local_results(&directory, &store, &workspace);
    {
        let mut state = server.objects.lock().unwrap();
        state
            .bytes
            .retain(|path, _| !path.contains("/actions/") || path == &root || path == &script);
        state.gated.extend([root, script]);
        state.marker_root = Some(store.join("outdirs"));
    }
    remote_run(&workspace, &store, &["build"]);
    assert_eq!(
        server.objects.lock().unwrap().gates_released,
        2,
        "both responses arrived after the script started"
    );
    let report = report_for_workspace(&store, &workspace);
    assert_unit_cache(
        &report,
        &directory.package_name,
        "compile",
        &directory.package_name,
        "hit",
    );
    assert_unit_cache(
        &report,
        &directory.package_name,
        "run_build_script",
        "build-script-build",
        "miss",
    );
}

#[test]
fn remote_cache_intermediate_hit_waits_for_transitive_metadata_needed_by_local_consumer() {
    let directory = TestDirectory::new("remote-intermediate");
    let server = HttpCache::new();
    let workspace = remote_fixture(&directory, &server, true);
    let store = directory.path.join("store");
    // root -> intermediate -> dependency, with a slow script on dependency.
    // Importing intermediate must not prune dependency: rustc still needs its
    // metadata when compiling the local root.
    fs::rename(
        workspace.join("build.rs"),
        workspace.join("dependency/build.rs"),
    )
    .unwrap();
    fs::create_dir_all(workspace.join("intermediate/src")).unwrap();
    fs::write(
        workspace.join("intermediate/Cargo.toml"),
        "[package]\nname = \"remote_intermediate\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [dependencies]\nremote_dependency = { path = \"../dependency\" }\n",
    )
    .unwrap();
    fs::write(
        workspace.join("intermediate/src/lib.rs"),
        "pub fn value() -> u32 { remote_dependency::value() }\n",
    )
    .unwrap();
    fs::write(workspace.join("Cargo.toml"), format!(
        "[package]\nname = {:?}\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [dependencies]\nremote_dependency = {{ package = \"remote_intermediate\", path = \"intermediate\" }}\n",
        directory.package_name,
    )).unwrap();
    remote_run(&workspace, &store, &["cache", "build"]);
    let intermediate = server
        .records()
        .into_iter()
        .find(|(_, record)| record["spec"]["package"][0] == "remote_intermediate")
        .expect("uploaded intermediate library")
        .0;
    clear_local_results(&directory, &store, &workspace);
    {
        let mut state = server.objects.lock().unwrap();
        state
            .bytes
            .retain(|path, _| !path.contains("/actions/") || path == &intermediate);
        state.gated.insert(intermediate);
        state.marker_root = Some(store.join("outdirs"));
    }
    remote_run(&workspace, &store, &["build"]);
    assert_eq!(server.objects.lock().unwrap().gates_released, 1);
    let report = report_for_workspace(&store, &workspace);
    assert_unit_cache(
        &report,
        "remote_intermediate",
        "compile",
        "remote_intermediate",
        "hit",
    );
    assert_unit_cache(
        &report,
        "remote_dependency",
        "compile",
        "remote_dependency",
        "miss",
    );
    assert_unit_cache(
        &report,
        &directory.package_name,
        "compile",
        &directory.package_name,
        "miss",
    );
    let output = Command::new(
        workspace
            .join("target/debug")
            .join(executable_name(&directory.package_name)),
    )
    .output()
    .unwrap();
    assert_success(
        &output,
        "run binary linked through an imported intermediate",
    );
    assert_eq!(output.stdout, b"42\n");
}

#[test]
fn remote_cache_upload_failure_is_strict_but_ordinary_read_outage_is_a_miss() {
    let directory = TestDirectory::new("remote-outage");
    let server = HttpCache::new();
    let workspace = remote_fixture(&directory, &server, false);
    let store = directory.path.join("store");
    server.objects.lock().unwrap().fail_reads = true;
    remote_run(&workspace, &store, &["build"]);
    assert_unit_cache(
        &report_for_workspace(&store, &workspace),
        &directory.package_name,
        "compile",
        &directory.package_name,
        "miss",
    );
    assert!(server.requests().iter().any(|(method, _)| method == "GET"));
    assert!(server.requests().iter().all(|(method, _)| method == "GET"));
    {
        let mut state = server.objects.lock().unwrap();
        state.fail_reads = false;
        state.fail_writes = true;
    }
    server.reset_requests();
    let output = remote_command(&workspace, &store, &["cache", "build"])
        .output()
        .unwrap();
    assert_failure(&output, "explicit cache population with failed PUT");
    assert!(server.requests().iter().any(|(method, _)| method == "PUT"));
}

#[test]
fn remote_cache_test_pass_is_uploaded_and_restored_without_running_the_test() {
    let directory = TestDirectory::new("remote-pass");
    let server = HttpCache::new();
    let workspace = remote_fixture(&directory, &server, true);
    let store = directory.path.join("store");
    fs::rename(
        workspace.join("build.rs"),
        workspace.join("dependency/build.rs"),
    )
    .unwrap();
    let marker = directory.path.join("test-ran");
    fs::write(
        workspace.join("src/main.rs"),
        "fn main() {}\n#[test]\nfn passes() {\n\
         std::fs::write(std::env::var(\"CORGI_TEST_MARKER\").unwrap(), \"ran\").unwrap();\n\
         assert!(std::env::var_os(\"CORGI_TEST_FAIL\").is_none());\n}\n",
    )
    .unwrap();
    let invoke = |arguments: &[&str]| {
        let output = remote_command(&workspace, &store, arguments)
            .env("CORGI_TEST_MARKER", &marker)
            .output()
            .unwrap();
        assert_success(&output, "remote test pass");
        output
    };
    invoke(&["test"]);
    let precise_key = report_for_workspace(&store, &workspace)["test_harnesses"][0]["cache"]
        ["pass_key"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(marker.exists());
    fs::remove_file(&marker).unwrap();
    invoke(&["cache", "test"]);
    assert!(
        !marker.exists(),
        "cache test must publish an existing local pass"
    );
    let (pass_path, mut pass) = remote_pass_record(&server);
    assert_eq!(pass["result"]["passed"], true);
    assert_eq!(pass["result"]["test_count"], 1);
    assert_eq!(pass["blobs"], serde_json::json!([]));
    let (harness_path, harness_record) = server
        .records()
        .into_iter()
        .find(|(_, record)| record["action_key"] == pass["harness_action"])
        .expect("uploaded harness action");
    // Producer action metadata is not authority to create a precise local pass.
    pass["harness_action"] = serde_json::json!("f".repeat(64));
    retain_remote_pass(&server, &pass_path, &pass);
    clear_local_results(&directory, &store, &workspace);
    {
        let mut state = server.objects.lock().unwrap();
        state.gated.insert(pass_path.clone());
        state.marker_root = Some(store.join("outdirs"));
    }
    server.reset_requests();
    let output = invoke(&["test"]);
    assert!(
        !marker.exists(),
        "a restored test pass must not execute the harness"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("1 tests passed (cached)"));
    assert_eq!(server.objects.lock().unwrap().gates_released, 1);
    let report = report_for_workspace(&store, &workspace);
    assert_eq!(
        report["test_harnesses"][0]["cache"]["pass_key"],
        pass["remote_key"]
    );
    assert_eq!(report["test_harnesses"][0]["cache"]["result"], "hit");
    assert_unit_cache(
        &report,
        "remote_dependency",
        "run_build_script",
        "build-script-build",
        "miss",
    );
    assert_pruned_target(&report, "remote_dependency", "remote_dependency");
    assert_pruned_target(&report, &directory.package_name, &directory.package_name);
    assert!(
        fs::read_dir(store.join("outdirs")).unwrap().any(|entry| {
            entry
                .unwrap()
                .path()
                .join("out/remote-test-finished")
                .is_file()
        }),
        "the already running script must finish, not be cancelled"
    );
    assert_no_test_artifacts(&workspace);
    assert!(server
        .requests()
        .contains(&("GET".into(), pass_path.clone())));
    assert!(!server
        .requests()
        .contains(&("GET".into(), harness_path.clone())));
    assert_eq!(server.requests(), vec![("GET".into(), pass_path.clone())]);
    let alias = local_action_path(&store, pass["remote_key"].as_str().unwrap());
    let imported: serde_json::Value = serde_json::from_slice(&fs::read(&alias).unwrap()).unwrap();
    assert_eq!(
        imported, pass,
        "persist the validated record at its requested remote key"
    );
    assert!(!local_action_path(&store, &precise_key).exists());
    assert!(
        local_action_records(&store)
            .iter()
            .all(|record| record.get("passed").is_none()),
        "untrusted harness_action must never manufacture a precise test pass"
    );

    server.reset_requests();
    invoke(&["test"]);
    assert!(
        server.requests().is_empty(),
        "warm remote pass alias must need zero GETs"
    );
    assert!(!marker.exists());
    assert_no_test_artifacts(&workspace);
    let report = report_for_workspace(&store, &workspace);
    assert_eq!(
        report["test_harnesses"][0]["cache"]["pass_key"],
        pass["remote_key"]
    );
    assert!(report["units"]
        .as_array()
        .unwrap()
        .iter()
        .all(|unit| unit["outcome"]["status"] == "skipped"));

    // A pass-only root remains publishable with missing artifact blobs or a
    // deserializable but invalid alias, without any precise manifest.
    server.objects.lock().unwrap().gated.clear();
    clear_local_results(&directory, &store, &workspace);
    fs::create_dir_all(alias.parent().unwrap()).unwrap();
    fs::write(&alias, serde_json::to_vec(&pass).unwrap()).unwrap();
    let stale_alias = store
        .join("remote-actions/corgi/v1")
        .join(harness_path.rsplit('/').next().unwrap());
    fs::create_dir_all(stale_alias.parent().unwrap()).unwrap();
    let mut invalid_record = harness_record.clone();
    invalid_record["action_key"] = serde_json::json!("0".repeat(64));
    for record in [harness_record, invalid_record] {
        fs::write(&stale_alias, serde_json::to_vec(&record).unwrap()).unwrap();
        server.objects.lock().unwrap().bytes.clear();
        invoke(&["cache", "test"]);
        assert!(!marker.exists());
        assert_no_test_artifacts(&workspace);
        assert_eq!(
            remote_pass_record(&server),
            (pass_path.clone(), pass.clone())
        );
        assert_eq!(server.objects.lock().unwrap().bytes.len(), 1);
    }

    // A failed forced rerun must invalidate the imported REMOTE alias, not
    // republish it or let the next invocation claim a cached success.
    server.objects.lock().unwrap().bytes.clear();
    for arguments in [&["cache", "test", "--force"][..], &["cache", "test"][..]] {
        let output = remote_command(&workspace, &store, arguments)
            .env("CORGI_TEST_MARKER", &marker)
            .env("CORGI_TEST_FAIL", "1")
            .output()
            .unwrap();
        assert_failure(
            &output,
            "a failed test must not retain or publish an old pass",
        );
        assert!(marker.exists(), "the failing test must actually execute");
        fs::remove_file(&marker).unwrap();
        assert!(
            !alias.exists(),
            "failed execution must remove the remote pass alias"
        );
        assert!(!server
            .objects
            .lock()
            .unwrap()
            .bytes
            .contains_key(&pass_path));
    }
}

#[test]
fn remote_cache_test_pass_shortcut_is_bypassed_by_noncanonical_invocations() {
    let directory = TestDirectory::new("remote-pass-bypass");
    let server = HttpCache::new();
    let workspace = remote_fixture(&directory, &server, false);
    let store = directory.path.join("store");
    let marker = directory.path.join("test-ran");
    fs::write(
        workspace.join("src/main.rs"),
        "fn main() {}\n#[test] fn passes() { std::fs::write(std::env::var(\"CORGI_TEST_MARKER\").unwrap(), \"ran\").unwrap(); }\n",
    ).unwrap();
    let invoke = |arguments: &[&str]| {
        let output = remote_command(&workspace, &store, arguments)
            .env("CORGI_TEST_MARKER", &marker)
            .output()
            .unwrap();
        assert_success(&output, "noncanonical test invocation");
    };
    invoke(&["cache", "test"]);
    let (path, pass) = remote_pass_record(&server);
    retain_remote_pass(&server, &path, &pass);
    for arguments in [
        &["test", "--force"][..],
        &["test", "^passes$"][..],
        &["test", "--", "--test-threads=1"][..],
        &["test", "--no-run"][..],
    ] {
        clear_local_results(&directory, &store, &workspace);
        fs::remove_file(&marker).unwrap();
        server.reset_requests();
        invoke(arguments);
        let no_run = arguments.contains(&"--no-run");
        assert_eq!(marker.exists(), !no_run, "{arguments:?}");
        let report = report_for_workspace(&store, &workspace);
        assert_unit_cache(
            &report,
            &directory.package_name,
            "compile_test",
            &directory.package_name,
            "miss",
        );
        assert!(
            !server.requests().contains(&("GET".into(), path.clone())),
            "must not probe a pass for {arguments:?}"
        );
    }
}

#[test]
fn remote_cache_invalid_test_pass_records_are_misses() {
    let directory = TestDirectory::new("remote-pass-invalid");
    let server = HttpCache::new();
    let workspace = remote_fixture(&directory, &server, false);
    let store = directory.path.join("store");
    let marker = directory.path.join("test-ran");
    fs::write(
        workspace.join("src/main.rs"),
        "fn main() {}\n#[test] fn passes() { std::fs::write(std::env::var(\"CORGI_TEST_MARKER\").unwrap(), \"ran\").unwrap(); }\n",
    ).unwrap();
    let invoke = |arguments: &[&str]| {
        let output = remote_command(&workspace, &store, arguments)
            .env("CORGI_TEST_MARKER", &marker)
            .output()
            .unwrap();
        assert_success(&output, "invalid remote pass falls back to execution");
    };
    invoke(&["cache", "test"]);
    let (path, pass) = remote_pass_record(&server);
    for (field, value) in [
        ("remote_key", serde_json::json!("0".repeat(64))),
        (
            "result",
            serde_json::json!({"passed": false, "test_count": 1}),
        ),
        ("blobs", serde_json::json!(["a".repeat(64)])),
    ] {
        let mut invalid = pass.clone();
        invalid[field] = value;
        retain_remote_pass(&server, &path, &invalid);
        clear_local_results(&directory, &store, &workspace);
        fs::remove_file(&marker).unwrap();
        server.reset_requests();
        invoke(&["test"]);
        assert!(marker.exists(), "invalid {field} must run tests");
        assert!(server.requests().contains(&("GET".into(), path.clone())));
        let report = report_for_workspace(&store, &workspace);
        assert_eq!(
            report["test_harnesses"][0]["cache"]["result"], "miss",
            "{field}"
        );
    }
}

#[test]
fn remote_cache_test_pass_prunes_runtime_binaries_only_when_no_other_root_needs_them() {
    let directory = TestDirectory::new("remote-pass-shared");
    let server = HttpCache::new();
    let workspace = remote_fixture(&directory, &server, true);
    let store = directory.path.join("store");
    fs::rename(
        workspace.join("build.rs"),
        workspace.join("dependency/build.rs"),
    )
    .unwrap();
    fs::write(
        workspace.join("Cargo.toml"),
        format!(
            "[package]\nname = {:?}\nversion = \"0.1.0\"\nedition = \"2024\"\nautotests = false\n\
         [dependencies]\nremote_dependency = {{ path = \"dependency\" }}\n\
         [[bin]]\nname = \"app\"\npath = \"src/main.rs\"\ntest = false\n\
         [[test]]\nname = \"cached\"\npath = \"tests/cached.rs\"\n\
         [[test]]\nname = \"uncached\"\npath = \"tests/uncached.rs\"\n\
         [[test]]\nname = \"opaque\"\npath = \"tests/opaque.rs\"\nharness = false\n",
            directory.package_name,
        ),
    )
    .unwrap();
    fs::create_dir_all(workspace.join("tests")).unwrap();
    fs::write(workspace.join("tests/cached.rs"),
        "#[test] fn passes() { assert!(std::path::Path::new(env!(\"CARGO_BIN_EXE_app\")).is_file()); std::fs::write(\"cached-ran\", \"ran\").unwrap(); }\n",
    ).unwrap();
    fs::write(workspace.join("tests/uncached.rs"),
        "#[test] fn passes() { assert_eq!(remote_dependency::value(), 42); let output = std::process::Command::new(env!(\"CARGO_BIN_EXE_app\")).output().unwrap(); assert!(output.status.success()); assert_eq!(output.stdout, b\"42\\n\"); std::fs::write(\"uncached-ran\", \"ran\").unwrap(); }\n",
    ).unwrap();
    fs::write(workspace.join("tests/opaque.rs"),
        "fn main() { assert_eq!(remote_dependency::value(), 42); assert!(std::process::Command::new(env!(\"CARGO_BIN_EXE_app\")).status().unwrap().success()); std::fs::write(\"opaque-ran\", \"ran\").unwrap(); }\n",
    ).unwrap();
    remote_run(&workspace, &store, &["cache", "test", "--test", "cached"]);
    let (path, pass) = remote_pass_record(&server);
    retain_remote_pass(&server, &path, &pass);
    for mixed in [false, true] {
        clear_local_results(&directory, &store, &workspace);
        for name in ["cached-ran", "uncached-ran", "opaque-ran"] {
            let marker = workspace.join(name);
            if marker.exists() {
                fs::remove_file(marker).unwrap();
            }
        }
        {
            let mut state = server.objects.lock().unwrap();
            state.gated.insert(path.clone());
            state.marker_root = Some(store.join("outdirs"));
            state.gates_released = 0;
        }
        server.reset_requests();
        let arguments = if mixed {
            &["test"][..]
        } else {
            &["test", "--test", "cached"][..]
        };
        remote_run(&workspace, &store, arguments);
        assert_eq!(server.objects.lock().unwrap().gates_released, 1);
        assert!(!workspace.join("cached-ran").exists());
        assert_eq!(workspace.join("uncached-ran").exists(), mixed);
        assert_eq!(workspace.join("opaque-ran").exists(), mixed);
        let report = report_for_workspace(&store, &workspace);
        assert_pruned_target(&report, &directory.package_name, "cached");
        if mixed {
            assert_unit_cache(
                &report,
                "remote_dependency",
                "compile",
                "remote_dependency",
                "miss",
            );
            assert_unit_cache(&report, &directory.package_name, "compile", "app", "miss");
            for target in ["uncached", "opaque"] {
                assert_unit_cache(
                    &report,
                    &directory.package_name,
                    "compile_test",
                    target,
                    "miss",
                );
            }
        } else {
            assert_pruned_target(&report, "remote_dependency", "remote_dependency");
            assert_pruned_target(&report, &directory.package_name, "app");
            assert!(!workspace
                .join("target/debug")
                .join(executable_name("app"))
                .exists());
            assert_no_test_artifacts(&workspace);
        }
    }
}

fn remote_pass_record(server: &HttpCache) -> (String, serde_json::Value) {
    let mut records = server
        .records()
        .into_iter()
        .filter(|(_, record)| record["harness_action"].is_string());
    let record = records.next().expect("uploaded test pass record");
    assert!(
        records.next().is_none(),
        "fixture should have one test harness"
    );
    record
}

fn retain_remote_pass(server: &HttpCache, path: &str, record: &serde_json::Value) {
    let mut state = server.objects.lock().unwrap();
    state.bytes.clear();
    state
        .bytes
        .insert(path.to_owned(), serde_json::to_vec(record).unwrap());
}

fn local_action_path(store: &Path, key: &str) -> PathBuf {
    store
        .join("cache")
        .join(&key[..2])
        .join(format!("{key}.json"))
}

fn local_action_records(store: &Path) -> Vec<serde_json::Value> {
    fs::read_dir(store.join("cache"))
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().path().is_dir())
        .flat_map(|entry| fs::read_dir(entry.unwrap().path()).unwrap())
        .filter_map(|entry| serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).ok())
        .collect()
}

fn assert_no_test_artifacts(workspace: &Path) {
    let deps = workspace.join("target/debug/deps");
    assert!(
        !deps.exists() || fs::read_dir(deps).unwrap().next().is_none(),
        "a pass-only hit must not export its harness or dependencies"
    );
}

fn assert_pruned_target(report: &serde_json::Value, package: &str, target: &str) {
    let unit = report["units"]
        .as_array()
        .unwrap()
        .iter()
        .find(|unit| unit["package"]["name"] == package && unit["target"]["name"] == target)
        .expect("planned target");
    assert_eq!(unit["outcome"]["status"], "skipped", "{unit:#}");
    assert_eq!(unit["outputs"], serde_json::json!([]), "{unit:#}");
}
