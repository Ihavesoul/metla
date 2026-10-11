//! All painting: header, status bar, scanning, browse, hogs, suggestions,
//! help and modals. Views own no state — they refresh what they need through
//! `App` and render only the visible window of any list.

use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Row, Table};
use ratatui::Frame;

use metla::action::{self, ActionPlan};
use metla::format::{shorten_home, Age, Date, HumanBytes, HumanCount};
use metla::junk::JunkCategory;
use metla::suggest::RecommendedAction;
use metla::types::{Bytes, NodeId, NodeKind, UnixTime};

use super::app::{App, Modal, Screen, SizeMode, ToastKind};
use super::theme::Palette;

pub fn header(f: &mut Frame, app: &App, area: Rect) {
    let p = app.pal;
    let mut root_short = String::new();
    shorten_home(&app.root_prefix, &app.home, &mut root_short);

    let (disk, apparent) = app.root_totals();
    let mut total = format!(" · {} disk", HumanBytes(disk.0));
    if apparent != disk {
        total.push_str(&format!(" · {} apparent", HumanBytes(apparent.0)));
    }
    let mut spans = vec![
        Span::styled(
            " metla",
            Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ▸ ", Style::new().fg(p.dim)),
        Span::styled(root_short, Style::new().fg(p.fg)),
        Span::styled(total, Style::new().fg(p.accent2)),
        Span::styled("    ", Style::new()),
    ];
    let tabs = [
        (Screen::Browse, "1 browse"),
        (Screen::Hogs, "2 hogs"),
        (Screen::Suggestions, "3 suggest"),
    ];
    for (screen, label) in tabs {
        let st = if app.screen == screen {
            Style::new()
                .fg(p.accent)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::new().fg(p.dim)
        };
        spans.push(Span::styled(format!("[{label}] "), st));
    }
    spans.push(Span::styled(
        format!("[t {}]", if p == Palette::DARK { "dark" } else { "light" }),
        Style::new().fg(p.dim),
    ));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn status(f: &mut Frame, app: &App, area: Rect) {
    let p = app.pal;
    let (left, color) = match &app.toast {
        Some((msg, kind, _)) => {
            let c = match kind {
                ToastKind::Good => p.ok,
                ToastKind::Bad => p.danger,
                ToastKind::Info => p.accent,
            };
            (msg.clone(), c)
        }
        None => (app.hint_line().to_string(), p.dim),
    };
    let line = Line::from(vec![
        Span::styled(format!(" {left}"), Style::new().fg(color)),
        Span::styled(format!("  {}", app.stats_line()), Style::new().fg(p.dim)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

pub fn scanning(f: &mut Frame, app: &App, area: Rect) {
    let p = app.pal;
    let Some(job) = app.scan_job.as_ref() else {
        return;
    };
    let secs = job.started.elapsed().as_secs_f64().max(0.001);
    let files = job
        .progress
        .files
        .load(std::sync::atomic::Ordering::Relaxed);
    let dirs = job.progress.dirs.load(std::sync::atomic::Ordering::Relaxed);
    let errors = job
        .progress
        .errors
        .load(std::sync::atomic::Ordering::Relaxed);
    let bytes = job
        .progress
        .bytes
        .load(std::sync::atomic::Ordering::Relaxed);

    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let frame = FRAMES[(secs * 10.0) as usize % FRAMES.len()];

    let mut root_short = String::new();
    shorten_home(&app.root_prefix, &app.home, &mut root_short);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!(" {frame} "),
                Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("indexing {root_short}"),
                Style::new().fg(p.fg).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("   elapsed  ", Style::new().fg(p.dim)),
            Span::styled(format!("{secs:.1}s"), Style::new().fg(p.fg)),
            Span::styled("      files  ", Style::new().fg(p.dim)),
            Span::styled(format!("{}", HumanCount(files)), Style::new().fg(p.fg)),
            Span::styled("      dirs  ", Style::new().fg(p.dim)),
            Span::styled(format!("{}", HumanCount(dirs)), Style::new().fg(p.fg)),
        ]),
        Line::from(vec![
            Span::styled("   apparent  ", Style::new().fg(p.dim)),
            Span::styled(format!("{}", HumanBytes(bytes)), Style::new().fg(p.fg)),
            Span::styled("      rate  ", Style::new().fg(p.dim)),
            Span::styled(
                format!("{}/s", HumanCount((files as f64 / secs) as u64)),
                Style::new().fg(p.fg),
            ),
        ]),
    ];
    if errors > 0 {
        lines.push(Line::from(vec![
            Span::styled("   errors  ", Style::new().fg(p.dim)),
            Span::styled(format!("{errors}"), Style::new().fg(p.warn)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "   q — quit · t — theme",
        Style::new().fg(p.dim),
    )));
    let block = Block::bordered()
        .title(" metla ")
        .border_style(Style::new().fg(p.border));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn scroll_top(sel: usize, rows: usize, visible: usize) -> usize {
    let visible = visible.max(1);
    let top = sel.saturating_sub(visible.saturating_sub(1) / 2);
    top.min(rows.saturating_sub(visible))
}

fn bar(p: Palette, share: f64, width: usize, empty_is_dim: bool) -> Span<'static> {
    if share <= 0.0 {
        return Span::styled(
            "░".repeat(width),
            Style::new().fg(if empty_is_dim { p.border } else { p.bar_lo }),
        );
    }
    let filled = ((share * width as f64).round() as usize).clamp(0, width);
    let color = if share > 0.5 {
        p.bar_hi
    } else if share > 0.2 {
        p.bar_mid
    } else {
        p.bar_lo
    };
    let mut s = String::with_capacity(width);
    for _ in 0..filled {
        s.push('█');
    }
    for _ in filled..width {
        s.push('░');
    }
    Span::styled(s, Style::new().fg(color))
}

pub fn browse(f: &mut Frame, app: &mut App, area: Rect) {
    app.refresh_browse();
    let p = app.pal;
    let crumb = app.path_string(app.browser.cwd);
    let Some(arena) = app.arena.as_ref() else {
        return;
    };
    let b = &app.browser;
    let cwd = arena.node(b.cwd);
    let total = subtree_of(cwd, app.size_mode);

    let block = Block::bordered()
        .border_style(Style::new().fg(p.border))
        .title(Line::from(vec![
            Span::styled(" ", Style::new()),
            Span::styled(
                crumb.clone(),
                Style::new().fg(p.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " · {} · {} files ",
                    HumanBytes(total.0),
                    HumanCount(cwd.subtree_files)
                ),
                Style::new().fg(p.dim),
            ),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let vis = inner.height.saturating_sub(1) as usize; // header row
    let rows_len = b.ids.len();
    let top = scroll_top(b.sel_pos, rows_len, vis);
    let end = (top + vis).min(rows_len);

    let mut rows: Vec<Row> = Vec::with_capacity(end - top);
    for (i, &id) in b.ids[top..end].iter().enumerate() {
        let n = arena.node(id);
        let selected = top + i == b.sel_pos;
        let marked = b.marked.contains(&id);
        let marker = if marked {
            Span::styled("■", Style::new().fg(p.mark))
        } else if selected {
            Span::styled("▸", Style::new().fg(p.accent))
        } else {
            Span::styled(" ", Style::new())
        };
        let size_v = subtree_of(n, app.size_mode).0;
        let share = if b.max_child.0 > 0 {
            size_v as f64 / b.max_child.0 as f64
        } else {
            0.0
        };
        let items = if n.kind == NodeKind::Dir {
            Span::styled(
                format!("{:>7}", HumanCount(n.subtree_files)),
                Style::new().fg(p.dim),
            )
        } else {
            Span::styled("       ", Style::new())
        };
        let name_spans = name_spans(p, arena, id);
        let mut cells: Vec<Line> = Vec::with_capacity(6);
        cells.push(Line::from(marker));
        cells.push(Line::from(name_spans));
        cells.push(Line::from(Span::styled(
            format!("{:>9}", HumanBytes(size_v)),
            Style::new()
                .fg(if selected { p.fg } else { p.dim })
                .add_modifier(if selected {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        )));
        cells.push(Line::from(bar(p, share, 10, true)));
        cells.push(Line::from(items));
        cells.push(Line::from(Span::styled(
            format!("{:>10}", Date(n.mtime)),
            Style::new().fg(p.dim),
        )));
        let mut row = Row::new(cells);
        if selected {
            row = row.style(Style::new().bg(p.sel_bg));
        }
        rows.push(row);
    }

    let header = Row::new([
        Line::from(""),
        Line::from(Span::styled(
            "NAME",
            Style::new().fg(p.dim).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            if app.size_mode == SizeMode::Physical {
                "PHYS"
            } else {
                "SIZE"
            },
            Style::new().fg(p.dim).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "FILES",
            Style::new().fg(p.dim).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "MODIFIED",
            Style::new().fg(p.dim).add_modifier(Modifier::BOLD),
        )),
    ]);
    let widths = [
        Constraint::Length(1),
        Constraint::Min(24),
        Constraint::Length(9),
        Constraint::Length(11),
        Constraint::Length(8),
        Constraint::Length(11),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .style(Style::new().bg(p.bg));
    f.render_widget(table, inner);

    if rows_len == 0 {
        let msg = if app.filter.is_some() {
            Line::from(Span::styled(
                " nothing matches the filter",
                Style::new().fg(p.dim),
            ))
        } else {
            Line::from(Span::styled(" empty directory", Style::new().fg(p.dim)))
        };
        f.render_widget(Paragraph::new(msg), inner);
    }
}

pub fn hogs(f: &mut Frame, app: &mut App, area: Rect) {
    let p = app.pal;
    let Some(arena) = app.arena.as_ref() else {
        return;
    };
    let rows_len = app.hogs.rows.len();
    let block = Block::bordered()
        .border_style(Style::new().fg(p.border))
        .title(Line::from(vec![
            Span::styled(
                " biggest files",
                Style::new().fg(p.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {} shown · enter jumps to them ", rows_len),
                Style::new().fg(p.dim),
            ),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if rows_len == 0 {
        f.render_widget(
            Paragraph::new(Span::styled(" nothing big enough", Style::new().fg(p.dim))),
            inner,
        );
        return;
    }

    let vis = inner.height.saturating_sub(1) as usize;
    let top = scroll_top(app.hogs.sel, rows_len, vis);
    let end = (top + vis).min(rows_len);
    let top_size = hog_size(app, arena, app.hogs.rows[0]);

    let mut rows: Vec<Row> = Vec::with_capacity(end - top);
    for (i, &id) in app.hogs.rows[top..end].iter().enumerate() {
        let n = arena.node(id);
        let selected = top + i == app.hogs.sel;
        let sz = hog_size(app, arena, id);
        let share = if top_size.0 > 0 {
            sz.0 as f64 / top_size.0 as f64
        } else {
            0.0
        };
        let path = node_path_lossy(arena, app, id);
        let mut cells: Vec<Line> = Vec::with_capacity(5);
        cells.push(Line::from(Span::styled(
            format!("{:>3}", top + i + 1),
            Style::new().fg(p.dim),
        )));
        cells.push(Line::from(Span::styled(
            format!("{:>9}", HumanBytes(sz.0)),
            Style::new().fg(if selected { p.fg } else { p.warn }),
        )));
        cells.push(Line::from(bar(p, share, 10, true)));
        cells.push(Line::from(Span::styled(path, Style::new().fg(p.fg))));
        cells.push(Line::from(Span::styled(
            format!(
                "{:>6} {:>10}",
                Age {
                    days: n.mtime.days_before(now_utc())
                },
                Date(n.mtime)
            ),
            Style::new().fg(p.dim),
        )));
        let mut row = Row::new(cells);
        if selected {
            row = row.style(Style::new().bg(p.sel_bg));
        }
        rows.push(row);
    }
    let header = Row::new([
        Line::from(Span::styled("#", Style::new().fg(p.dim))),
        Line::from(Span::styled(
            if app.size_mode == SizeMode::Physical {
                "PHYS"
            } else {
                "SIZE"
            },
            Style::new().fg(p.dim).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "PATH",
            Style::new().fg(p.dim).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "AGE",
            Style::new().fg(p.dim).add_modifier(Modifier::BOLD),
        )),
    ]);
    let widths = [
        Constraint::Length(4),
        Constraint::Length(9),
        Constraint::Length(11),
        Constraint::Min(30),
        Constraint::Length(17),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .style(Style::new().bg(p.bg));
    f.render_widget(table, inner);
}

pub fn suggestions(f: &mut Frame, app: &mut App, area: Rect) {
    let p = app.pal;
    let Some(arena) = app.arena.as_ref() else {
        return;
    };
    let rows_len = app.sug.rows.len();
    let total: u64 = app.sug.rows.iter().map(|s| s.bytes.0).sum();
    let block = Block::bordered()
        .border_style(Style::new().fg(p.border))
        .title(Line::from(vec![
            Span::styled(" suggestions", Style::new().fg(p.fg).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(
                    " · {rows_len} found · up to {} reclaimable · space checks · d/a acts · D/A acts on all ",
                    HumanBytes(total)
                ),
                Style::new().fg(p.dim),
            ),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if rows_len == 0 {
        f.render_widget(
            Paragraph::new(Span::styled(
                " nothing to suggest — this place is already clean",
                Style::new().fg(p.ok),
            )),
            inner,
        );
        return;
    }

    let vis = inner.height.saturating_sub(1) as usize;
    let top = scroll_top(app.sug.sel, rows_len, vis);
    let end = (top + vis).min(rows_len);

    let mut rows: Vec<Row> = Vec::with_capacity(end - top);
    for (i, s) in app.sug.rows[top..end].iter().enumerate() {
        let selected = top + i == app.sug.sel;
        let checked = app.sug.checked.contains(&(top + i));
        let n = arena.node(s.target);
        let path = node_path_lossy(arena, app, s.target);
        let (tag_color, badge, badge_color) = match s.action {
            RecommendedAction::Delete => (p.warn, "DEL", p.danger),
            RecommendedAction::Archive => (p.accent, "ARC", p.accent),
        };
        let age_days = n.mtime.days_before(now_utc());
        let check = if checked {
            Span::styled("✓", Style::new().fg(p.mark))
        } else if selected {
            Span::styled("▸", Style::new().fg(p.accent))
        } else {
            Span::styled(" ", Style::new())
        };
        let mut cells: Vec<Line> = Vec::with_capacity(6);
        cells.push(Line::from(check));
        cells.push(Line::from(Span::styled(
            format!("{:<14}", s.kind.tag()),
            Style::new().fg(tag_color),
        )));
        cells.push(Line::from(Span::styled(
            format!("{:>9}", HumanBytes(s.bytes.0)),
            Style::new().fg(p.fg),
        )));
        cells.push(Line::from(Span::styled(
            format!("{:>7}", HumanCount(s.files)),
            Style::new().fg(p.dim),
        )));
        cells.push(Line::from(Span::styled(
            format!("{badge:<4}"),
            Style::new().fg(badge_color).add_modifier(Modifier::BOLD),
        )));
        cells.push(Line::from(vec![
            Span::styled(path, Style::new().fg(p.fg)),
            Span::styled(
                format!(" — {}, {}", s.kind.reason(), Age { days: age_days }),
                Style::new().fg(p.dim),
            ),
        ]));
        let mut row = Row::new(cells);
        if selected {
            row = row.style(Style::new().bg(p.sel_bg));
        }
        rows.push(row);
    }
    let header = Row::new([
        Line::from(""),
        Line::from(Span::styled(
            "CATEGORY",
            Style::new().fg(p.dim).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "SIZE",
            Style::new().fg(p.dim).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "FILES",
            Style::new().fg(p.dim).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "TARGET",
            Style::new().fg(p.dim).add_modifier(Modifier::BOLD),
        )),
    ]);
    let widths = [
        Constraint::Length(1),
        Constraint::Length(14),
        Constraint::Length(9),
        Constraint::Length(8),
        Constraint::Length(4),
        Constraint::Min(30),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1)
        .style(Style::new().bg(p.bg));
    f.render_widget(table, inner);
}

pub fn help(f: &mut Frame, app: &App, area: Rect) {
    let p = app.pal;
    let keys: &[(&str, &str)] = &[
        ("1 / 2 / 3", "browse / hogs / suggestions"),
        ("?", "toggle this help"),
        ("t", "toggle dark / light theme"),
        ("q, ctrl-c", "quit"),
        ("", ""),
        ("↑ ↓ / j k", "move"),
        ("pgup pgdn", "page"),
        ("g / G, home / end", "jump to start / end"),
        ("enter / l", "enter directory"),
        ("u / h", "parent directory"),
        ("space", "mark item (browse) / check row (suggestions)"),
        ("X", "clear all marks"),
        ("d", "delete — permanently, no Trash"),
        ("a", "archive into ~/metla-archive/*.tar.zst"),
        ("/", "filter current listing by substring"),
        ("s", "cycle sort: size → name → date → count"),
        (
            "p",
            "physical (default, real disk use) ↔ logical (apparent)",
        ),
        ("", ""),
        ("restore", "tar --zstd -xf <archive> -C <where-it-was>"),
    ];
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(""));
    for (k, v) in keys {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<18}", k = k), Style::new().fg(p.accent)),
            Span::styled(*v, Style::new().fg(p.fg)),
        ]));
    }
    let block = Block::bordered()
        .border_style(Style::new().fg(p.border))
        .title(" keys ")
        .title_style(Style::new().fg(p.fg).add_modifier(Modifier::BOLD));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

// ---------------------------------------------------------------------------- modals

pub fn modal(f: &mut Frame, app: &App, area: Rect) {
    match &app.modal {
        Modal::None => {}
        Modal::Confirm(plan) => confirm_modal(f, app, plan, area),
        Modal::Working => working_modal(f, app, area),
        Modal::Error(msg) => error_modal(f, app, msg, area),
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width.saturating_sub(2)).max(10);
    let h = h.min(area.height.saturating_sub(2)).max(3);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn confirm_modal(f: &mut Frame, app: &App, plan: &ActionPlan, area: Rect) {
    let p = app.pal;
    let (title, title_color) = match plan.action {
        action::PlannedAction::Archive => (
            " archive — originals go into the tar, then freed ",
            p.accent,
        ),
        action::PlannedAction::Delete => (" delete — permanently, bypassing the Trash ", p.danger),
    };
    let show_phys = plan.physical != plan.logical;
    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!(
                    "  {} item(s) · {}",
                    plan.items.len(),
                    HumanBytes(plan.logical.0)
                ),
                Style::new().fg(p.fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if show_phys {
                    format!(" (phys {})", HumanBytes(plan.physical.0))
                } else {
                    String::new()
                },
                Style::new().fg(p.dim),
            ),
        ]),
    ];
    if plan.action == action::PlannedAction::Archive {
        lines.push(Line::from(Span::styled(
            format!(
                "  → {}",
                action::propose_archive_path(&app.archive_dir).display()
            ),
            Style::new().fg(p.accent),
        )));
    }
    lines.push(Line::from(""));
    for item in plan.items.iter().take(6) {
        let mut short = String::new();
        shorten_home(&item.path, &app.home, &mut short);
        lines.push(Line::from(vec![
            Span::styled("  · ", Style::new().fg(p.dim)),
            Span::styled(short, Style::new().fg(p.fg)),
            Span::styled(
                format!(" ({})", HumanBytes(item.logical.0)),
                Style::new().fg(p.dim),
            ),
        ]));
    }
    if plan.items.len() > 6 {
        lines.push(Line::from(Span::styled(
            format!("  … and {} more", plan.items.len() - 6),
            Style::new().fg(p.dim),
        )));
    }
    if !plan.warnings.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "  ⚠ {} target(s) skipped: {}",
                plan.warnings.len(),
                plan.warnings[0]
            ),
            Style::new().fg(p.warn),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  y", Style::new().fg(p.ok).add_modifier(Modifier::BOLD)),
        Span::styled(" proceed   ", Style::new().fg(p.dim)),
        Span::styled("n", Style::new().fg(p.danger).add_modifier(Modifier::BOLD)),
        Span::styled(" cancel", Style::new().fg(p.dim)),
    ]));

    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = centered(area, 78, h);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::new().fg(title_color))
        .title(title);
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn working_modal(f: &mut Frame, app: &App, area: Rect) {
    let p = app.pal;
    let Some(job) = app.work_job.as_ref() else {
        return;
    };
    let done = job.progress.done.load(std::sync::atomic::Ordering::Relaxed);
    let total = job
        .progress
        .total
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(1);
    let phase = job.progress.phase_name();
    let current = job.progress.current();
    let cancelled = job.progress.is_cancelled();
    let pct = (done as f64 / total as f64).min(1.0);
    let bar_w = 30;
    let filled = (pct * bar_w as f64).round() as usize;
    let mut bar_s = String::with_capacity(bar_w);
    for _ in 0..filled {
        bar_s.push('█');
    }
    for _ in filled..bar_w {
        bar_s.push('░');
    }
    let elapsed = job.started.elapsed().as_secs();
    let mut clock = String::new();
    metla::format::fmt_clock(elapsed, &mut clock);
    // current path, clipped to the box
    let current_line = if current.is_empty() {
        Line::from(Span::styled("  …", Style::new().fg(p.dim)))
    } else {
        let max = 66;
        let shown: String = if current.chars().count() > max {
            let tail: String = current
                .chars()
                .skip(current.chars().count() - max)
                .collect();
            format!("…{tail}")
        } else {
            current.clone()
        };
        Line::from(Span::styled(format!("  {shown}"), Style::new().fg(p.fg)))
    };
    let status = if cancelled {
        Line::from(Span::styled(
            "  cancel requested — stopping after the current item",
            Style::new().fg(p.warn).add_modifier(Modifier::BOLD),
        ))
    } else {
        Line::from(Span::styled(
            format!("  elapsed {clock} · q — cancel (stops between items)"),
            Style::new().fg(p.dim),
        ))
    };
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {phase}…"),
            Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
        )),
        current_line,
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("  {done}/{total} "), Style::new().fg(p.fg)),
            Span::styled(bar_s, Style::new().fg(p.accent)),
            Span::styled(
                format!(" {}%", (pct * 100.0) as u64),
                Style::new().fg(p.dim),
            ),
        ]),
        Line::from(""),
        status,
    ];
    let rect = centered(area, 78, 9);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::new().fg(p.accent))
        .title(" working ");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn error_modal(f: &mut Frame, app: &App, msg: &str, area: Rect) {
    let p = app.pal;
    let lines: Vec<Line> = std::iter::once(Line::from(""))
        .chain(
            msg.lines()
                .map(|l| Line::from(Span::styled(format!("  {l}"), Style::new().fg(p.fg)))),
        )
        .chain(std::iter::once(Line::from("")))
        .chain(std::iter::once(Line::from(Span::styled(
            "  any key — dismiss",
            Style::new().fg(p.dim),
        ))))
        .collect();
    let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = centered(area, 80, h);
    f.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_style(Style::new().fg(p.danger))
        .title(" error ");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

// ---------------------------------------------------------------------------- helpers

fn now_utc() -> UnixTime {
    UnixTime(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    )
}

fn subtree_of(n: &metla::arena::Node, mode: SizeMode) -> Bytes {
    match mode {
        SizeMode::Logical => n.subtree_size,
        SizeMode::Physical => n.subtree_physical,
    }
}

fn hog_size(app: &App, arena: &metla::arena::FileArena, id: NodeId) -> Bytes {
    let n = arena.node(id);
    match app.size_mode {
        SizeMode::Logical => n.size,
        SizeMode::Physical => n.physical,
    }
}

/// Absolute path of a node as an owned string (shortened by `~`).
/// Used only for the visible window of a list, a few strings per frame.
fn node_path_lossy(arena: &metla::arena::FileArena, app: &App, id: NodeId) -> String {
    thread_local! {
        static NAMES: std::cell::RefCell<Vec<metla::arena::NameRef>> =
            const { std::cell::RefCell::new(Vec::new()) };
        // PathBuf::new is not const-stable yet
        #[allow(clippy::missing_const_for_thread_local)]
        static PATH: std::cell::RefCell<std::path::PathBuf> =
            std::cell::RefCell::new(std::path::PathBuf::new());
    }
    let mut out = String::new();
    NAMES.with_borrow_mut(|names| {
        PATH.with_borrow_mut(|path| {
            arena.build_path(id, &app.root_prefix, names, path);
            shorten_home(path, &app.home, &mut out);
        });
    });
    out
}

fn name_spans(p: Palette, arena: &metla::arena::FileArena, id: NodeId) -> Vec<Span<'static>> {
    let n = arena.node(id);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
    if n.flags & metla::types::flags::HARDLINK != 0 {
        spans.push(Span::styled("=", Style::new().fg(p.dim)));
    }
    match n.kind {
        NodeKind::Dir => spans.push(Span::styled(
            arena.name(id).to_string_lossy().into_owned(),
            Style::new().fg(p.accent).add_modifier(Modifier::BOLD),
        )),
        NodeKind::Symlink => spans.push(Span::styled(
            format!("@{}", arena.name(id).to_string_lossy()),
            Style::new().fg(p.dim),
        )),
        _ => spans.push(Span::styled(
            arena.name(id).to_string_lossy().into_owned(),
            Style::new().fg(p.fg),
        )),
    }
    if let Some(cat) = JunkCategory::from_code(n.junk) {
        spans.push(Span::styled(
            format!(" · {}", cat.as_str()),
            Style::new().fg(p.warn),
        ));
    }
    spans
}
