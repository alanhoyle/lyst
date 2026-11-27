//! LESSOPEN-style input preprocessing.
//!
//! Reads `LYST_OPEN` (falling back to `LESSOPEN`) with the same syntax as less:
//!
//! - `|cmd %s`   run cmd; show its output, or the original file if it prints nothing
//! - `||cmd %s`  as above, but only use the output if cmd exits successfully
//!   (even if empty); on failure show the original
//! - `|-cmd %s` / `||-cmd %s`  also filter standard input, with `%s` replaced by `-`
//! - `cmd %s`    cmd prints the name of a replacement file to show instead;
//!   `LYST_CLOSE`/`LESSCLOSE` (`cmd %s %s`) is then run with the
//!   original and replacement names so it can clean up

use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// How long a preprocessor may run before we show that we're waiting on it.
const SHOW_AFTER: Duration = Duration::from_millis(500);

/// Where to show progress while a slow preprocessor runs.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Feedback {
    None = 0,
    /// A status line on stderr (before the full-screen UI starts).
    Stderr = 1,
    /// The bottom row of the full-screen UI; Esc or Ctrl-C cancels.
    Screen = 2,
}

static FEEDBACK: AtomicU8 = AtomicU8::new(Feedback::None as u8);

pub fn set_feedback(f: Feedback) {
    FEEDBACK.store(f as u8, Ordering::Relaxed);
}

fn feedback() -> Feedback {
    match FEEDBACK.load(Ordering::Relaxed) {
        1 => Feedback::Stderr,
        2 => Feedback::Screen,
        _ => Feedback::None,
    }
}

fn canceled_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "Canceled")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// Output names a replacement file.
    File,
    /// Output replaces the file contents if non-empty.
    Pipe,
    /// Output replaces the file contents if the command succeeds.
    Strict,
}

#[derive(Debug)]
pub struct Preprocessor {
    kind: Kind,
    stdin_too: bool,
    cmd: String,
    close: Option<String>,
    /// Short name shown in the status bar, e.g. "lesspipe.sh".
    label: String,
}

static GLOBAL: OnceLock<Option<Preprocessor>> = OnceLock::new();

/// Reads the environment once at startup. `enabled = false` disables preprocessing.
pub fn init(enabled: bool) {
    GLOBAL.get_or_init(|| {
        if !enabled {
            return None;
        }
        let env = |a: &str, b: &str| std::env::var(a).or_else(|_| std::env::var(b)).ok();
        Preprocessor::parse(&env("LYST_OPEN", "LESSOPEN")?, env("LYST_CLOSE", "LESSCLOSE"))
    });
}

fn global() -> Option<&'static Preprocessor> {
    GLOBAL.get().and_then(Option::as_ref)
}

/// Filters a file. Returns the replacement contents and a label, or `None`
/// to show the original file. Fails only if the user canceled.
pub fn file(path: &Path) -> io::Result<Option<(Vec<u8>, String)>> {
    let Some(p) = global() else { return Ok(None) };
    match p.run_file(path) {
        Ok(out) => Ok(out.map(|b| (b, p.label.clone()))),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => Err(e),
        // A broken preprocessor shouldn't stop us showing the file itself.
        Err(_) => Ok(None),
    }
}

/// Filters standard input if the preprocessor asked for it (`|-`).
pub fn stdin(input: Vec<u8>) -> io::Result<(Vec<u8>, Option<String>)> {
    match global() {
        Some(p) if p.stdin_too && p.kind != Kind::File => match p.run_pipe("-", Some(&input)) {
            Ok(Some(out)) => Ok((out, Some(p.label.clone()))),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Err(e),
            _ => Ok((input, None)),
        },
        _ => Ok((input, None)),
    }
}

impl Preprocessor {
    fn parse(spec: &str, close: Option<String>) -> Option<Self> {
        let spec = spec.trim_start();
        let (kind, rest) = if let Some(r) = spec.strip_prefix("||") {
            (Kind::Strict, r)
        } else if let Some(r) = spec.strip_prefix('|') {
            (Kind::Pipe, r)
        } else {
            (Kind::File, spec)
        };
        let (stdin_too, rest) = match rest.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, rest),
        };
        let cmd = rest.trim().to_string();
        if cmd.is_empty() {
            return None;
        }
        let label = cmd
            .split_whitespace()
            .next()
            .and_then(|w| Path::new(w).file_name())
            .map_or_else(|| "preprocessor".into(), |n| n.to_string_lossy().into_owned());
        let close = close.filter(|c| !c.trim().is_empty() && kind == Kind::File);
        Some(Preprocessor { kind, stdin_too, cmd, close, label })
    }

    fn run_file(&self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        let name = path.to_string_lossy();
        if self.kind != Kind::File {
            return self.run_pipe(&name, None);
        }
        let (_, stdout) = run(shell(&substitute(&self.cmd, &[&name])), None, &self.label, &name)?;
        let replacement = String::from_utf8_lossy(&stdout).trim().to_string();
        if replacement.is_empty() {
            return Ok(None);
        }
        let data = std::fs::read(&replacement);
        if let Some(close) = &self.close {
            let _ = shell(&substitute(close, &[&name, &replacement]))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        data.map(Some)
    }

    fn run_pipe(&self, name: &str, input: Option<&[u8]>) -> io::Result<Option<Vec<u8>>> {
        let shown = if name == "-" { "standard input" } else { name };
        let (status, stdout) = run(shell(&substitute(&self.cmd, &[name])), input, &self.label, shown)?;
        Ok(match self.kind {
            Kind::Strict if status.success() => Some(stdout),
            Kind::Pipe if !stdout.is_empty() => Some(stdout),
            _ => None,
        })
    }
}

