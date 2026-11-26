mod app;
mod browser;
mod doc;
mod encoding;
mod external;
mod goto;
mod highlight;
mod preprocess;
mod search;
mod table;
mod usage;
mod ui;

use app::{App, Options, Source};
use clap::Parser;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::{cursor, execute, terminal};
use encoding::Encoding;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use ui::Ui;

/// A fast, keyboard-driven file viewer in the spirit of Vernon Buerg's LIST.
#[derive(Parser)]
#[command(
    version,
    after_help = "With no files (or a directory), opens a file browser. Press F1 or ? in the viewer for keys.\nUse - for standard input."
)]
struct Cli {
    /// Files to view, or a directory to browse (default: browse the current directory)
    files: Vec<PathBuf>,
    /// Wrap long lines
    #[arg(short, long)]
    wrap: bool,
    /// Start in hex dump mode
    #[arg(short = 'x', long)]
    hex: bool,
    /// Show line numbers
    #[arg(short, long)]
    numbers: bool,
    /// Tab width
    #[arg(short, long, default_value_t = 8, value_parser = clap::value_parser!(u8).range(1..=32))]
    tabs: u8,
    /// Character encoding
    #[arg(short, long, value_enum, default_value_t = Encoding::Utf8)]
    encoding: Encoding,
    /// Follow the file as it grows, like tail -f
    #[arg(short, long)]
    follow: bool,
    /// Treat search patterns as regular expressions
    #[arg(short, long)]
    regex: bool,
    /// Search for PATTERN on startup
    #[arg(short = 'p', long, value_name = "PATTERN")]
    search: Option<String>,
    /// Start at line N
    #[arg(short = 'l', long, value_name = "N")]
    line: Option<usize>,
    /// Don't syntax-highlight (toggle with S in the viewer)
    #[arg(long)]
    no_syntax: bool,
    /// Highlighting theme (default `ansi`, which uses your terminal's colors)
    #[arg(long, value_name = "NAME")]
    theme: Option<String>,
    /// Force a syntax (e.g. rust, py, "Markdown") instead of detecting it
    #[arg(long, value_name = "NAME")]
    syntax: Option<String>,
    /// List available highlighting themes and exit
    #[arg(long)]
    list_themes: bool,
    /// List known syntaxes (including ones added in the config directory) and exit
    #[arg(long)]
    list_syntaxes: bool,
    /// Ignore LYST_OPEN / LESSOPEN input preprocessors
    #[arg(long)]
    no_preprocess: bool,
    /// Show ANSI escape sequences raw instead of as colors (toggle with C)
    #[arg(long)]
    no_ansi: bool,
    /// Don't capture the mouse (toggle with F9)
    #[arg(long)]
    no_mouse: bool,
    /// Show the file browser in multiple columns
    #[arg(long)]
    wide: bool,
    /// DOS-style white-on-blue colors
    #[arg(long)]
    classic: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.list_themes {
        for t in highlight::theme_names() {
            println!("{t}");
        }
        return ExitCode::SUCCESS;
    }
    if cli.list_syntaxes {
        print!("{}", highlight::describe_syntaxes());
        return ExitCode::SUCCESS;
    }
    preprocess::init(!cli.no_preprocess);
    preprocess::set_feedback(preprocess::Feedback::Stderr);
    // Loading syntax definitions takes a moment; do it while we start up.
    std::thread::spawn(highlight::preload);
    let highlight = match highlight::HlConfig::new(!cli.no_syntax, cli.theme.as_deref(), cli.syntax.as_deref()) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("lyst: {e}");
            return ExitCode::from(2);
        }
    };

    let mut sources: Vec<Source> = cli
        .files
        .iter()
        .map(|p| if p.as_os_str() == "-" { Source::Stdin } else { Source::Path(p.clone()) })
        .collect();
    let browse_dir = match cli.files.as_slice() {
        [] if io::stdin().is_terminal() => Some(PathBuf::from(".")),
        [p] if p.is_dir() => Some(p.clone()),
        _ => None,
    };
    if sources.is_empty() {
        sources.push(Source::Stdin);
    }

    let opts = Options {
        wrap: cli.wrap,
        hex: cli.hex,
        numbers: cli.numbers,
        tab: cli.tabs as usize,
        encoding: cli.encoding,
        follow: cli.follow,
        classic: cli.classic,
        regex: cli.regex,
        highlight,
        ansi: !cli.no_ansi,
    };

    let prefs = browser::Prefs::new(cli.wide);
    if let Some(dir) = browse_dir {
        if !io::stdout().is_terminal() {
            eprintln!("lyst: the file browser needs a terminal");
            return ExitCode::from(2);
        }
        let browser = match browser::Browser::new(&dir, cli.classic, prefs) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("lyst: {}: {e}", dir.display());
                return ExitCode::FAILURE;
            }
        };
        let mut opts = opts;
        opts.follow = false;
        let mut ui = Ui::with_browser(browser, opts);
        ui.mouse = !cli.no_mouse;
        return run_tui(ui);
    }

    // Like `less`, behave like `cat` when output isn't a terminal.
    if !io::stdout().is_terminal() {
        return cat(&sources);
    }

    // Open the first file that loads; report the rest as we go.
    let mut first = None;
    for (i, s) in sources.iter().enumerate() {
        match s.load() {
            Ok(d) => {
                first = Some((i, d));
                break;
            }
            Err(e) => eprintln!("lyst: {}: {e}", describe(s)),
        }
    }
    let Some((idx, doc)) = first else { return ExitCode::FAILURE };

    let mut opts = opts;
    opts.follow &= matches!(sources[idx], Source::Path(_));

    // The terminal must be set up before App::new measures it.
    if let Err(e) = enter_terminal(!cli.no_mouse) {
        eprintln!("lyst: cannot set up terminal: {e}");
        return ExitCode::FAILURE;
    }
    install_panic_hook();

    let mut app = App::new(sources, idx, doc, opts);
    if let Some(n) = cli.line {
        app.goto_line(n);
    }
    if let Some(p) = &cli.search {
        app.set_search(p);
    }
    let mut ui = Ui::with_viewer(app, prefs);
    ui.mouse = !cli.no_mouse;
    run_tui_entered(ui)
}

