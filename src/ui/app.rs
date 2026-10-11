//! Application state machine: screens, modal dialogs, background jobs and
//! the full keymap. Pure state transitions — rendering lives in `views.rs`.

use std::collections::HashSet;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::CrosstermBackend, Terminal};

use metla::action::{self, ActionPlan, WorkProgress};
use metla::arena::FileArena;
use metla::format::{shorten_home, HumanBytes, HumanCount};
use metla::suggest::{self, Suggestion};
use metla::types::{flags, Bytes, NodeId, NodeKind, UnixTime};
use metla::walk::{ScanOptions, ScanProgress, ScanStats};
use metla::MetlaError;

use super::theme::Palette;

pub type ScanResult = Result<(FileArena, ScanStats), MetlaError>;

pub struct ScanJob {
    pub progress: Arc<ScanProgress>,
    pub rx: mpsc::Receiver<ScanResult>,
    pub started: Instant,
}

pub struct WorkJob {
    pub progress: Arc<WorkProgress>,
    pub rx: mpsc::Receiver<action::ActionReport>,
    pub started: Instant,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Screen {
    Browse,
    Hogs,
    Suggestions,
    Help,
}

pub enum Modal {
    None,
    Confirm(Box<ActionPlan>),
    Working,
    Error(String),
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum SortKey {
    Size,
    Name,
    Modified,
    Items,
}

#[derive(Copy, Clone)]
pub struct SortBy {
    pub key: SortKey,
    pub asc: bool,
}

impl Default for SortBy {
    fn default() -> Self {
        SortBy {
            key: SortKey::Size,
            asc: false,
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum SizeMode {
    Logical,
    Physical,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Good,
    Bad,
}

pub struct Browser {
    pub cwd: NodeId,
    pub sel: NodeId,
    pub sort: SortBy,
    pub marked: HashSet<NodeId>,
    /// Sorted+filtered live children of `cwd`, refreshed on every frame.
    pub ids: Vec<NodeId>,
    pub sel_pos: usize,
    pub max_child: Bytes,
}

pub struct HogsState {
    pub rows: Vec<NodeId>,
    pub sel: usize,
}

pub struct SuggestState {
    pub rows: Vec<Suggestion>,
    pub sel: usize,
    pub checked: HashSet<usize>,
}

pub struct Scratch {
    names: Vec<metla::arena::NameRef>,
    path: PathBuf,
    text: String,
}

pub struct App {
    pub quit: bool,
    pub pal: Palette,
    pub screen: Screen,
    pub modal: Modal,
    pub root_prefix: PathBuf,
    pub archive_dir: PathBuf,
    pub home: PathBuf,
    pub arena: Option<FileArena>,
    pub stats: Option<ScanStats>,
    pub scan_job: Option<ScanJob>,
    pub work_job: Option<WorkJob>,
    pub browser: Browser,
    pub hogs: HogsState,
    pub sug: SuggestState,
    pub filter: Option<String>,
    pub filter_input: bool,
    pub size_mode: SizeMode,
    pub freed_total: Bytes,
    pub toast: Option<(String, ToastKind, Instant)>,
    pub scratch: Scratch,
    pub rules: suggest::Rules,
}

pub struct AppOptions {
    pub root: PathBuf,
    pub archive_dir: PathBuf,
    pub light: bool,
    pub dedup_hardlinks: bool,
}

const HOGS_KEEP: usize = 300;

impl App {
    pub fn new(opts: AppOptions) -> App {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let (tx, rx) = mpsc::channel();
        let progress = Arc::new(ScanProgress::new());
        let skip_paths = metla::walk::default_skip_paths(&opts.root, &opts.archive_dir);
        let scan_opts = ScanOptions {
            root: opts.root.clone(),
            dedup_hardlinks: opts.dedup_hardlinks,
            skip_paths,
        };
        let prog = Arc::clone(&progress);
        std::thread::Builder::new()
            .name("metla-scan".into())
            .spawn(move || {
                let res = metla::walk::run_scan(&scan_opts, &prog);
                let _ = tx.send(res);
            })
            .expect("spawn scanner thread");

        App {
            quit: false,
            pal: if opts.light {
                Palette::LIGHT
            } else {
                Palette::DARK
            },
            screen: Screen::Browse,
            modal: Modal::None,
            root_prefix: opts.root,
            archive_dir: opts.archive_dir,
            home,
            arena: None,
            stats: None,
            scan_job: Some(ScanJob {
                progress,
                rx,
                started: Instant::now(),
            }),
            work_job: None,
            browser: Browser {
                cwd: NodeId::NONE,
                sel: NodeId::NONE,
                sort: SortBy::default(),
                marked: HashSet::new(),
                ids: Vec::with_capacity(256),
                sel_pos: 0,
                max_child: Bytes(0),
            },
            hogs: HogsState {
                rows: Vec::new(),
                sel: 0,
            },
            sug: SuggestState {
                rows: Vec::new(),
                sel: 0,
                checked: HashSet::new(),
            },
            filter: None,
            filter_input: false,
            // Physical by default: users want what the disk actually holds,
            // not apparent sizes inflated by cloud placeholders and clones.
            size_mode: SizeMode::Physical,
            freed_total: Bytes(0),
            toast: None,
            scratch: Scratch {
                names: Vec::with_capacity(64),
                path: PathBuf::with_capacity(256),
                text: String::with_capacity(128),
            },
            rules: suggest::Rules::default(),
        }
    }

    // ------------------------------------------------------------- event loop

    pub fn run(mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
        loop {
            terminal.draw(|f| super::render(f, &mut self))?;
            self.poll_background();
            if self.quit {
                return Ok(());
            }
            if event::poll(Duration::from_millis(100))? {
                if let crossterm::event::Event::Key(key) = event::read()? {
                    if key.kind == crossterm::event::KeyEventKind::Press {
                        self.handle_key(key);
                    }
                }
            }
        }
    }

    fn poll_background(&mut self) {
        // scan
        if let Some(job) = self.scan_job.take() {
            match job.rx.try_recv() {
                Ok(res) => match res {
                    Ok((arena, stats)) => {
                        self.arena = Some(arena);
                        self.stats = Some(stats);
                        self.browser.cwd = self
                            .arena
                            .as_ref()
                            .map(|a| a.root())
                            .unwrap_or(NodeId::NONE);
                        self.browser.sel = NodeId::NONE;
                        self.rebuild_hogs();
                        self.recompute_suggestions();
                        let (a, s) = (&self.arena.as_ref().unwrap(), self.stats.as_ref().unwrap());
                        let mut msg = format!(
                            "indexed {} files · {} in {:.1}s",
                            HumanCount(s.files),
                            HumanBytes(a.node(a.root()).subtree_size.0),
                            s.elapsed.as_secs_f64()
                        );
                        if s.errors > 0 {
                            msg.push_str(&format!(" · {} errors", s.errors));
                        }
                        self.set_toast(msg, ToastKind::Good);
                    }
                    Err(e) => {
                        self.modal = Modal::Error(format!("scan failed: {e}"));
                    }
                },
                Err(mpsc::TryRecvError::Empty) => self.scan_job = Some(job),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.modal = Modal::Error("scan thread died without a result".into());
                }
            }
        }
        // work
        if self.work_job.is_some() {
            let job = self.work_job.take().expect("checked");
            match job.rx.try_recv() {
                Ok(report) => self.on_work_done(report),
                Err(mpsc::TryRecvError::Empty) => self.work_job = Some(job),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.modal = Modal::Error("worker thread died without a result".into());
                }
            }
        }
        // expire toasts
        if let Some((_, _, at)) = self.toast {
            if at.elapsed() > Duration::from_secs(5) {
                self.toast = None;
            }
        }
    }

    fn on_work_done(&mut self, report: action::ActionReport) {
        // THE critical reset: the worker finished, so the Working modal must
        // die with it — otherwise an invisible modal keeps swallowing every
        // key press while the screen looks perfectly normal.
        if matches!(self.modal, Modal::Working) {
            self.modal = Modal::None;
        }
        if let Some(arena) = self.arena.as_mut() {
            for id in &report.removed {
                arena.remove_subtree(*id);
            }
        }
        self.freed_total = self.freed_total + report.freed_logical;
        if let Some(arena) = self.arena.as_ref() {
            self.browser.marked.retain(|&id| !arena.is_freed(id));
            // climb out of any deleted directory
            let mut cwd = self.browser.cwd;
            while cwd.is_some() && arena.is_freed(cwd) {
                cwd = arena.parent(cwd);
            }
            self.browser.cwd = cwd;
            if !self.browser.sel.is_some() || arena.is_freed(self.browser.sel) {
                self.browser.sel = NodeId::NONE;
            }
        }
        self.rebuild_hogs();
        self.recompute_suggestions();

        let mut msg = String::new();
        if report.cancelled {
            msg.push_str("cancelled · ");
        }
        if let Some((dest, size)) = &report.archived {
            msg.push_str(&format!(
                "archived {} to {} · ",
                HumanBytes(*size),
                dest.display()
            ));
        }
        msg.push_str(&format!("freed {}", HumanBytes(report.freed_logical.0)));
        if report.cancelled {
            // danger color on purpose: the plan did NOT fully run
            self.set_toast(msg, ToastKind::Bad);
        } else if report.errors.is_empty() {
            self.set_toast(msg, ToastKind::Good);
        } else {
            let head = format!("{msg} · {} failed", report.errors.len());
            self.modal = Modal::Error(format!(
                "{head}\n\n{}",
                report
                    .errors
                    .iter()
                    .take(6)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
    }

    // ---------------------------------------------------------------- actions

    pub fn set_toast(&mut self, msg: String, kind: ToastKind) {
        self.toast = Some((msg, kind, Instant::now()));
    }

    /// Targets for d/a in the browser: marks if any, else the selection.
    fn browser_targets(&self) -> Vec<NodeId> {
        if self.browser.marked.is_empty() {
            if self.browser.sel.is_some() {
                vec![self.browser.sel]
            } else {
                Vec::new()
            }
        } else {
            self.browser.marked.iter().copied().collect()
        }
    }

    pub fn begin_action(&mut self, act: action::PlannedAction, targets: &[NodeId]) {
        let Some(arena) = self.arena.as_ref() else {
            return;
        };
        if targets.is_empty() {
            self.set_toast("nothing selected".into(), ToastKind::Info);
            return;
        }
        let plan = action::build_plan(
            arena,
            &self.root_prefix,
            targets,
            act,
            &self.archive_dir,
            &self.home,
        );
        if plan.is_empty() {
            let msg = if plan.warnings.is_empty() {
                "nothing to do".to_string()
            } else {
                format!(
                    "all {} target(s) refused:\n{}",
                    plan.warnings.len(),
                    plan.warnings.join("\n")
                )
            };
            self.modal = Modal::Error(msg);
            return;
        }
        self.modal = Modal::Confirm(Box::new(plan));
    }

    fn confirm_plan(&mut self) {
        if let Modal::Confirm(plan) = std::mem::replace(&mut self.modal, Modal::None) {
            if plan.action == action::PlannedAction::Archive {
                let _ = std::fs::create_dir_all(&self.archive_dir);
            }
            let progress = Arc::new(WorkProgress::new());
            let (tx, rx) = mpsc::channel();
            let archive_dir = self.archive_dir.clone();
            let log_path = operation_log_path();
            let verb = plan.action.verb();
            // Paths are snapshot before the plan is consumed by the worker.
            let item_paths: Vec<String> = plan
                .items
                .iter()
                .map(|i| i.path.display().to_string())
                .collect();
            let prog = Arc::clone(&progress);
            self.work_job = Some(WorkJob {
                progress,
                rx,
                started: Instant::now(),
            });
            self.modal = Modal::Working;
            std::thread::Builder::new()
                .name("metla-work".into())
                .spawn(move || {
                    let report = action::execute(*plan, &archive_dir, &prog);
                    append_operation_log(&log_path, verb, &item_paths, &report);
                    let _ = tx.send(report);
                })
                .expect("spawn worker thread");
        }
    }

    pub fn suggestion_targets(&self, all: bool) -> Vec<NodeId> {
        let s = &self.sug;
        if all {
            return s.rows.iter().map(|r| r.target).collect();
        }
        if !s.checked.is_empty() {
            return s
                .rows
                .iter()
                .enumerate()
                .filter(|(i, _)| s.checked.contains(i))
                .map(|(_, r)| r.target)
                .collect();
        }
        s.rows
            .get(s.sel)
            .map(|r| vec![r.target])
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------ views

    pub fn rebuild_hogs(&mut self) {
        let Some(arena) = self.arena.as_ref() else {
            return;
        };
        let mut heap = std::collections::BinaryHeap::with_capacity(HOGS_KEEP + 1);
        for idx in 0..arena.len() {
            let n = arena.node(NodeId(idx as u32));
            if n.kind != NodeKind::File || n.flags & (flags::FREED | flags::HARDLINK) != 0 {
                continue;
            }
            let key = match self.size_mode {
                SizeMode::Logical => n.size.0,
                SizeMode::Physical => n.physical.0,
            };
            if key == 0 {
                continue;
            }
            if heap.len() < HOGS_KEEP {
                heap.push(std::cmp::Reverse((key, idx as u32)));
            } else if heap
                .peek()
                .is_some_and(|&std::cmp::Reverse((k, _))| key > k)
            {
                heap.pop();
                heap.push(std::cmp::Reverse((key, idx as u32)));
            }
        }
        let mode = self.size_mode;
        let mut rows: Vec<NodeId> = heap
            .into_iter()
            .map(|std::cmp::Reverse((_, i))| NodeId(i))
            .collect();
        rows.sort_unstable_by(|&a, &b| {
            let ka = hog_key(arena, a, mode);
            let kb = hog_key(arena, b, mode);
            kb.cmp(&ka)
        });
        self.hogs = HogsState { rows, sel: 0 };
    }

    pub fn recompute_suggestions(&mut self) {
        let Some(arena) = self.arena.as_ref() else {
            return;
        };
        let now = UnixTime(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        );
        let mut rows = std::mem::take(&mut self.sug.rows);
        suggest::run(arena, &self.rules, now, &mut rows);
        self.sug = SuggestState {
            rows,
            sel: 0,
            checked: HashSet::new(),
        };
    }

    /// Collect, filter and sort the current directory's children into
    /// `browser.ids`; resolve the selection. Called every frame — everything
    /// reuses scratch buffers, so this is allocation-free after warm-up.
    pub fn refresh_browse(&mut self) {
        let size_mode = self.size_mode;
        let Self {
            arena,
            browser,
            filter,
            scratch,
            ..
        } = self;
        let Some(arena) = arena.as_ref() else { return };
        while browser.cwd.is_some() && arena.is_freed(browser.cwd) {
            browser.cwd = arena.parent(browser.cwd);
        }
        if !browser.cwd.is_some() {
            browser.cwd = arena.root();
        }
        browser.ids.clear();
        browser.ids.extend(arena.children(browser.cwd));
        if let Some(f) = filter.as_ref() {
            let f = f.as_str();
            browser.ids.retain(|&id| {
                let name = arena.name(id);
                scratch.text.clear();
                let bytes = name.as_bytes();
                if bytes.is_ascii() {
                    for &b in bytes {
                        scratch.text.push((b.to_ascii_lowercase()) as char);
                    }
                } else {
                    scratch
                        .text
                        .push_str(&name.to_string_lossy().to_lowercase());
                }
                scratch.text.contains(f)
            });
        }
        let sort = browser.sort;
        browser.max_child = Bytes(0);
        for &id in &browser.ids {
            let v = subtree_value(arena.node(id), size_mode);
            if v.0 > browser.max_child.0 {
                browser.max_child = v;
            }
        }
        browser
            .ids
            .sort_unstable_by(|&a, &b| cmp_children(arena, a, b, sort, size_mode));
        browser.sel_pos = browser
            .ids
            .iter()
            .position(|&id| id == browser.sel)
            .unwrap_or(0);
        if browser.sel_pos < browser.ids.len() {
            browser.sel = browser.ids[browser.sel_pos];
        } else {
            browser.sel = browser.ids.first().copied().unwrap_or(NodeId::NONE);
            browser.sel_pos = 0;
        }
    }

    // ------------------------------------------------------------------ input

    fn request_quit(&mut self) {
        if matches!(self.modal, Modal::Working) {
            self.cancel_work();
            return;
        }
        self.quit = true;
    }

    /// `q` during a running operation asks the worker to stop between items.
    /// Quitting mid-operation is never allowed — the terminal must stay up
    /// until the worker thread reports back.
    fn cancel_work(&mut self) {
        if let Some(job) = &self.work_job {
            if !job.progress.is_cancelled() {
                job.progress.request_cancel();
                self.set_toast(
                    "cancel requested — finishing the current item, then stopping".into(),
                    ToastKind::Info,
                );
            } else {
                self.set_toast(
                    "cancel already requested — waiting for the worker".into(),
                    ToastKind::Info,
                );
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }
        if let Modal::Error(_) = self.modal {
            self.modal = Modal::None;
            return;
        }
        if matches!(self.modal, Modal::Working) {
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                self.cancel_work();
            }
            return;
        }
        if let Modal::Confirm(_) = self.modal {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_plan(),
                KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => self.modal = Modal::None,
                _ => {}
            }
            return;
        }

        // Still scanning: only quit / theme.
        if self.arena.is_none() {
            match key.code {
                KeyCode::Char('q') => self.request_quit(),
                KeyCode::Char('t') => self.pal.toggle(),
                _ => {}
            }
            return;
        }

        // Filter input mode swallows everything.
        if self.filter_input {
            match key.code {
                KeyCode::Esc => {
                    self.filter = None;
                    self.filter_input = false;
                }
                KeyCode::Enter => self.filter_input = false,
                KeyCode::Backspace => {
                    if let Some(f) = self.filter.as_mut() {
                        f.pop();
                        if f.is_empty() {
                            self.filter = None;
                            self.filter_input = false;
                        }
                    }
                }
                KeyCode::Char(c) => {
                    let lower = c.to_ascii_lowercase();
                    self.filter.get_or_insert_with(String::new).push(lower);
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Char('t') => self.pal.toggle(),
            KeyCode::Char('1') => self.screen = Screen::Browse,
            KeyCode::Char('2') => self.screen = Screen::Hogs,
            KeyCode::Char('3') => self.screen = Screen::Suggestions,
            KeyCode::Char('?') => {
                self.screen = if self.screen == Screen::Help {
                    Screen::Browse
                } else {
                    Screen::Help
                };
            }
            KeyCode::Char('q') => self.request_quit(),
            _ => match self.screen {
                Screen::Browse => self.handle_browse(key),
                Screen::Hogs => self.handle_hogs(key),
                Screen::Suggestions => self.handle_suggestions(key),
                Screen::Help => {}
            },
        }
    }

    fn handle_browse(&mut self, key: KeyEvent) {
        let n = self.browser.ids.len();
        let pos = self.browser.sel_pos;
        match key.code {
            KeyCode::Char(' ') => {
                let sel = self.browser.sel;
                if sel.is_some() && !self.browser.marked.remove(&sel) {
                    self.browser.marked.insert(sel);
                }
            }
            KeyCode::Char('X') => self.browser.marked.clear(),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-(visible_rows().max(1) as isize)),
            KeyCode::PageDown => self.move_sel(visible_rows().max(1) as isize),
            KeyCode::Home | KeyCode::Char('g') => {
                if n > 0 {
                    self.browser.sel = self.browser.ids[0];
                }
            }
            KeyCode::End | KeyCode::Char('G') => {
                if n > 0 {
                    self.browser.sel = self.browser.ids[n - 1];
                }
            }
            KeyCode::Left | KeyCode::Char('h') | KeyCode::Char('u') => self.ascend(),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => self.descend(),
            KeyCode::Char('d') => {
                let t = self.browser_targets();
                self.begin_action(action::PlannedAction::Delete, &t);
            }
            KeyCode::Char('a') => {
                let t = self.browser_targets();
                self.begin_action(action::PlannedAction::Archive, &t);
            }
            KeyCode::Char('s') => self.cycle_sort(),
            KeyCode::Char('/') => {
                self.filter_input = true;
                if self.filter.is_none() {
                    self.filter = Some(String::new());
                }
            }
            KeyCode::Char('p') => self.toggle_size_mode(),
            _ => {
                let _ = (n, pos);
            }
        }
    }

    fn handle_hogs(&mut self, key: KeyEvent) {
        let n = self.hogs.rows.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.hogs.sel = self.hogs.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if n > 0 {
                    self.hogs.sel = (self.hogs.sel + 1).min(n - 1);
                }
            }
            KeyCode::PageUp => self.hogs.sel = self.hogs.sel.saturating_sub(visible_rows()),
            KeyCode::PageDown => {
                self.hogs.sel = (self.hogs.sel + visible_rows()).min(n.saturating_sub(1));
            }
            KeyCode::Home | KeyCode::Char('g') => self.hogs.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.hogs.sel = n.saturating_sub(1),
            KeyCode::Enter => {
                let target = self.hogs.rows.get(self.hogs.sel).copied();
                if let Some(id) = target {
                    self.jump_to(id);
                }
            }
            KeyCode::Char('p') => self.toggle_size_mode(),
            KeyCode::Char('a') => {
                if let Some(&id) = self.hogs.rows.get(self.hogs.sel) {
                    self.begin_action(action::PlannedAction::Archive, &[id]);
                }
            }
            KeyCode::Char('d') => {
                if let Some(&id) = self.hogs.rows.get(self.hogs.sel) {
                    self.begin_action(action::PlannedAction::Delete, &[id]);
                }
            }
            _ => {}
        }
    }

    fn handle_suggestions(&mut self, key: KeyEvent) {
        let n = self.sug.rows.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.sug.sel = self.sug.sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if n > 0 {
                    self.sug.sel = (self.sug.sel + 1).min(n - 1);
                }
            }
            KeyCode::PageUp => self.sug.sel = self.sug.sel.saturating_sub(visible_rows()),
            KeyCode::PageDown => {
                self.sug.sel = (self.sug.sel + visible_rows()).min(n.saturating_sub(1));
            }
            KeyCode::Home | KeyCode::Char('g') => self.sug.sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.sug.sel = n.saturating_sub(1),
            KeyCode::Char(' ') => {
                if n > 0 {
                    let i = self.sug.sel;
                    if !self.sug.checked.remove(&i) {
                        self.sug.checked.insert(i);
                    }
                }
            }
            KeyCode::Enter => {
                if let Some(r) = self.sug.rows.get(self.sug.sel) {
                    let t = r.target;
                    self.jump_to(t);
                }
            }
            KeyCode::Char('d') => {
                let t = self.suggestion_targets(false);
                self.begin_action(action::PlannedAction::Delete, &t);
            }
            KeyCode::Char('a') => {
                let t = self.suggestion_targets(false);
                self.begin_action(action::PlannedAction::Archive, &t);
            }
            KeyCode::Char('D') => {
                let t = self.suggestion_targets(true);
                self.begin_action(action::PlannedAction::Delete, &t);
            }
            KeyCode::Char('A') => {
                let t = self.suggestion_targets(true);
                self.begin_action(action::PlannedAction::Archive, &t);
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------- navigation

    fn move_sel(&mut self, delta: isize) {
        let n = self.browser.ids.len();
        if n == 0 {
            return;
        }
        let pos = (self.browser.sel_pos as isize + delta).clamp(0, n as isize - 1) as usize;
        self.browser.sel = self.browser.ids[pos];
    }

    fn descend(&mut self) {
        let arena = self.arena.as_ref().expect("arena present");
        let sel = self.browser.sel;
        if sel.is_some() && arena.node(sel).kind == NodeKind::Dir {
            self.browser.cwd = sel;
            self.browser.sel = NodeId::NONE;
        }
    }

    fn ascend(&mut self) {
        let arena = self.arena.as_ref().expect("arena present");
        let parent = arena.parent(self.browser.cwd);
        if parent.is_some() {
            self.browser.sel = self.browser.cwd;
            self.browser.cwd = parent;
        }
    }

    fn jump_to(&mut self, id: NodeId) {
        let arena = self.arena.as_ref().expect("arena present");
        let parent = arena.parent(id);
        self.browser.cwd = if parent.is_some() {
            parent
        } else {
            arena.root()
        };
        self.browser.sel = id;
        self.screen = Screen::Browse;
    }

    fn cycle_sort(&mut self) {
        self.browser.sort = match (self.browser.sort.key, self.browser.sort.asc) {
            (SortKey::Size, false) => SortBy {
                key: SortKey::Size,
                asc: true,
            },
            (SortKey::Size, true) => SortBy {
                key: SortKey::Name,
                asc: true,
            },
            (SortKey::Name, true) => SortBy {
                key: SortKey::Name,
                asc: false,
            },
            (SortKey::Name, false) => SortBy {
                key: SortKey::Modified,
                asc: false,
            },
            (SortKey::Modified, false) => SortBy {
                key: SortKey::Modified,
                asc: true,
            },
            (SortKey::Modified, true) => SortBy {
                key: SortKey::Items,
                asc: false,
            },
            (SortKey::Items, false) => SortBy {
                key: SortKey::Items,
                asc: true,
            },
            (SortKey::Items, true) => SortBy {
                key: SortKey::Size,
                asc: false,
            },
        };
    }

    fn toggle_size_mode(&mut self) {
        self.size_mode = match self.size_mode {
            SizeMode::Logical => SizeMode::Physical,
            SizeMode::Physical => SizeMode::Logical,
        };
        self.rebuild_hogs();
    }

    // ---------------------------------------------------------------- helpers

    pub fn path_string(&mut self, id: NodeId) -> String {
        let Some(arena) = self.arena.as_ref() else {
            return String::new();
        };
        let Scratch { names, path, .. } = &mut self.scratch;
        arena.build_path(id, &self.root_prefix, names, path);
        let mut out = String::new();
        shorten_home(path, &self.home, &mut out);
        out
    }

    pub fn root_total(&self) -> Bytes {
        self.arena
            .as_ref()
            .map(|a| {
                let r = a.node(a.root());
                match self.size_mode {
                    SizeMode::Logical => r.subtree_size,
                    SizeMode::Physical => r.subtree_physical,
                }
            })
            .unwrap_or(Bytes(0))
    }

    /// `(on disk, apparent)` totals of the whole scan — the header shows
    /// both because the gap is the cloud-placeholders/clones story.
    pub fn root_totals(&self) -> (Bytes, Bytes) {
        self.arena
            .as_ref()
            .map(|a| {
                let r = a.node(a.root());
                (r.subtree_physical, r.subtree_size)
            })
            .unwrap_or((Bytes(0), Bytes(0)))
    }

    pub fn root_files(&self) -> u64 {
        self.arena
            .as_ref()
            .map(|a| a.node(a.root()).subtree_files)
            .unwrap_or(0)
    }
}

fn hog_key(arena: &FileArena, id: NodeId, mode: SizeMode) -> u64 {
    let n = arena.node(id);
    match mode {
        SizeMode::Logical => n.size.0,
        SizeMode::Physical => n.physical.0,
    }
}

/// Dirs first, then the active key. Descending by default everywhere.
fn cmp_children(
    arena: &FileArena,
    a: NodeId,
    b: NodeId,
    sort: SortBy,
    mode: SizeMode,
) -> std::cmp::Ordering {
    use std::cmp::Ordering as O;
    let (na, nb) = (arena.node(a), arena.node(b));
    let (da, db) = (na.kind == NodeKind::Dir, nb.kind == NodeKind::Dir);
    if da != db {
        return if da { O::Less } else { O::Greater };
    }
    let ord = match sort.key {
        SortKey::Size => subtree_value(na, mode).cmp(&subtree_value(nb, mode)),
        SortKey::Name => arena.name(a).as_bytes().cmp(arena.name(b).as_bytes()),
        SortKey::Modified => na.mtime.cmp(&nb.mtime),
        SortKey::Items => na.subtree_files.cmp(&nb.subtree_files),
    };
    if sort.asc {
        ord
    } else {
        ord.reverse()
    }
}

fn subtree_value(n: &metla::arena::Node, mode: SizeMode) -> Bytes {
    match mode {
        SizeMode::Logical => n.subtree_size,
        SizeMode::Physical => n.subtree_physical,
    }
}

pub fn visible_rows() -> usize {
    // Fallback when the pane height is unknown; views recompute precisely.
    20
}

impl App {
    pub fn hint_line(&self) -> &'static str {
        match self.screen {
            Screen::Browse => "↑↓ move · enter open · u up · space mark · d delete · a archive · / filter · s sort · p size · 1/2/3 views · ? help · q quit",
            Screen::Hogs => "↑↓ move · enter jump · d delete · a archive · p logical/physical · 1 browse · 3 suggest · ? help · q quit",
            Screen::Suggestions => "↑↓ move · space check · enter jump · d/a act on checked · D/A act on all · 1 browse · 2 hogs · ? help · q quit",
            Screen::Help => "? back · q quit",
        }
    }

    pub fn stats_line(&self) -> String {
        let mut s = format!(
            "{} files · {}",
            HumanCount(self.root_files()),
            HumanBytes(self.root_total().0)
        );
        match self.size_mode {
            SizeMode::Logical => s.push_str(" logical"),
            SizeMode::Physical => s.push_str(" physical"),
        }
        if self.freed_total.0 > 0 {
            s.push_str(&format!(" · freed {}", HumanBytes(self.freed_total.0)));
        }
        s.push_str(&format!(
            " · {} · {}",
            self.pal.name(),
            env!("CARGO_PKG_VERSION")
        ));
        s
    }
}

// ------------------------------------------------------------------ operation log

/// `~/.metla/metla.log` — every destructive operation is appended here by
/// the worker, so "what exactly happened" survives any UI bugs.
fn operation_log_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".metla/metla.log"))
}

fn append_operation_log(
    path: &Option<PathBuf>,
    verb: &str,
    items: &[String],
    report: &action::ActionReport,
) {
    let Some(path) = path else { return };
    let dir = path.parent().map(PathBuf::from);
    if let Some(dir) = dir {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    use std::io::Write as _;
    let ts = metla::types::UnixTime(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    );
    let (y, mo, d) = ts.civil();
    let (hh, mm, ss) = ts.hms();
    let status = if report.cancelled {
        "cancelled"
    } else if report.errors.is_empty() {
        "ok"
    } else {
        "partial"
    };
    let mut summary = format!(
        "{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z\t{verb}\t{status}\titems={}\tfreed={}\n",
        items.len(),
        HumanBytes(report.freed_logical.0)
    );
    if let Some((dest, _)) = &report.archived {
        summary.push_str(&format!(
            "{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z\t  archive→\t{}\n",
            dest.display()
        ));
    }
    for p in &report.done {
        summary.push_str(&format!(
            "{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z\t  done\t{}\n",
            p.display()
        ));
    }
    for e in &report.errors {
        summary.push_str(&format!(
            "{y:04}-{mo:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z\t  fail\t{e}\n"
        ));
    }
    let _ = f.write_all(summary.as_bytes());
}