/// Runs `cmd`, collecting stdout. If it takes a while, shows a spinner
/// (see [`Feedback`]); on screen, Esc or Ctrl-C kills it and returns
/// an `Interrupted` error.
fn run(mut cmd: Command, input: Option<&[u8]>, label: &str, name: &str) -> io::Result<(ExitStatus, Vec<u8>)> {
    cmd.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        // stderr would scribble over the full-screen display.
        .stderr(Stdio::null());
    // Own process group, so canceling also stops helpers like tar and gzip.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn()?;

    // Feed stdin and drain stdout on threads so neither pipe can fill up and stall.
    let writer = match (input, child.stdin.take()) {
        (Some(data), Some(mut sink)) => {
            let data = data.to_vec();
            Some(std::thread::spawn(move || {
                let _ = sink.write_all(&data);
            }))
        }
        _ => None,
    };
    let mut out = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = out.read_to_end(&mut v);
        v
    });

    let start = Instant::now();
    let mut progress = Progress { frame: 0, shown: false };
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let elapsed = start.elapsed();
        if elapsed >= SHOW_AFTER {
            progress.show(label, name, elapsed);
        }
        if wait_for_cancel(Duration::from_millis(100)) {
            kill_tree(&mut child);
            let _ = reader.join();
            progress.clear();
            return Err(canceled_error());
        }
    };
    let stdout = reader.join().unwrap_or_default();
    if let Some(w) = writer {
        let _ = w.join();
    }
    progress.clear();
    Ok((status, stdout))
}

/// Runs `work` on a thread, showing the same progress as a slow
/// preprocessor. Esc or Ctrl-C sets the flag `work` is given and returns
/// an `Interrupted` error once `work` notices it and returns.
pub fn with_progress<T: Send + 'static>(
    label: &str,
    name: &str,
    work: impl FnOnce(&AtomicBool) -> T + Send + 'static,
) -> io::Result<T> {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let handle = std::thread::spawn(move || work(&flag));
    let start = Instant::now();
    let mut progress = Progress { frame: 0, shown: false };
    while !handle.is_finished() {
        let elapsed = start.elapsed();
        if elapsed < SHOW_AFTER {
            // Too soon for a spinner: leave keys typed ahead for afterwards.
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        progress.show(label, name, elapsed);
        if wait_for_cancel(Duration::from_millis(50)) {
            cancel.store(true, Ordering::Relaxed);
            let _ = handle.join();
            progress.clear();
            return Err(canceled_error());
        }
    }
    progress.clear();
    handle.join().map_err(|_| io::Error::other(format!("{label} failed")))
}

/// Sleeps up to `d`; on screen, returns true early if Esc or Ctrl-C is pressed.
fn wait_for_cancel(d: Duration) -> bool {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    if feedback() != Feedback::Screen {
        std::thread::sleep(d);
        return false;
    }
    let deadline = Instant::now() + d;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if !event::poll(left).unwrap_or(false) {
            return false;
        }
        // Other input is dropped; the caller re-checks the terminal size afterwards.
        if let Ok(Event::Key(k)) = event::read()
            && k.kind != KeyEventKind::Release
            && (k.code == KeyCode::Esc
                || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL)))
        {
            return true;
        }
    }
}

fn kill_tree(child: &mut Child) {
    #[cfg(unix)]
    // SAFETY: plain syscall; a negative pid signals the whole process group
    // we created for the child.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGTERM);
    }
    let _ = child.kill();
    let _ = child.wait();
}

struct Progress {
    frame: usize,
    shown: bool,
}

impl Progress {
    const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

