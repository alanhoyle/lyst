//! Handing a file to the operating system's default application, or to
//! the user's text editor.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

/// The platform's "open with default app" command for `path`.
fn command(path: &Path) -> Command {
    #[cfg(target_os = "macos")]
    let c = {
        let mut c = Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(windows)]
    let c = {
        // `start`'s first quoted argument is a window title, so pass an empty one.
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]).arg(path);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let c = {
        let mut c = Command::new("xdg-open");
        c.arg(path);
        c
    };
    c
}

/// Opens `path` with its default application without waiting for it.
/// The launcher's output is discarded so it can't scribble over the screen.
pub fn open(path: &Path) -> io::Result<()> {
    // An absolute path can't be mistaken for an option by the launcher.
    let path = crate::paths::canonicalize(path)?;
    // Some launchers (xdg-open) stay around a while; `launch` reaps them.
    launch(command(&path))
}

/// Reaps a launcher when it exits, so it doesn't linger as a zombie.
fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Spawns `c` detached from the screen: no input, output discarded.
fn launch(mut c: Command) -> io::Result<()> {
    let child = c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    reap(child);
    Ok(())
}

/// Opens `path` in the default web browser (or `$BROWSER`) without waiting.
pub fn open_in_browser(path: &Path) -> io::Result<()> {
    let path = crate::paths::canonicalize(path)?;
    let url = file_url(&path);
    if let Some(cmd) = browser_env(std::env::var("BROWSER").ok()) {
        return launch(shell_with_arg(&cmd, &url));
    }
    launch(default_browser_command(&path, &url)?)
}

/// The first command in `$BROWSER`, which (as with Python's `webbrowser`)
/// may list several separated like `PATH`.
fn browser_env(var: Option<String>) -> Option<String> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    var?.split(sep).map(str::trim).find(|c| !c.is_empty()).map(String::from)
}

/// Runs a user-supplied command line through the shell with `arg` in place
/// of `%s`, or after it. `arg` is passed separately, never interpreted.
fn shell_with_arg(cmd: &str, arg: &str) -> Command {
    #[cfg(windows)]
    let c = {
        use std::os::windows::process::CommandExt;
        let quoted = format!("\"{arg}\"");
        let line = if cmd.contains("%s") { cmd.replace("%s", &quoted) } else { format!("{cmd} {quoted}") };
        let mut c = Command::new("cmd");
        c.arg("/C").raw_arg(line);
        c
    };
    #[cfg(not(windows))]
    let c = {
        let line = if cmd.contains("%s") { cmd.replace("%s", "\"$1\"") } else { format!("{cmd} \"$1\"") };
        let mut c = Command::new("sh");
        c.arg("-c").arg(line).arg("sh").arg(arg);
        c
    };
    c
}

/// A `file://` URL for an absolute path, percent-encoding anything but
/// unreserved characters and `/`.
fn file_url(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let mut url = String::from(if s.starts_with('/') { "file://" } else { "file:///" });
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => url.push(b as char),
            // Keep a Windows drive letter's colon readable: file:///C:/...
            b':' if url.len() == 9 => url.push(':'),
            _ => url.push_str(&format!("%{b:02X}")),
        }
    }
    url
}

/// macOS: asks Launch Services which app opens https links, then has `open`
/// give it the file.
#[cfg(target_os = "macos")]
fn default_browser_command(path: &Path, _url: &str) -> io::Result<Command> {
    let script = r#"ObjC.import("AppKit"); $.NSWorkspace.sharedWorkspace.URLForApplicationToOpenURL($.NSURL.URLWithString("https://example.com")).path.js"#;
    let out = Command::new("osascript").args(["-l", "JavaScript", "-e", script]).stderr(Stdio::null()).output()?;
    let app = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let app = if out.status.success() && !app.is_empty() { app } else { "Safari".into() };
    let mut c = Command::new("open");
    c.arg("-a").arg(app).arg(path);
    Ok(c)
}

/// Linux and other Unixes: the desktop entry `xdg-settings` names as the
/// default browser, else the usual fallbacks.
#[cfg(all(unix, not(target_os = "macos")))]
fn default_browser_command(_path: &Path, url: &str) -> io::Result<Command> {
    let entry = Command::new("xdg-settings")
        .args(["get", "default-web-browser"])
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|id| id.ends_with(".desktop"))
        .and_then(|id| desktop_exec(&id));
    if let Some(argv) = entry.and_then(|exec| exec_argv(&exec, url)) {
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        return Ok(c);
    }
    for b in ["sensible-browser", "x-www-browser", "firefox", "chromium", "google-chrome"] {
        if which(b) {
            let mut c = Command::new(b);
            c.arg(url);
            return Ok(c);
        }
    }
    let mut c = Command::new("xdg-open");
    c.arg(url);
    Ok(c)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn which(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(program).is_file()))
}