fn run_tui(ui: Ui) -> ExitCode {
    if let Err(e) = enter_terminal(ui.mouse) {
        eprintln!("lyst: cannot set up terminal: {e}");
        return ExitCode::FAILURE;
    }
    install_panic_hook();
    run_tui_entered(ui)
}

fn run_tui_entered(mut ui: Ui) -> ExitCode {
    let result = run(&mut ui);
    leave_terminal();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lyst: {e}");
            ExitCode::FAILURE
        }
    }
}

fn install_panic_hook() {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave_terminal();
        prev_hook(info);
    }));
}

fn run(app: &mut Ui) -> io::Result<()> {
    let mut out = io::BufWriter::with_capacity(1 << 16, io::stdout());
    let mut dirty = true;
    let mut size = terminal::size()?;
    // LYST_EVENT_LOG=path records every input event, for diagnosing terminals.
    let mut log = std::env::var_os("LYST_EVENT_LOG").and_then(|p| std::fs::File::create(p).ok());
    while !app.done {
        if dirty {
            app.draw(&mut out)?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(250))? {
            let ev = event::read()?;
            if let Some(f) = &mut log {
                let _ = writeln!(f, "{ev:?}");
            }
            match ev {
                Event::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k),
                // Motion events arrive constantly; only redraw when something changed.
                Event::Mouse(m) if !app.on_mouse(m) => continue,
                Event::Mouse(_) => {}
                Event::Resize(w, h) => {
                    size = (w, h);
                    app.resize(w, h);
                }
                _ => continue,
            }
            if let Some(path) = app.take_edit() {
                // Hand the terminal to the editor, then take it back.
                leave_terminal();
                let result = external::edit(&path);
                enter_terminal(app.mouse)?;
                app.edited(&path, result);
            }
            // A slow preprocessor (or the editor) may have swallowed a resize.
            let now = terminal::size()?;
            if now != size {
                size = now;
                app.resize(now.0, now.1);
            }
            dirty = true;
        } else if app.tick() {
            dirty = true;
        }
    }
    Ok(())
}

fn enter_terminal(mouse: bool) -> io::Result<()> {
    terminal::enable_raw_mode()?;
    preprocess::set_feedback(preprocess::Feedback::Screen);
    execute!(io::stdout(), terminal::EnterAlternateScreen, terminal::DisableLineWrap, cursor::Hide)?;
    if mouse {
        execute!(io::stdout(), event::EnableMouseCapture)?;
    }
    Ok(())
}

fn leave_terminal() {
    preprocess::set_feedback(preprocess::Feedback::None);
    let _ = execute!(
        io::stdout(),
        event::DisableMouseCapture,
        cursor::Show,
        terminal::EnableLineWrap,
        terminal::LeaveAlternateScreen
    );
    let _ = terminal::disable_raw_mode();
}

fn describe(s: &Source) -> String {
    match s {
        Source::Stdin => "<stdin>".into(),
        Source::Path(p) => p.display().to_string(),
    }
}

fn cat(sources: &[Source]) -> ExitCode {
    let mut out = io::stdout().lock();
    let mut code = ExitCode::SUCCESS;
    for s in sources {
        match s.load() {
            Ok(d) => {
                if out.write_all(d.bytes()).is_err() {
                    return code;
                }
            }
            Err(e) => {
                eprintln!("lyst: {}: {e}", describe(s));
                code = ExitCode::FAILURE;
            }
        }
    }
    code
}