    fn show(&mut self, label: &str, name: &str, elapsed: Duration) {
        let spin = Self::SPINNER[self.frame % Self::SPINNER.len()];
        self.frame += 1;
        let file = Path::new(name).file_name().map_or(name.into(), |f| f.to_string_lossy());
        let secs = elapsed.as_secs_f32();
        match feedback() {
            Feedback::None => return,
            Feedback::Stderr => {
                use std::io::IsTerminal;
                if !io::stderr().is_terminal() {
                    return;
                }
                eprint!("\r\x1b[K{spin} {label}: {file}  {secs:.1}s  (Ctrl-C to quit)");
            }
            Feedback::Screen => {
                use crossterm::style::{Attribute, Print, SetAttribute};
                use crossterm::{cursor, queue, terminal};
                let (w, h) = terminal::size().unwrap_or((80, 24));
                let text = format!(" {spin} {label}: {file}  {secs:.1}s  · Esc cancels ");
                let text: String = text.chars().take(w as usize).collect();
                let mut o = io::stdout();
                let _ = queue!(
                    o,
                    cursor::Hide,
                    cursor::MoveTo(0, h.saturating_sub(1)),
                    SetAttribute(Attribute::Reset),
                    SetAttribute(Attribute::Reverse),
                    Print(&text),
                    SetAttribute(Attribute::Reset),
                    terminal::Clear(terminal::ClearType::UntilNewLine),
                );
                let _ = o.flush();
            }
        }
        self.shown = true;
    }

    fn clear(&mut self) {
        // On screen, the next redraw replaces the line.
        if self.shown && feedback() == Feedback::Stderr {
            eprint!("\r\x1b[K");
        }
        self.shown = false;
    }
}

fn shell(cmd: &str) -> Command {
    let mut c = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    // lesspipe otherwise prepends "==> ..." tips about less's own keys.
    c.env("LESSQUIET", "1");
    c
}

/// Replaces successive `%s` with shell-quoted arguments (`%%` is a literal `%`).
fn substitute(template: &str, args: &[&str]) -> String {
    let mut out = String::new();
    let mut args = args.iter();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, chars.peek()) {
            ('%', Some('s')) => {
                chars.next();
                out.push_str(&quote(args.next().copied().unwrap_or("")));
            }
            ('%', Some('%')) => {
                chars.next();
                out.push('%');
            }
            _ => out.push(c),
        }
    }
    out
}

fn quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./,:@+=".contains(&b)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_less_syntax() {
        let p = Preprocessor::parse("|/opt/homebrew/bin/lesspipe.sh %s", None).unwrap();
        assert_eq!((p.kind, p.stdin_too, p.label.as_str()), (Kind::Pipe, false, "lesspipe.sh"));
        let p = Preprocessor::parse("||- myfilter %s", Some("x".into())).unwrap();
        assert_eq!((p.kind, p.stdin_too, p.cmd.as_str()), (Kind::Strict, true, "myfilter %s"));
        assert!(p.close.is_none(), "LESSCLOSE only applies to the non-pipe form");
        let p = Preprocessor::parse("lessopen.sh %s", Some("lessclose.sh %s %s".into())).unwrap();
        assert_eq!(p.kind, Kind::File);
        assert!(p.close.is_some());
        assert!(Preprocessor::parse("|", None).is_none());
    }

    #[test]
    fn substitution_quotes_names() {
        assert_eq!(substitute("cat %s", &["a.txt"]), "cat a.txt");
        assert_eq!(substitute("cat %s", &["it's here"]), r"cat 'it'\''s here'");
        assert_eq!(substitute("x %s %s 100%%", &["a", "b c"]), "x a 'b c' 100%");
    }

    #[cfg(unix)]
    #[test]
    fn pipe_modes() {
        let dir = std::env::temp_dir();
        let f = dir.join(format!("lyst-pre-{}.txt", std::process::id()));
        std::fs::write(&f, "hello\n").unwrap();

        let upper = Preprocessor::parse("|tr a-z A-Z < %s", None).unwrap();
        assert_eq!(upper.run_file(&f).unwrap().unwrap(), b"HELLO\n");
        // Empty output from `|` means "use the original".
        let silent = Preprocessor::parse("|true %s", None).unwrap();
        assert!(silent.run_file(&f).unwrap().is_none());
        // `||` with success and no output means "empty file"; failure means original.
        let strict_ok = Preprocessor::parse("||true %s", None).unwrap();
        assert_eq!(strict_ok.run_file(&f).unwrap().unwrap(), b"");
        let strict_fail = Preprocessor::parse("||echo junk; false %s", None).unwrap();
        assert!(strict_fail.run_file(&f).unwrap().is_none());
        // `|-` filters standard input with %s = "-".
        let p = Preprocessor::parse("|-tr a-z A-Z < /dev/stdin #%s", None).unwrap();
        assert_eq!(p.run_pipe("-", Some(b"abc")).unwrap().unwrap(), b"ABC");
        // Replacement-file form plus close hook.
        let rep = dir.join(format!("lyst-rep-{}.txt", std::process::id()));
        std::fs::write(&rep, "replaced").unwrap();
        // The close hook gets (original, replacement); this one removes the replacement.
        let p = Preprocessor::parse(&format!("echo {} #%s", rep.display()), Some("rm -f %s.none %s".into())).unwrap();
        assert_eq!(p.run_file(&f).unwrap().unwrap(), b"replaced");
        assert!(!rep.exists());
        let _ = std::fs::remove_file(&f);
    }
}