/// The `Exec=` line of a desktop entry, searched for in the XDG data dirs.
#[cfg(all(unix, not(target_os = "macos")))]
fn desktop_exec(id: &str) -> Option<String> {
    let home = std::env::var("XDG_DATA_HOME")
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.local/share")));
    let dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    home.into_iter()
        .chain(dirs.split(':').map(String::from))
        .map(|d| Path::new(&d).join("applications").join(id))
        .find_map(|f| std::fs::read_to_string(f).ok())
        .and_then(|text| {
            // Only the main [Desktop Entry] group, not actions like "New Window".
            text.lines()
                .skip_while(|l| l.trim() != "[Desktop Entry]")
                .take_while(|l| !l.starts_with('[') || l.trim() == "[Desktop Entry]")
                .find_map(|l| l.strip_prefix("Exec=").map(String::from))
        })
}

/// Splits a desktop entry's `Exec=` value into arguments, putting `url`
/// where its field code (`%u`, `%U`, `%f`, `%F`) is, or at the end.
#[cfg_attr(any(windows, target_os = "macos"), allow(dead_code))]
fn exec_argv(exec: &str, url: &str) -> Option<Vec<String>> {
    let mut argv = Vec::new();
    let (mut cur, mut quoted, mut any, mut placed) = (String::new(), false, false, false);
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            '\\' if quoted => cur.extend(chars.next()),
            ' ' if !quoted => {
                if any || !cur.is_empty() {
                    argv.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            '%' => match chars.next() {
                Some('u' | 'U' | 'f' | 'F') => {
                    cur.push_str(url);
                    placed = true;
                }
                Some('%') => cur.push('%'),
                // Other field codes (%i, %c, %k, ...) are dropped.
                _ => {}
            },
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        argv.push(cur);
    }
    if argv.first().is_none_or(String::is_empty) {
        return None;
    }
    if !placed {
        argv.push(url.to_string());
    }
    Some(argv)
}

/// Windows: the program registered for https links, from the registry.
#[cfg(windows)]
fn default_browser_command(path: &Path, url: &str) -> io::Result<Command> {
    use std::os::windows::process::CommandExt;
    let script = r"$p = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\Shell\Associations\UrlAssociations\https\UserChoice').ProgId; (Get-ItemProperty ('Registry::HKEY_CLASSES_ROOT\' + $p + '\shell\open\command')).'(default)'";
    let line = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    // Typically: "C:\...\browser.exe" --single-argument %1
    let (exe, rest) = match line.strip_prefix('"').and_then(|l| l.split_once('"')) {
        Some((exe, rest)) => (exe.to_string(), rest.trim().to_string()),
        None => match line.split_once(' ') {
            Some((exe, rest)) => (exe.to_string(), rest.to_string()),
            None => (line.clone(), String::new()),
        },
    };
    if exe.is_empty() {
        return Ok(command(path));
    }
    let quoted = format!("\"{url}\"");
    let args = if rest.contains("%1") { rest.replace("\"%1\"", &quoted).replace("%1", &quoted) } else { format!("{rest} {quoted}") };
    let mut c = Command::new(exe);
    c.raw_arg(args);
    Ok(c)
}

/// The escape sequence asking the terminal to put `text` on the clipboard
/// (OSC 52). Most terminals support it, also over SSH; tmux needs
/// `set -g set-clipboard on`.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= chunk.len() { A[(n >> shift & 63) as usize] as char } else { '=' });
        }
    }
    out
}

/// The user's editor: `$VISUAL`, then `$EDITOR`, then `vi` (Notepad on
/// Windows, which has no convention of its own). It may include arguments,
/// like `code --wait`.
pub fn editor() -> String {
    editor_from(|k| std::env::var(k).ok())
}

fn editor_from(var: impl Fn(&str) -> Option<String>) -> String {
    ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(var)
        .map(|e| e.trim().to_string())
        .find(|e| !e.is_empty())
        .unwrap_or_else(|| if cfg!(windows) { "notepad".into() } else { "vi".into() })
}

/// Command that runs `editor` on `path`. The editor setting goes through the
/// shell so it can carry arguments; the path is passed separately so its
/// characters are never interpreted.
fn edit_command(editor: &str, path: &Path) -> Command {
    #[cfg(windows)]
    let c = {
        use std::os::windows::process::CommandExt;
        let mut c = Command::new("cmd");
        c.arg("/C").raw_arg(format!("{editor} \"{}\"", path.display()));
        c
    };
    #[cfg(not(windows))]
    let c = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(path);
        c
    };
    c
}

/// Runs the user's editor on `path` in this terminal and waits for it.
pub fn edit(path: &Path) -> io::Result<ExitStatus> {
    edit_command(&editor(), path).status()
}

