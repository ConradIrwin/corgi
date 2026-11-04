use super::capture_with_live_stderr;
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn captured_commands_inherit_stdin() {
    const CHILD: &str = "CORGI_TEST_CAPTURE_STDIN";
    const INPUT: &str = "credential-helper-input\n";

    if std::env::var_os(CHILD).is_some() {
        let mut command = Command::new("sh");
        command.args(["-c", "IFS= read -r input; printf '%s\\n' \"$input\""]);
        assert_eq!(
            capture_with_live_stderr(&mut command, "read helper input").unwrap(),
            INPUT
        );
        // Distinguish exercising the helper from a filter that selected no tests.
        std::process::exit(42);
    }

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "build::capture_tests::captured_commands_inherit_stdin",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(INPUT.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(42),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
