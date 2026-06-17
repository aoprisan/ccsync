//! Interactive terminal UI (`ccsync tui`). A small tabbed ratatui app that
//! presents three views over the existing backup machinery:
//!
//! 1. **What's backed up** — a dry-run snapshot summary of `~/.claude`.
//! 2. **Local backups** — the unified list from [`crate::backups`].
//! 3. **Upload** — run a git push or write an encrypted archive.
//!
//! The TUI is purely a presentation + orchestration layer: every action calls
//! into the same `snapshot`/`git`/`archive` code the CLI uses. Long actions run
//! synchronously and report through a status line; errors are caught and shown
//! rather than tearing down the terminal.

use std::collections::BTreeMap;
use std::io::{self, Stdout};

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use ratatui::prelude::*;
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph, Tabs, Wrap};

use crate::backups::{self, BackupKind, LocalBackup};
use crate::config::Config;
use crate::manifest::Manifest;
use crate::snapshot::{self, SnapshotOptions};
use crate::theme::{Theme, ThemeVariant};
use crate::{archive, git, paths};

const TAB_TITLES: [&str; 3] = ["What's backed up", "Local backups", "Upload"];
const UPLOAD_ACTIONS: [&str; 2] = ["Push to git remote", "Export encrypted archive"];

/// Cached result of the dry-run snapshot used by the first tab.
enum CaptureSummary {
    Ready(Vec<String>),
    Failed(String),
}

struct App {
    config: Config,
    theme_variant: ThemeVariant,
    theme: Theme,
    tab: usize,
    capture: CaptureSummary,
    backups: Vec<LocalBackup>,
    backups_state: ListState,
    upload_selected: usize,
    status: String,
    should_quit: bool,
}

impl App {
    fn new(config: Config) -> Self {
        let variant = ThemeVariant::SolarizedDark;
        let mut app = App {
            config,
            theme_variant: variant,
            theme: variant.theme(),
            tab: 0,
            capture: CaptureSummary::Failed(String::new()),
            backups: Vec::new(),
            backups_state: ListState::default(),
            upload_selected: 0,
            status: "↹ switch tabs · ↑/↓ move · r refresh · t theme · q quit".to_string(),
            should_quit: false,
        };
        app.refresh();
        app
    }

    /// Recompute the capture summary and re-enumerate local backups.
    fn refresh(&mut self) {
        self.capture = compute_capture(&self.config);
        self.backups = backups::collect(&self.config);
        if self.backups.is_empty() {
            self.backups_state.select(None);
        } else {
            let idx = self
                .backups_state
                .selected()
                .unwrap_or(0)
                .min(self.backups.len() - 1);
            self.backups_state.select(Some(idx));
        }
    }

    /// Cycle to the next color theme.
    fn toggle_theme(&mut self) {
        self.theme_variant = self.theme_variant.next();
        self.theme = self.theme_variant.theme();
        self.status = format!("theme: {}", self.theme.name);
    }

    fn next_tab(&mut self) {
        self.tab = (self.tab + 1) % TAB_TITLES.len();
    }

    fn prev_tab(&mut self) {
        self.tab = (self.tab + TAB_TITLES.len() - 1) % TAB_TITLES.len();
    }

    fn move_selection(&mut self, delta: isize) {
        match self.tab {
            1 => {
                if self.backups.is_empty() {
                    return;
                }
                let len = self.backups.len() as isize;
                let cur = self.backups_state.selected().unwrap_or(0) as isize;
                let next = (cur + delta).rem_euclid(len) as usize;
                self.backups_state.select(Some(next));
            }
            2 => {
                let len = UPLOAD_ACTIONS.len() as isize;
                let cur = self.upload_selected as isize;
                self.upload_selected = (cur + delta).rem_euclid(len) as usize;
            }
            _ => {}
        }
    }

    /// Run the currently-selected upload action.
    fn run_upload(&mut self) {
        let result = match self.upload_selected {
            0 => self.push_to_git(),
            1 => self.export_archive(),
            _ => Ok("nothing to do".to_string()),
        };
        match result {
            Ok(msg) => self.status = msg,
            Err(e) => self.status = format!("error: {e:#}"),
        }
        // An upload may have changed what local backups exist.
        self.refresh();
    }

    fn push_to_git(&mut self) -> Result<String> {
        let staging = paths::staging_dir()?;
        self.stage_fresh_snapshot(&staging)?;
        let remote = git::resolve_remote(None, self.config.remote.as_deref())?;
        git::push(&remote, &staging)?;
        Ok(format!("pushed snapshot to {remote}"))
    }

