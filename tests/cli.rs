//! End-to-end tests of the `lyst` binary. With stdout captured (not a
//! terminal), lyst behaves like `cat`, which makes it scriptable and testable.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn lyst() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_lyst"));
    // Keep the user's lesspipe setup out of the tests.
    for v in ["LESSOPEN", "LESSCLOSE", "LYST_OPEN", "LYST_CLOSE"] {
        c.env_remove(v);
    }
    c
}

fn run(mut c: Command, stdin: &[u8]) -> Output {
    let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

/// A scratch directory unique to one test.
fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lyst-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn cat_mode_copies_files_byte_for_byte() {
    let d = scratch("cat");
    let bin: Vec<u8> = (0..=255).collect();
    std::fs::write(d.join("a.txt"), "first\n").unwrap();
    std::fs::write(d.join("b.bin"), &bin).unwrap();
    let mut c = lyst();
    c.arg(d.join("a.txt")).arg(d.join("b.bin"));
    let out = run(c, b"");
    assert!(out.status.success());
    let mut want = b"first\n".to_vec();
    want.extend(&bin);
    assert_eq!(out.stdout, want);
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn stdin_is_read_with_no_args_or_dash() {
    let out = run(lyst(), b"piped\n");
    assert_eq!(out.stdout, b"piped\n");
    let mut c = lyst();
    c.arg("-");
    assert_eq!(run(c, b"dash\n").stdout, b"dash\n");
}

#[test]
fn missing_files_are_reported_but_others_still_print() {
    let d = scratch("missing");
    std::fs::write(d.join("ok.txt"), "ok\n").unwrap();
    let mut c = lyst();
    c.arg(d.join("nope.txt")).arg(d.join("ok.txt"));
    let out = run(c, b"");
    assert!(!out.status.success());
    assert_eq!(out.stdout, b"ok\n");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nope.txt"), "stderr: {err}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn browser_refuses_to_run_without_a_terminal() {
    let d = scratch("browse");
    let mut c = lyst();
    c.arg(&d);
    let out = run(c, b"");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs a terminal"));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn bad_arguments_are_rejected() {
    for args in [&["--tabs", "0"][..], &["--theme", "no-such-theme"], &["--encoding", "ebcdic"]] {
        let mut c = lyst();
        c.args(args);
        let out = run(c, b"");
        assert_eq!(out.status.code(), Some(2), "args {args:?}");
    }
}

#[test]
fn info_flags() {
    let mut c = lyst();
    c.arg("--version");
    let out = run(c, b"");
    assert!(String::from_utf8_lossy(&out.stdout).contains(env!("CARGO_PKG_VERSION")));

    let mut c = lyst();
    c.arg("--list-themes");
    let themes = String::from_utf8(run(c, b"").stdout).unwrap();
    assert!(themes.lines().any(|t| t == "ansi"), "themes: {themes}");
}

#[cfg(unix)]
#[test]
fn preprocessor_output_replaces_file_contents() {
    let d = scratch("pre");
    let f = d.join("in.txt");
    std::fs::write(&f, "shout\n").unwrap();

    let mut c = lyst();
    c.env("LYST_OPEN", "|tr a-z A-Z < %s").arg(&f);
    assert_eq!(run(c, b"").stdout, b"SHOUT\n");

    // LYST_OPEN takes precedence over LESSOPEN.
    let mut c = lyst();
    c.env("LESSOPEN", "|echo less %s").env("LYST_OPEN", "|echo lyst").arg(&f);
    assert_eq!(run(c, b"").stdout, b"lyst\n");

    // --no-preprocess shows the original.
    let mut c = lyst();
    c.env("LYST_OPEN", "|tr a-z A-Z < %s").arg("--no-preprocess").arg(&f);
    assert_eq!(run(c, b"").stdout, b"shout\n");

    // Table files lyst lays out itself are read directly.
    let csv = d.join("t.csv");
    std::fs::write(&csv, "a,b\n").unwrap();
    let mut c = lyst();
    c.env("LYST_OPEN", "|echo reformatted %s").arg(&csv);
    assert_eq!(run(c, b"").stdout, b"a,b\n");

    // A preprocessor that only adds colors is ignored in favor of the file.
    let mut c = lyst();
    c.env("LYST_OPEN", "|printf '\\033[32m'; cat %s; printf '\\033[0m'").arg(&f);
    assert_eq!(run(c, b"").stdout, b"shout\n");

    // lesspipe's "==> ..." tips are switched off.
    let mut c = lyst();
    c.env("LYST_OPEN", "|echo quiet=$LESSQUIET #%s").arg(&f);
    assert_eq!(run(c, b"").stdout, b"quiet=1\n");

    // The `|-` form also filters standard input.
    let mut c = lyst();
    c.env("LYST_OPEN", "|-tr a-z A-Z #%s");
    assert_eq!(run(c, b"quiet\n").stdout, b"QUIET\n");
    let _ = std::fs::remove_dir_all(&d);
}