/// Heuristic for "not a text file": a NUL byte, or many control characters,
/// in the first 8 KB on disk. (The viewer's own check looks at what a
/// preprocessor produced, which may be text made from a binary file.)
pub fn looks_binary(path: &Path) -> io::Result<bool> {
    let mut buf = Vec::with_capacity(8192);
    std::fs::File::open(path)?.take(8192).read_to_end(&mut buf)?;
    Ok(binary_bytes(&buf))
}

fn binary_bytes(b: &[u8]) -> bool {
    let control = b
        .iter()
        .filter(|&&c| c < 0x20 && !matches!(c, b'\t' | b'\n' | b'\r' | 0x0c | 0x1b))
        .count();
    b.contains(&0) || control * 10 > b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_sequence() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("héllo wörld".as_bytes()), "aMOpbGxvIHfDtnJsZA==");
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn file_urls() {
        assert_eq!(file_url(Path::new("/tmp/a b/x#1.html")), "file:///tmp/a%20b/x%231.html");
        assert_eq!(file_url(Path::new("/tmp/caf\u{e9}.md")), "file:///tmp/caf%C3%A9.md");
        assert_eq!(file_url(Path::new(r"C:\Users\me\a.html")), "file:///C:/Users/me/a.html");
    }

    #[test]
    fn browser_variable() {
        assert_eq!(browser_env(None), None);
        assert_eq!(browser_env(Some(" ".into())), None);
        let sep = if cfg!(windows) { ";" } else { ":" };
        assert_eq!(browser_env(Some(format!("{sep}firefox --new-tab{sep}chromium"))).as_deref(), Some("firefox --new-tab"));
    }

    #[cfg(not(windows))]
    #[test]
    fn browser_command_gets_the_url_once() {
        let out = |cmd| {
            let o = shell_with_arg(cmd, "file:///a%20b").output().unwrap();
            String::from_utf8(o.stdout).unwrap()
        };
        assert_eq!(out("echo"), "file:///a%20b\n");
        assert_eq!(out("echo before %s after"), "before file:///a%20b after\n");
    }

    #[test]
    fn desktop_entry_exec_lines() {
        let u = "file:///x";
        let v = |a: &[&str]| Some(a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(exec_argv("firefox %u", u), v(&["firefox", u]));
        assert_eq!(exec_argv("/usr/bin/google-chrome-stable %U", u), v(&["/usr/bin/google-chrome-stable", u]));
        assert_eq!(exec_argv("env BAMF=1 \"/opt/my browser/run\" --flag %F %i", u), v(&["env", "BAMF=1", "/opt/my browser/run", "--flag", u]));
        assert_eq!(exec_argv("epiphany", u), v(&["epiphany", u]));
        assert_eq!(exec_argv("", u), None);
    }

    #[test]
    fn editor_preference_order() {
        let env = |pairs: &'static [(&str, &str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(editor_from(env(&[("VISUAL", "nano"), ("EDITOR", "vim")])), "nano");
        assert_eq!(editor_from(env(&[("VISUAL", " "), ("EDITOR", "code --wait")])), "code --wait");
        let fallback = if cfg!(windows) { "notepad" } else { "vi" };
        assert_eq!(editor_from(env(&[])), fallback);
    }

    #[cfg(not(windows))]
    #[test]
    fn editor_gets_the_path_as_one_argument() {
        let dir = std::env::temp_dir().join(format!("lyst-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("it's a $file; with spaces.txt");
        std::fs::write(&file, "").unwrap();
        // The "editor" appends to the file it's given, with an argument of its own.
        let status = edit_command("sh -c 'echo edited >> \"$0\"'", &file).status().unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn binary_detection() {
        assert!(!binary_bytes(b"plain text\n\twith tabs\r\n"));
        assert!(!binary_bytes(b"\x1b[31mcolored\x1b[0m\n"));
        assert!(!binary_bytes("caf\u{e9} \u{2603}\n".as_bytes()));
        assert!(!binary_bytes(b""));
        assert!(binary_bytes(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"));
        assert!(binary_bytes(&[1, 2, 3, 4, b'a', b'b']));
        assert!(looks_binary(Path::new("samples/people.csv")).is_ok_and(|b| !b));
        assert!(looks_binary(Path::new("/definitely/not/here")).is_err());
    }

    #[test]
    fn launcher_gets_the_path() {
        let c = command(Path::new("/tmp/some file.txt"));
        let program = c.get_program().to_string_lossy().into_owned();
        let args: Vec<_> = c.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(["open", "xdg-open", "cmd"].contains(&program.as_str()), "program: {program}");
        assert_eq!(args.last().map(String::as_str), Some("/tmp/some file.txt"));
    }

    #[test]
    fn missing_file_is_an_error_without_launching_anything() {
        assert!(open(Path::new("/definitely/not/here.txt")).is_err());
    }
}
