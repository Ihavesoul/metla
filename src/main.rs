//! metla — a zero-nonsense disk sweeper.
//!
//! metla [ROOT] [--light | --dark] [--archive-dir DIR] [--no-hardlinks]
//!       [--scan-only] [--help]

mod ui;

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use metla::arena::{FileArena, NameRef};
use metla::format::{shorten_home, HumanBytes, HumanCount};
use metla::suggest::{self, RecommendedAction};
use metla::types::{NodeId, NodeKind, UnixTime};
use metla::walk::{ScanOptions, ScanProgress};
use ui::{App, AppOptions};

const USAGE: &str = "\
metla — a zero-nonsense disk sweeper

USAGE:
    metla [ROOT] [FLAGS]

ARGS:
    ROOT                  directory to index (default: $HOME)

FLAGS:
    --light               start with the light theme
    --dark                start with the dark theme (default)
    --archive-dir DIR     where archives go (default: ~/metla-archive)
    --no-hardlinks        do not deduplicate hardlinks while indexing
    --scan-only           index, print hogs and suggestions to stdout, exit
    --help                this text

keys: ? in the TUI shows the full keymap. Deletion is permanent by design —
use `a` to archive (tar.zst) instead. Restore with:
    tar --zstd -xf <archive.tar.zst> -C <original-parent>
";

struct Args {
    root: PathBuf,
    light: bool,
    dark: bool,
    archive_dir: Option<PathBuf>,
    scan_only: bool,
    no_hardlinks: bool,
    help: bool,
}

fn parse_args() -> Result<Args, String> {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let mut a = Args {
        root: home,
        light: false,
        dark: false,
        archive_dir: None,
        scan_only: false,
        no_hardlinks: false,
        help: false,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--light" => a.light = true,
            "--dark" => a.dark = true,
            "--scan-only" => a.scan_only = true,
            "--no-hardlinks" => a.no_hardlinks = true,
            "--help" | "-h" => a.help = true,
            "--archive-dir" => {
                let v = it.next().ok_or("--archive-dir needs a value")?;
                a.archive_dir = Some(PathBuf::from(v));
            }
            other if other.starts_with("--") => return Err(format!("unknown flag: {other}")),
            other => a.root = PathBuf::from(other),
        }
    }
    if a.light && a.dark {
        return Err("--light and --dark are mutually exclusive".into());
    }
    Ok(a)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("metla: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if args.help {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.scan_only {
        return scan_only(&args);
    }
    match tui_main(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("metla: {e}");
            ExitCode::from(1)
        }
    }
}

// ----------------------------------------------------------------- scan-only

fn scan_only(args: &Args) -> ExitCode {
    let home = home_dir();
    let archive_dir = args
        .archive_dir
        .clone()
        .unwrap_or_else(|| home.join("metla-archive"));
    let progress = ScanProgress::new();
    let opts = ScanOptions {
        root: args.root.clone(),
        dedup_hardlinks: !args.no_hardlinks,
        skip_paths: metla::walk::default_skip_paths(&args.root, &archive_dir),
    };
    let (arena, stats) = match metla::walk::run_scan(&opts, &progress) {
        Ok(res) => res,
        Err(e) => {
            eprintln!("metla: {e}");
            return ExitCode::from(1);
        }
    };
    let now = UnixTime(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    );
    let mut sug = Vec::new();
    suggest::run(&arena, &suggest::Rules::default(), now, &mut sug);

    let root = arena.node(arena.root());
    let mut summary = format!(
        "indexed {} files · {} dirs · {} on disk",
        HumanCount(stats.files),
        HumanCount(stats.dirs),
        HumanBytes(root.subtree_physical.0),
    );
    if root.subtree_size != root.subtree_physical {
        let apparent = HumanBytes(root.subtree_size.0);
        summary.push_str(&format!(" ({apparent} apparent)"));
    }
    summary.push_str(&format!(
        " in {:.1}s ({} errors)",
        stats.elapsed.as_secs_f64(),
        stats.errors
    ));
    println!("{summary}");

    let mut rows: Vec<(u64, NodeId)> = Vec::new();
    for idx in 0..arena.len() {
        let id = NodeId(idx as u32);
        let n = arena.node(id);
        if n.kind == NodeKind::File && n.physical.0 > 0 {
            rows.push((n.physical.0, id));
        }
    }
    rows.sort_unstable_by_key(|r| std::cmp::Reverse(r.0));
    rows.truncate(30);

    let mut names: Vec<NameRef> = Vec::with_capacity(64);
    let mut path = PathBuf::with_capacity(512);
    let mut shown = String::new();
    let mut display = |arena: &FileArena, id: NodeId| -> String {
        arena.build_path(id, &opts.root, &mut names, &mut path);
        shown.clear();
        shorten_home(&path, &home, &mut shown);
        shown.clone()
    };

    println!("\ntop space hogs:");
    for (sz, id) in &rows {
        println!("  {:>9}  {}", HumanBytes(*sz), display(&arena, *id));
    }

    println!("\nsuggestions ({}):", sug.len());
    for s in sug.iter().take(20) {
        println!(
            "  {:>9}  {:<15} {:>7} files  {}  → {}",
            HumanBytes(s.bytes.0),
            s.kind.tag(),
            HumanCount(s.files),
            display(&arena, s.target),
            match s.action {
                RecommendedAction::Delete => "delete",
                RecommendedAction::Archive => "archive",
            }
        );
    }
    ExitCode::SUCCESS
}

// ----------------------------------------------------------------- TUI

fn tui_main(args: &Args) -> io::Result<()> {
    let home = home_dir();
    let archive_dir = args
        .archive_dir
        .clone()
        .unwrap_or_else(|| home.join("metla-archive"));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // If anything panics, give the terminal back before the backtrace.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    let app = App::new(AppOptions {
        root: args.root.clone(),
        archive_dir,
        light: args.light && !args.dark,
        dedup_hardlinks: !args.no_hardlinks,
    });
    let res = app.run(&mut terminal);

    // restore terminal
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    res
}
