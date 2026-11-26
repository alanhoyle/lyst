//! Handing a file to the operating system's default application.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};

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
    let path = std::fs::canonicalize(path)?;
    let mut child = command(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reap the launcher when it exits; some (xdg-open) stay around a while.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
