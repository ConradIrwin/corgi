use super::{ensure_tool_shims, read_corgi_toml, Store, ToolRt};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn command_alias_uses_the_effective_executable_and_invalidates_cached_shims() {
    let fixture = Fixture::new();
    let pinned = fixture.store.root.join("pinned-cmake");
    let native = fixture.store.root.join("native-cmake");
    let tool = ToolRt {
        name: "cmake-linux".into(),
        version: "1".into(),
        env: "CMAKE".into(),
        value: pinned.to_string_lossy().into_owned(),
        command: "cmake".into(),
        id: "pin".into(),
        bin: "bin/cmake".into(),
        packages: Vec::new(),
        targets: Vec::new(),
    };
    fs::write(&pinned, "").unwrap();
    fs::write(&native, "").unwrap();

    let shims = ensure_tool_shims(&fixture.store, &[&tool], &[])
        .unwrap()
        .unwrap();
    assert_eq!(fs::read_link(shims.join("cmake")).unwrap(), pinned);
    let environment = vec![("CMAKE".into(), native.to_string_lossy().into_owned())];
    let overridden = ensure_tool_shims(&fixture.store, &[&tool], &environment)
        .unwrap()
        .unwrap();
    assert_ne!(overridden, shims);
    assert_eq!(fs::read_link(overridden.join("cmake")).unwrap(), native);
    assert_eq!(
        ensure_tool_shims(&fixture.store, &[&tool], &environment)
            .unwrap()
            .unwrap(),
        overridden
    );
    assert!(ensure_tool_shims(&fixture.store, &[&tool, &tool], &[]).is_err());
}

#[test]
fn tool_commands_must_be_filenames_for_executable_tools() {
    let fixture = Fixture::new();
    for command in ["", ".", "..", "/cmake", "../cmake", r"..\cmake"] {
        fixture.write_config("bin", Some(command));
        assert!(read_corgi_toml(&fixture.store.root).is_err());
    }
    fixture.write_config("path", Some("cmake"));
    assert!(read_corgi_toml(&fixture.store.root).is_err());
    fixture.write_config("bin", Some("cmake"));
    let config = read_corgi_toml(&fixture.store.root).unwrap().unwrap();
    assert_eq!(
        config.tools["cmake-linux"].command.as_deref(),
        Some("cmake")
    );
    fixture.write_config("bin", None);
    let config = read_corgi_toml(&fixture.store.root).unwrap().unwrap();
    assert_eq!(config.tools["cmake-linux"].command, None);
}

struct Fixture {
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "corgi-tool-shims-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        Self {
            store: Store::new(root).unwrap(),
        }
    }

    fn write_config(&self, export: &str, command: Option<&str>) {
        let command = command
            .map(|command| format!("command = {command:?}\n"))
            .unwrap_or_default();
        fs::write(
            self.store.root.join("corgi.toml"),
            format!(
                "[tools.cmake-linux]\nversion = \"1\"\nurl = \"https://example.com/cmake.tar.gz\"\n\
                 sha256 = \"pin\"\n{export} = \"bin/cmake\"\nenv = \"CMAKE\"\n{command}"
            ),
        )
        .unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.store.root).unwrap();
    }
}
