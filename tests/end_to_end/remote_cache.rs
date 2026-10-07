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
    let workspace = remote_fixture(&directory, &server, false);
    let store = directory.path.join("store");
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
    assert!(marker.exists());
    fs::remove_file(&marker).unwrap();
    invoke(&["cache", "test"]);
    assert!(
        !marker.exists(),
        "cache test must publish an existing local pass"
    );
    assert!(
        server.records().iter().any(|(_, record)| {
            record["harness_action"].is_string()
                && record["result"]["passed"] == true
                && record["blobs"].as_array().is_some_and(Vec::is_empty)
        }),
        "publish a pass record as well as the harness"
    );
    clear_local_results(&directory, &store, &workspace);
    let output = invoke(&["test"]);
    assert!(
        !marker.exists(),
        "a restored test pass must not execute the harness"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("1 tests passed (cached)"));

    // Remove only the remote pass, retaining the harness and the imported
    // local pass. A failed forced rerun must invalidate that historical pass,
    // not republish it or let the next invocation claim a cached success.
    let pass_paths: Vec<_> = server
        .records()
        .into_iter()
        .filter(|(_, record)| record["harness_action"].is_string())
        .map(|(path, _)| path)
        .collect();
    for path in &pass_paths {
        server.objects.lock().unwrap().bytes.remove(path);
    }
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
        assert!(pass_paths.iter().all(|path| !server
            .objects
            .lock()
            .unwrap()
            .bytes
            .contains_key(path)));
    }
}
