//! Remote build and test-result workflows against a loopback object store.
use super::*;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    os::unix::fs::{symlink, PermissionsExt},
    process::Stdio,
    sync::{atomic::AtomicBool, Arc, Mutex},
    thread::{self, JoinHandle},
    time::Instant,
};

#[cfg(target_os = "macos")]
#[test]
fn cache_setup_uses_one_password_and_selects_the_project_store() {
    let fixture = Fixture::new("remote-cache-setup");
    let store = fixture.directory.path.join("configured-store");
    let store = store
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap()
        .join("configured-store");
    let original = format!(
        "[\"{PRIVATE_URL}\"]\naccess-key-id = \"old\"\nsecret-access-key = \"old\"\n\
         [\"https://other.example.com/bucket\"]\naccess-key-id = \"keep\"\nsecret-access-key = \"keep\"\n"
    );
    let credentials = fixture
        .directory
        .path
        .join("home/.config/corgi/credentials");
    fs::write(&credentials, original).unwrap();
    fixture.write(
        "corgi.toml",
        &format!(
            "[cache]\nread-url = \"{PRIVATE_URL}\"\nmacos-cache-path = {:?}\n\
             op-access-key-id = \"op://vault/item/access\"\n\
             op-secret-access-key = \"op://vault/item/secret\"\n",
            store.to_str().unwrap(),
        ),
    );
    fixture.write(
        "op",
        "#!/bin/sh\ncase \"$3\" in\n  op://vault/item/access) printf 'fixture-read-access' ;;\n  op://vault/item/secret) printf 'fixture-read-secret' ;;\n  *) exit 1 ;;\nesac\n",
    );
    fs::set_permissions(
        fixture.workspace.join("op"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let path = std::env::join_paths(std::iter::once(fixture.workspace.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();

    let before = fixture
        .command(&["pin"])
        .env_remove("CORGI_STORE")
        .output()
        .unwrap();
    assert_success(&before, "pin without remote cache setup");
    assert!(String::from_utf8_lossy(&before.stderr)
        .contains("Remote caching available: run `corgi cache setup`"));

    let setup = fixture
        .command(&["cache", "setup"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert_success(&setup, "configure remote cache");
    assert!(store.is_dir());
    let saved = fs::read_to_string(&credentials).unwrap();
    assert!(saved.contains("fixture-read-access"));
    assert!(saved.contains("fixture-read-secret"));
    assert!(saved.contains("https://other.example.com/bucket"));
    assert_eq!(
        fs::metadata(&credentials).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!String::from_utf8_lossy(&setup.stderr).contains("fixture-read-secret"));

    let output = fixture
        .command(&["build"])
        .env_remove("CORGI_STORE")
        .output()
        .unwrap();
    assert_success(&output, "build with configured store");
    assert!(store.join("cache").is_dir());
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("Remote caching available"),
        "setup must suppress the hint"
    );

    fixture.write(
        "op",
        "#!/bin/sh\nprintf 'sensitive diagnostic' >&2\nexit 1\n",
    );
    let failed = fixture
        .command(&["cache", "setup"])
        .env("PATH", &path)
        .output()
        .unwrap();
    assert_failure(&failed, "1Password failure must stop setup");
    assert!(
        !String::from_utf8_lossy(&failed.stderr).contains("sensitive diagnostic"),
        "do not forward 1Password output into the build log"
    );
    assert_eq!(fs::read_to_string(credentials).unwrap(), saved);
}

#[test]
fn cache_publishes_dependencies_while_build_or_test_is_running() {
    for command in ["build", "test"] {
        let fixture = Fixture::new(&format!("remote-stream-{command}"));
        if command == "test" {
            fixture.write(
                "tests/smoke.rs",
                "#[test]\nfn smoke() { assert!(std::path::Path::new(env!(\"CARGO_BIN_EXE_app\")).is_file()); }\n",
            );
        }
        fs::remove_file(fixture.release_path()).unwrap();
        let mut child = fixture
            .command(&["cache", command])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Toolchain setup can take longer than the compiler's own gate.
        // Start the publication deadline only once the gated compiler runs.
        let startup_deadline = Instant::now() + Duration::from_secs(120);
        while !compiler_running(&fixture.store) && Instant::now() < startup_deadline {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut published_early = false;
        while Instant::now() < deadline {
            let objects = fixture.remote.objects.lock().unwrap();
            published_early = compiler_running(&fixture.store)
                && objects
                    .bytes
                    .iter()
                    .filter(|(path, _)| path.contains("/actions/"))
                    .any(|(_, bytes)| {
                        serde_json::from_slice::<Value>(bytes).unwrap()["spec"]["target"]["name"]
                            == "dependency"
                    });
            drop(objects);
            if published_early || child.try_wait().unwrap().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        fixture.release_compiler();
        let output = child.wait_with_output().unwrap();
        assert_success(&output, &format!("cache {command}"));
        assert!(
            published_early,
            "cache {command}: dependency must publish before root finishes; requests: {:?}",
            fixture.remote.objects.lock().unwrap().requests
        );
    }
}

#[test]
fn cache_publication_bounds_parallel_uploads() {
    let fixture = Fixture::new("remote-parallel");
    let manifest = fs::read_to_string(fixture.workspace.join("Cargo.toml")).unwrap();
    let mut manifest = manifest;
    for index in 0..5 {
        let name = format!("extra_{index}");
        manifest.push_str(&format!("{name} = {{ path = \"{name}\" }}\n"));
        fixture.write(
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
        );
        fixture.write(
            &format!("{name}/src/lib.rs"),
            "pub fn value() -> u32 { 1 }\n",
        );
    }
    fixture.write("Cargo.toml", &manifest);
    fixture.remote.objects.lock().unwrap().slow_put = true;
    fixture.run(&["cache", "build"]);
    let peak = fixture.remote.objects.lock().unwrap().peak_puts;
    assert!(
        (2..=4).contains(&peak),
        "expected 2–4 concurrent PUTs, got {peak}"
    );
}

#[test]
fn remote_build_cache_lifecycle() {
    let mixed = Fixture::new("remote-incremental");
    let manifest = fs::read_to_string(mixed.workspace.join("Cargo.toml")).unwrap();
    mixed.write(
        "Cargo.toml",
        &format!("{manifest}middle = {{ path = \"middle\" }}\n"),
    );
    mixed.write(
        "middle/Cargo.toml",
        "[package]\nname = \"middle\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [dependencies]\ndependency = { path = \"../dependency\" }\n",
    );
    mixed.write(
        "middle/src/lib.rs",
        "pub fn value() -> u32 { dependency::value() }\n",
    );
    mixed.write(
        "dependency/Cargo.toml",
        "[package]\nname = \"dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
         [dependencies]\ncompiler_gate = { path = \"../compiler-gate\" }\n",
    );
    mixed.write(
        "dependency/src/lib.rs",
        &format!(
            "compiler_gate::wait!({:?});\npub fn value() -> u32 {{ 42 }}\n",
            mixed.release_path()
        ),
    );
    mixed.write(
        "src/main.rs",
        "fn main() { println!(\"{}\", middle::value()); }\n",
    );
    mixed.run(&["cache", "build", "--no-incremental"]);
    let (path, non_incremental) = mixed
        .remote
        .record(|record| record["spec"]["target"]["name"] == "middle");
    mixed.remote.retain_record(&path, &non_incremental);
    mixed.clear_results();
    mixed.run(&["build"]);
    assert_unit_cache(&mixed.report(), "middle", "compile", "middle", "miss");
    mixed.assert_binary_output("42\n");
    mixed.run(&["cache", "build"]);
    let (path, incremental) = mixed.remote.record(|record| {
        record["spec"]["target"]["name"] == "middle" && record["spec"]["incremental"] == true
    });
    assert_ne!(incremental["remote_key"], non_incremental["remote_key"]);
    assert_ne!(incremental["action_key"], non_incremental["action_key"]);
    mixed.remote.retain_record(&path, &incremental);
    mixed.clear_results();
    let key = incremental["action_key"].as_str().unwrap();
    fs::remove_file(mixed.release_path()).unwrap();
    symlink(
        mixed
            .store
            .join("cache")
            .join(&key[..2])
            .join(format!("{key}.json")),
        mixed.release_path(),
    )
    .unwrap();
    mixed.run(&["build"]);
    let report = mixed.report();
    assert_eq!(
        report_unit(&report, "middle", "compile")["cache"]["probe"],
        "remote"
    );
    assert_unit_cache(&report, "dependency", "compile", "dependency", "miss");
    assert_unit_cache(
        &report,
        &mixed.directory.package_name,
        "compile",
        "app",
        "miss",
    );
    mixed.assert_binary_output("42\n");

    let fixture = Fixture::new("remote-build");
    fixture.remote.objects.lock().unwrap().transient_failures = 1;
    let output = fixture.run(&["cache", "build"]);
    assert!(output.contains("retrying cache publication"), "{output}");
    let (record_path, record) = fixture
        .remote
        .record(|record| record["spec"]["target"]["name"] == "app");
    let binary_hash = record["result"]["outputs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|output| output["exe"] == true)
        .unwrap()["hash"]
        .as_str()
        .unwrap();
    let blob_path = format!("corgi/v2/blobs/{binary_hash}");
    fixture.remote.retain_record(&record_path, &record);
    fixture.clear_results();
    // An unrelated nested build output must not change the remote source key.
    fixture.write("fixtures/target/app", "not an input");
    fs::remove_file(fixture.release_path()).unwrap();
    fixture.remote.objects.lock().unwrap().compiler_gate =
        Some((record_path.clone(), fixture.store.clone()));
    fixture.remote.objects.lock().unwrap().slow_blob = Some(blob_path.clone());

    let output = fixture.run(&["build"]);
    fixture.remote.objects.lock().unwrap().slow_blob = None;
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

    let credentials_path = fixture
        .directory
        .path
        .join("home/.config/corgi/credentials");
    let credentials = format!(
        "[\"{PRIVATE_URL}\"]\naccess-key-id = \"fixture-read-access\"\nsecret-access-key = \"fixture-read-secret\"\n"
    );
    fs::write(&credentials_path, &credentials).unwrap();
    fixture.write(
        "corgi.toml",
        &format!("[cache]\nread-url = \"{PRIVATE_URL}\"\n"),
    );
    fixture.clear_results();
    fixture.run(&["build"]);
    assert_eq!(
        report_unit(
            &fixture.report(),
            &fixture.directory.package_name,
            "compile"
        )["cache"]["probe"],
        "remote"
    );
    fixture.assert_binary_output("42\n");
    fixture.remote.objects.lock().unwrap().requests.clear();
    fixture.run(&["build"]);
    assert!(fixture.remote.objects.lock().unwrap().requests.is_empty());

    fixture.release_compiler();
    fixture.remote.objects.lock().unwrap().compiler_gate = None;
    for contents in [None, Some(credentials.replace("/bucket", "/other-bucket"))] {
        match contents {
            Some(contents) => fs::write(&credentials_path, contents).unwrap(),
            None => fs::remove_file(&credentials_path).unwrap(),
        }
        fixture.clear_results();
        let output = fixture.run(&["build"]);
        assert_eq!(
            output
                .matches("Warning no credentials for remote cache configured")
                .count(),
            1,
            "{output}"
        );
        assert!(fixture.remote.objects.lock().unwrap().requests.is_empty());
        let report = fixture.report();
        assert!(report["remote_downloads"].as_array().unwrap().is_empty());
        assert_unit_cache(
            &report,
            &fixture.directory.package_name,
            "compile",
            "app",
            "miss",
        );
        fixture.assert_binary_output("42\n");
    }

    for (url, contents, expected) in [
        (
            PRIVATE_URL.to_owned(),
            format!("{credentials}invalid = ["),
            "invalid R2 credentials file",
        ),
        (
            PRIVATE_URL.replace("https:", "http:"),
            credentials,
            "require HTTPS",
        ),
    ] {
        fixture.write("corgi.toml", &format!("[cache]\nread-url = \"{url}\"\n"));
        fs::write(&credentials_path, contents).unwrap();
        let output = fixture.command(&["build"]).output().unwrap();
        assert_failure(&output, "invalid private read configuration");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("fixture-read-secret"), "{error}");
        assert!(!error.contains("fixture-read-access"), "{error}");
        assert!(fixture.remote.objects.lock().unwrap().requests.is_empty());
    }
    fixture.write(
        "corgi.toml",
        &format!("[cache]\nread-url = \"{}/read\"\n", fixture.remote.url),
    );

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

    for build_script in [false, true] {
        let fixture = Fixture::new("remote-prune");
        if build_script {
            fixture.write("build.rs", &format!(r#"
fn main() {{
    let scratch = std::path::PathBuf::from(std::env::var_os("TMPDIR").unwrap());
    std::fs::write(scratch.join("remote-compiler-running"), "running").unwrap();
    let start = std::time::Instant::now();
    while !std::path::Path::new({:?}).exists() {{
        assert!(start.elapsed() < std::time::Duration::from_secs(30), "build script was not cancelled");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }}
}}
"#, fixture.release_path()));
        } else {
            fixture.write(
                "dependency/Cargo.toml",
                "[package]\nname = \"dependency\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
                 [dependencies]\ncompiler_gate = { path = \"../compiler-gate\" }\n",
            );
            fixture.write(
                "dependency/src/lib.rs",
                &format!(
                    "compiler_gate::wait!({:?});\npub fn value() -> u32 {{ 42 }}\n",
                    fixture.release_path()
                ),
            );
        }
        fixture.run(&["cache", "build"]);
        let (path, record) = fixture
            .remote
            .record(|record| record["spec"]["target"]["name"] == "app");
        fixture.remote.retain_record(&path, &record);
        fixture.clear_results();
        fs::remove_file(fixture.release_path()).unwrap();
        fixture.remote.objects.lock().unwrap().compiler_gate = Some((path, fixture.store.clone()));
        let output = fixture.run(&["build", "--timings"]);
        let report = fixture.report();
        let unit = report_unit(
            &report,
            if build_script {
                &fixture.directory.package_name
            } else {
                "dependency"
            },
            if build_script {
                "run_build_script"
            } else {
                "compile"
            },
        );
        assert_eq!(unit["outcome"]["status"], "cancelled", "{report}");
        assert!(!compiler_running(&fixture.store));
        let units = report["units"].as_array().unwrap();
        let cancelled = units
            .iter()
            .filter(|unit| unit["outcome"]["status"] == "cancelled")
            .count();
        let skipped = units
            .iter()
            .filter(|unit| unit["outcome"]["status"] == "skipped")
            .count();
        let executed = units
            .iter()
            .filter(|unit| {
                unit["outcome"]["status"] == "success" && unit["cache"]["result"] == "miss"
            })
            .count();
        assert!(output.contains(&format!("{executed} executed, 1 downloaded, 0 cached, {cancelled} cancelled, {skipped} skipped")), "{output}");
        assert_eq!(executed + 1 + cancelled + skipped, units.len());
        fixture.assert_binary_output("42\n");
    }
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

const PRIVATE_URL: &str =
    "https://00000000000000000000000000000000.r2.cloudflarestorage.com/bucket";

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
        fs::create_dir_all(fixture.directory.path.join("home/.config/corgi")).unwrap();
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
            &r#"#!/usr/bin/env python3
import os, sys
sys.argv = [arg.replace("PRIVATE_CACHE_URL/", "LOCAL_CACHE_URL/", 1)
            if arg.startswith("PRIVATE_CACHE_URL/") else arg for arg in sys.argv]
if any(arg.startswith("http://127.0.0.1:") for arg in sys.argv[1:]):
    os.execvp("curl", ["curl", "-q", *sys.argv[1:]])
tool = os.environ["CORGI_TEST_DOWNLOAD_CURL"]
os.execvp(tool, [tool, *sys.argv[1:]])
"#
            .replace("PRIVATE_CACHE_URL", PRIVATE_URL)
            .replace(
                "LOCAL_CACHE_URL",
                &format!("{}/private", fixture.remote.url),
            ),
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
            .env("HOME", self.directory.path.join("home"))
            .env(
                "RUSTUP_HOME",
                std::env::var_os("RUSTUP_HOME")
                    .unwrap_or_else(|| dirs::home_dir().unwrap().join(".rustup").into_os_string()),
            )
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
    slow_blob: Option<String>,
    transient_failures: usize,
    slow_put: bool,
    active_puts: usize,
    peak_puts: usize,
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
    let private = url.starts_with("/private/");
    let path = url
        .strip_prefix(if public {
            "/read/"
        } else if private {
            "/private/"
        } else {
            "/bucket/"
        })
        .unwrap()
        .to_owned();
    let mut length = 0;
    let mut authenticated = false;
    let mut authorization = String::new();
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
                "authorization" => {
                    authenticated = true;
                    authorization = value.trim().to_owned();
                }
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
    if private {
        assert_eq!(method, "GET");
        assert!(authorization.starts_with("AWS4-HMAC-SHA256 "));
        assert!(authorization.contains("Credential=fixture-read-access/"));
        assert!(!authorization.contains("fixture-read-secret"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let (gate, slow_blob, slow_put) = {
        let mut state = objects.lock().unwrap();
        state.requests.push((method.clone(), path.clone()));
        let gate = state
            .compiler_gate
            .clone()
            .filter(|(key, _)| public && key == &path);
        let slow_put = method == "PUT" && state.slow_put;
        if slow_put {
            state.active_puts += 1;
            state.peak_puts = state.peak_puts.max(state.active_puts);
        }
        (
            gate,
            method == "GET" && state.slow_blob.as_ref() == Some(&path),
            slow_put,
        )
    };
    if slow_put {
        thread::sleep(Duration::from_millis(100));
        objects.lock().unwrap().active_puts -= 1;
    }
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
            _ if !public && !private && state.transient_failures > 0 => {
                state.transient_failures -= 1;
                (503, Vec::new())
            }
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
        if slow_blob && status == 200 {
            // A progressing blob must survive the former ten-second overall deadline.
            for chunk in response.chunks(response.len().div_ceil(12)) {
                thread::sleep(Duration::from_secs(1));
                stream.write_all(chunk)?;
            }
        } else {
            stream.write_all(&response)?;
        }
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