    fn export_archive(&mut self) -> Result<String> {
        let pass = archive::passphrase_from_env()?;
        let staging = paths::staging_dir()?;
        self.stage_fresh_snapshot(&staging)?;
        let dir = paths::backups_dir()?;
        std::fs::create_dir_all(&dir)?;
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let out = dir.join(format!("claude-backup-{ts}.tar.gz.age"));
        archive::create(&staging, &out, &pass)?;
        Ok(format!("wrote encrypted archive to {}", out.display()))
    }

    /// Build a fresh (non-dry-run) snapshot into `staging`, mirroring the
    /// `backup`/`export` CLI paths so an upload always reflects current state.
    fn stage_fresh_snapshot(&self, staging: &std::path::Path) -> Result<()> {
        let claude = paths::claude_dir()?;
        let opts = SnapshotOptions::new(false, false, &self.config);
        snapshot::build(&claude, staging, &self.config, &opts)?;
        Ok(())
    }
}

/// Run a dry-run snapshot and format it into display lines, or capture the
/// error (e.g. a detected secret) so the user sees why a backup would abort.
fn compute_capture(config: &Config) -> CaptureSummary {
    let claude = match paths::claude_dir() {
        Ok(c) => c,
        Err(e) => return CaptureSummary::Failed(format!("{e}")),
    };
    let staging = match paths::staging_dir() {
        Ok(s) => s,
        Err(e) => return CaptureSummary::Failed(format!("{e}")),
    };
    let opts = SnapshotOptions::new(true, true, config);
    match snapshot::build(&claude, &staging, config, &opts) {
        Ok(manifest) => CaptureSummary::Ready(summarize_manifest(&manifest, &claude)),
        Err(e) => CaptureSummary::Failed(format!("{e:#}")),
    }
}

/// Group a manifest's files by their top-level component for a compact summary.
fn summarize_manifest(m: &Manifest, claude: &std::path::Path) -> Vec<String> {
    let total: u64 = m.files.iter().map(|f| f.size).sum();
    let mut lines = vec![
        format!("source: {}", claude.display()),
        format!(
            "{} files · {} · {} session root(s)",
            m.files.len(),
            backups::human_size(total),
            m.project_roots.len(),
        ),
        String::new(),
        "included:".to_string(),
    ];

    let mut groups: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    for f in &m.files {
        let top = f.rel_path.split('/').next().unwrap_or(&f.rel_path).to_string();
        let e = groups.entry(top).or_insert((0, 0));
        e.0 += 1;
        e.1 += f.size;
    }
    if groups.is_empty() {
        lines.push("  (nothing — ~/.claude is empty or fully excluded)".to_string());
    }
    for (name, (count, size)) in groups {
        lines.push(format!("  {name}  —  {count} file(s), {}", backups::human_size(size)));
    }
    lines
}

/// Launch the interactive terminal UI.
pub fn run(config: &Config) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let app = App::new(config.clone());
    let result = run_loop(&mut terminal, app);
    restore_terminal(&mut terminal)?;
    result
}

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn setup_terminal() -> Result<Tui> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Tui) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn run_loop(terminal: &mut Tui, mut app: App) -> Result<()> {
    loop {
        terminal.draw(|f| ui(f, &mut app))?;
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                KeyCode::Tab | KeyCode::Right => app.next_tab(),
                KeyCode::BackTab | KeyCode::Left => app.prev_tab(),
                KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
                KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
                KeyCode::Char('r') => {
                    app.refresh();
                    app.status = "refreshed".to_string();
                }
                KeyCode::Char('t') => app.toggle_theme(),
                KeyCode::Enter => {
                    if app.tab == 2 {
                        app.status = "working…".to_string();
                        terminal.draw(|f| ui(f, &mut app))?;
                        app.run_upload();
                    }
                }
                _ => {}
            }
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(f.area());

    // Paint the whole screen with the theme background first, then the tabs.
    // These borrows of `app.theme` are kept short so the body render below can
    // take `app` mutably.
    {
        let t = &app.theme;
        f.render_widget(Block::default().style(Style::default().bg(t.bg)), f.area());
        let tabs = Tabs::new(TAB_TITLES.iter().map(|s| Line::from(*s)).collect::<Vec<_>>())
            .block(t.panel(" ccsync "))
            .style(t.text())
            .select(app.tab)
            .highlight_style(t.selection());
        f.render_widget(tabs, chunks[0]);
    }

    match app.tab {
        0 => render_capture(f, chunks[1], app),
        1 => render_backups(f, chunks[1], app),
        _ => render_upload(f, chunks[1], app),
    }

    let t = &app.theme;
    let status = Paragraph::new(app.status.clone())
        .style(t.text())
        .block(t.panel(" status "))
        .wrap(Wrap { trim: true });
    f.render_widget(status, chunks[2]);
}

