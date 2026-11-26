//! Switches between the file browser and the viewer.

use crate::app::{App, Options, Source};
use crate::browser::{Action, Browser, Prefs};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use std::io::{self, Write};

pub struct Ui {
    browser: Option<Browser>,
    viewer: Option<App>,
    /// Display settings, carried from one viewed file to the next.
    opts: Options,
    /// Browser settings used when a browser is (re)opened.
    prefs: Prefs,
    /// Whether the terminal is reporting mouse events to us.
    pub mouse: bool,
    pub done: bool,
}

impl Ui {
    pub fn with_viewer(app: App, prefs: Prefs) -> Self {
        let opts = app.options().clone();
        Ui { browser: None, viewer: Some(app), opts, prefs, mouse: false, done: false }
    }

    pub fn with_browser(browser: Browser, opts: Options) -> Self {
        let prefs = browser.prefs();
        Ui { browser: Some(browser), viewer: None, opts, prefs, mouse: false, done: false }
    }

    pub fn draw(&mut self, out: &mut impl Write) -> io::Result<()> {
        match (&mut self.viewer, &mut self.browser) {
            (Some(v), _) => v.draw(out),
            (None, Some(b)) => b.draw(out),
            (None, None) => Ok(()),
        }
    }

    pub fn resize(&mut self, w: u16, h: u16) {
        if let Some(v) = &mut self.viewer {
            v.resize(w, h);
        }
        if let Some(b) = &mut self.browser {
            b.resize(w, h);
        }
    }

    pub fn tick(&mut self) -> bool {
        match (&mut self.viewer, &mut self.browser) {
            (Some(v), _) => v.tick(),
            (None, Some(b)) => b.tick(),
            (None, None) => false,
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::F(9) {
            return self.toggle_mouse();
        }
        if let Some(v) = &mut self.viewer {
            v.on_key(k);
            if v.quit {
                self.close_viewer();
            }
            return;
        }
        if let Some(b) = &mut self.browser {
            match b.on_key(k) {
                Action::Quit => self.done = true,
                Action::Open => self.open_selected(),
                Action::None => {}
            }
        }
    }

    /// Returns true if the event changed anything.
    pub fn on_mouse(&mut self, m: MouseEvent) -> bool {
        if !self.mouse {
            return false;
        }
        if let Some(v) = &mut self.viewer {
            let handled = v.on_mouse(m);
            if v.quit {
                self.close_viewer();
            }
            return handled;
        }
        let Some(b) = &mut self.browser else { return false };
        match b.on_mouse(m) {
            None => false,
            Some(Action::Open) => {
                self.open_selected();
                true
            }
            Some(Action::Quit) => {
                self.done = true;
                true
            }
            Some(Action::None) => true,
        }
    }

    /// A file the viewer or browser wants edited, if any.
    pub fn take_edit(&mut self) -> Option<std::path::PathBuf> {
        match (&mut self.viewer, &mut self.browser) {
            (Some(v), _) => v.want_edit.take(),
            (None, Some(b)) => b.want_edit.take(),
            (None, None) => None,
        }
    }

    /// After the editor run for `take_edit` exits.
    pub fn edited(&mut self, path: &std::path::Path, result: io::Result<std::process::ExitStatus>) {
        match (&mut self.viewer, &mut self.browser) {
            (Some(v), _) => v.edited(result),
            (None, Some(b)) => b.edited(path, result),
            (None, None) => {}
        }
    }

    fn toggle_mouse(&mut self) {
        self.mouse = !self.mouse;
        let r = if self.mouse {
            execute!(io::stdout(), EnableMouseCapture)
        } else {
            execute!(io::stdout(), DisableMouseCapture)
        };
        let msg = match (r, self.mouse) {
            (Err(e), _) => format!("Mouse: {e}"),
            (Ok(()), true) => "Mouse on (F9 to turn off and select text)".into(),
            (Ok(()), false) => "Mouse off: the terminal can select text (F9 to turn back on)".into(),
        };
        match (&mut self.viewer, &mut self.browser) {
            (Some(v), _) => v.flash(msg),
            (None, Some(b)) => b.flash(msg),
            _ => {}
        }
    }

    fn close_viewer(&mut self) {
        let mut v = self.viewer.take().expect("viewer is open");
        self.opts = v.options().clone();
        self.opts.follow = false;
        if let Some((dir, select)) = v.want_browser.take() {
            if let Some(b) = &self.browser {
                self.prefs = b.prefs();
            }
            match Browser::new(&dir, self.opts.classic, self.prefs) {
                Ok(mut b) => {
                    if let Some(name) = select {
                        b.select_name(&name);
                    }
                    self.browser = Some(b);
                }
                Err(e) => {
                    v.quit = false;
                    v.error(format!("{}: {e}", dir.display()));
                    self.viewer = Some(v);
                }
            }
            return;
        }
        match &mut self.browser {
            Some(b) if !v.quit_all => {
                b.refresh();
                if let Some(p) = v.current_path() {
                    b.select_path(p);
                }
            }
            _ => self.done = true,
        }
    }

    fn open_selected(&mut self) {
        let b = self.browser.as_mut().expect("browser is open");
        let (files, idx) = b.file_list();
        let sources: Vec<Source> = files.into_iter().map(Source::Path).collect();
        match sources[idx].load() {
            Ok(doc) => {
                let mut app = App::new(sources, idx, doc, self.opts.clone());
                app.from_browser = true;
                self.viewer = Some(app);
            }
            Err(e) => b.error(format!("{e}")),
        }
    }
}