fn render_capture(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let (text, style) = match &app.capture {
        CaptureSummary::Ready(lines) => (lines.join("\n"), t.text()),
        CaptureSummary::Failed(e) => (
            format!("snapshot would abort:\n\n{e}"),
            Style::default().fg(t.error).bg(t.bg),
        ),
    };
    let p = Paragraph::new(text)
        .style(style)
        .block(t.panel(" what would be backed up "))
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}

fn render_backups(f: &mut Frame, area: Rect, app: &mut App) {
    let t = &app.theme;
    if app.backups.is_empty() {
        let p = Paragraph::new("No local backups found yet.\n\nUse the Upload tab to push to git or write an encrypted archive.")
            .style(t.text())
            .block(t.panel(" local backups "))
            .wrap(Wrap { trim: true });
        f.render_widget(p, area);
        return;
    }

    let items: Vec<ListItem> = app
        .backups
        .iter()
        .map(|b| ListItem::new(backup_lines(b, t)))
        .collect();
    let title = format!(" local backups ({}) ", app.backups.len());
    let list = List::new(items)
        .style(t.text())
        .block(t.panel(&title))
        .highlight_style(t.selection())
        .highlight_symbol("▌ ");
    f.render_stateful_widget(list, area, &mut app.backups_state);
}

fn backup_lines(b: &LocalBackup, t: &Theme) -> Vec<Line<'static>> {
    let when = b.created_at.clone().unwrap_or_else(|| "—".to_string());
    let header = format!("[{:<11}] {}", b.kind.label(), b.label);
    let color = match b.kind {
        BackupKind::Staged => t.staged,
        BackupKind::GitCommit => t.git,
        BackupKind::Archive => t.archive,
        BackupKind::RestoreBackup => t.restore,
    };
    vec![
        Line::from(Span::styled(header, Style::default().fg(color))),
        Line::from(Span::styled(
            format!("    {when}  ·  {}", b.detail),
            Style::default().fg(t.dim),
        )),
    ]
}

fn render_upload(f: &mut Frame, area: Rect, app: &App) {
    let t = &app.theme;
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(UPLOAD_ACTIONS.len() as u16 + 2), Constraint::Min(1)])
        .split(area);

    let items: Vec<ListItem> = UPLOAD_ACTIONS
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let marker = if i == app.upload_selected { "▌ " } else { "  " };
            let style = if i == app.upload_selected {
                Style::default().fg(t.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.fg)
            };
            ListItem::new(Line::from(Span::styled(format!("{marker}{label}"), style)))
        })
        .collect();
    let list = List::new(items)
        .style(t.text())
        .block(t.panel(" upload (Enter to run) "));
    f.render_widget(list, chunks[0]);

    let remote = app
        .config
        .remote
        .clone()
        .unwrap_or_else(|| "(none — set with `ccsync init --remote …`)".to_string());
    let pass_set = std::env::var("CCSYNC_PASSPHRASE").map(|p| !p.is_empty()).unwrap_or(false);
    let help = format!(
        "Each action first captures a fresh snapshot of ~/.claude, then:\n\n\
         • Push to git remote → commits & pushes to the configured remote.\n    \
         remote: {remote}\n\n\
         • Export encrypted archive → writes a timestamped .tar.gz.age into\n    \
         {}\n    \
         CCSYNC_PASSPHRASE: {}",
        paths::backups_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "~/.config/ccsync/backups".to_string()),
        if pass_set { "set" } else { "NOT set — export will fail" },
    );
    let p = Paragraph::new(help)
        .style(t.text())
        .block(t.panel(" details "))
        .wrap(Wrap { trim: false });
    f.render_widget(p, chunks[1]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    /// Each tab renders without panicking against an off-screen backend, under
    /// both color themes.
    #[test]
    fn renders_every_tab() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(Config::default());
        // Both Solarized variants.
        for _ in 0..2 {
            for tab in 0..TAB_TITLES.len() {
                app.tab = tab;
                terminal.draw(|f| ui(f, &mut app)).unwrap();
            }
            app.toggle_theme();
        }
    }
}
