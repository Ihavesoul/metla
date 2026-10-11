<div align="center">

# 🧹 metla

**A zero-nonsense disk sweeper for macOS.**

Find what is actually eating your disk. Delete it or archive it. In one keypress.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platform](https://img.shields.io/badge/platform-macOS-lightgrey)
![Rust](https://img.shields.io/badge/rust-1.75%2B-orange)

</div>

---

``` text
 metla ▸ ~ · 639G disk · 12.6T apparent    [1 browse] [2 hogs] [3 suggest] [t dark]
┌ ~/Work/myapp · 484M · 3.7K files ────────────────────────────────────────────┐
│  NAME                                     SIZE         FILES    MODIFIED    │
│▸ target · rust target                     484M  ██████████  3.7K   2026-10  │
│  src                                      144K  ░░░░░░░░░░    14   2026-10  │
│  Cargo.lock                                20K  ░░░░░░░░░░         2026-10  │
└──────────────────────────────────────────────────────────────────────────────┘
 indexed 4.4M files · 546K dirs in 7:00 · freed 3.1G today
```

That header is real. A one-terabyte Mac reporting **12.6T apparent** — iCloud
placeholders and sparse images happily lie about their size. metla counts what is
*on the disk* and shows the phantom number next to it, so you always know which one
you are looking at.

## Why

Every disk sweeper for macOS eventually fails you in one of three ways:

- **It shows phantom terabytes.** Apparent sizes count iCloud placeholders and APFS
  clones as if the bytes were on your disk. They are not.
- **It can't handle the small-file swamp.** A directory with 5,000 tiny stale files
  is a full afternoon of clicking — and you still can't archive it.
- **It only deletes.** Sometimes you want the space *and* the data — archived, not
  vaporized.

metla fixes all three: parallel indexing that finishes before your coffee, honest
physical sizes by default, bulk suggestions for junk and small-file crowds, and
two actions on everything — permanent delete or `tar.zst` archive.

## Features

- **Fast indexing** — one rayon task per directory: ~2.1M files / 145 GB in ~18 s.
  Syscall-bound, not allocation-bound.
- **Real sizes by default** — physical (`st_blocks`) everywhere; logical size one
  keypress away, both totals always in the header.
- **Hardlink dedup** — `(dev, ino)` table, sizes never double-counted.
- **Suggestions** — known junk dirs, crowds of small stale files, old heavyweights,
  stale archives, oversized logs; ranked by reclaimable bytes, one keypress to act.
- **Archive, don't just delete** — streaming `tar.zst` (flat memory, any size),
  originals removed only after the archive is complete.
- **Two hand-tuned themes** — dark and light, hot-swappable with `t`.
- **Safety net** — protected paths, confirm dialogs with byte estimates, operation
  log, mid-operation cancel.
- **Zero UI jank** — every list renders only the visible window; sort/filter/suggest
  reuse scratch buffers, no per-frame allocation storms.

## Installation

Build from source (Rust 1.75+):

``` bash
git clone https://github.com/Ihavesoul/metla
cd metla && make release
./target/release/metla
```

or straight from the repo:

``` bash
cargo install --locked --git https://github.com/Ihavesoul/metla
```

> **Full Disk Access.** To index `~/Library` (caches, Xcode leftovers, containers)
> give your terminal app *Full Disk Access* in System Settings → Privacy & Security.
> Without it, macOS silently hides those directories from every tool — metla
> included — and reports them as errors.

## Quick start

``` bash
metla                      # index $HOME and open the browser
metla ~/Work               # index a specific tree
metla --light              # light theme
metla --scan-only ~        # no TUI: totals, top hogs and suggestions to stdout
```

The loop is: wait for the scan → press `3` → read the suggestions → `space` what
you agree with → `a` to archive or `d` to delete → `y` to confirm. Done.

## The three views

| View | Key | What it shows |
|---|---|---|
| **Browse** | `1` | The tree, one directory at a time: sizes as proportional bars, file counts, dates. Sort by size/name/date/count, filter by substring. |
| **Hogs** | `2` | The 300 biggest files under the scan root with full paths and age. `enter` jumps to the file's parent in Browse. |
| **Suggestions** | `3` | What metla thinks you should reclaim, ranked by bytes, each with a `DEL`/`ARC` recommendation. |

### What counts as a suggestion

| Rule | Trigger | Recommendation |
|---|---|---|
| Known junk | `node_modules`, Rust `target` (when a `Cargo.toml` sits next to it), Xcode `DerivedData`, `Caches`, `.gradle`, `.npm`, `__pycache__`, `~/.Trash`, … | delete (regenerable) or archive |
| Tiny crowd | ≥ 200 files ≤ 64 KB each, ≤ 512 MB apparent, **≥ 1 MB actually on disk**, untouched ≥ 90 days | archive |
| Old heavyweight | single file ≥ 256 MB **on disk**, untouched ≥ 365 days | archive |
| Stale archive | `zip / 7z / dmg / tar.gz / …` ≥ 10 MB on disk, untouched ≥ 180 days | delete |
| Big log | `*.log` ≥ 128 MB on disk | delete |

Thresholds live in one place — `impl Default for Rules` in `src/suggest.rs` — if you
want the sweeper less or more aggressive.

## Keybindings

| Key | Action |
|---|---|
| `1` / `2` / `3` | browse / hogs / suggestions |
| `↑↓` `jk`, `pgup` `pgdn`, `g` `G` | move |
| `enter` `l` / `u` `h` | enter directory / go to parent |
| `space`, `X` | mark item / clear all marks |
| `d` | **delete — permanently, no Trash** (confirm dialog first) |
| `a` | **archive** into `~/metla-archive/*.tar.zst`, then free the originals |
| `/` | filter the current listing by substring |
| `s` | cycle sort: size → name → date → file count |
| `p` | physical (default) ↔ logical sizes |
| `t` / `?` | dark/light theme / help |
| `q` | quit — during an operation, requests a cancel |

In Suggestions: `space` checks rows, `d`/`a` act on the checked rows, `D`/`A` on
all of them.

## Real sizes, not phantom terabytes

`du`-style physical size (`st_blocks`) is the default everywhere, and the header
always shows both totals when they disagree:

``` text
 metla ▸ ~ · 639G disk · 12.6T apparent
```

Three macOS phenomena make apparent size a work of fiction, and metla handles each:

- **iCloud / File Provider placeholders** — evicted files report their full logical
  size while occupying ~0 bytes locally. They don't inflate any number here, and
  the suggestion engine refuses to "reclaim" them (archiving one would force a
  full download — metla doesn't do that to you).
- **Sparse images** — colima/Docker/AVD disks report terabytes of logical size over
  a fraction of real blocks. Physical is what you can actually reclaim.
- **APFS clones** — Finder duplicates share blocks but each copy reports full size;
  hardlinks are deduplicated through a `(dev, ino)` table.

Two index-time skips keep the truth honest and the scan fast:

- Scanning `/` skips `/System/Volumes` — the firmlinked data volume, otherwise every
  byte gets counted twice (the same content is reachable through `/Users` and friends).
- `$HOME`'s cloud trees (`~/Library/Mobile Documents`, `~/Library/CloudStorage`) are
  skipped by default: nothing inside can be reclaimed locally, and every `opendir`
  there round-trips through the File Provider daemon, which can stall a scan for
  minutes. Want to see what's in the cloud? Point metla at it explicitly —
  `metla ~/Library/Mobile\ Documents` — an explicitly requested path is never skipped.

## Delete or archive

`d` deletes for real — no Trash, by design; this is a broom, not a museum. `a`
streams everything into a single `tar.zst` under `~/metla-archive/` and removes the
originals only after the archive closes cleanly. Any failure leaves originals
untouched and removes the partial archive.

``` bash
# bring it all back:
tar --zstd -xf ~/metla-archive/metla-20261010-094212.tar.zst -C <original-parent>
```

Every operation goes through:

- **Confirm dialog** — item count, byte estimate, first six paths, and any targets
  the safety net refused.
- **The safety net** — never touches `$HOME`, the scan root, the archive directory,
  anything containing the archive directory, or `/System /bin /sbin /usr /dev
  /private/etc /private/var/db /private/var/vm`. Nested targets are deduplicated
  (delete the parent, drop the child).
- **Live progress** — the working dialog shows the exact path being processed right
  now, even when the plan is a single 163 GB file.
- **Cancellation** — `q` mid-operation stops the worker *between* items; the current
  item finishes, nothing new starts, a red toast tells you the plan didn't fully run.
- **An operation log** — `~/.metla/metla.log` records verb, status
  (`ok` / `partial` / `cancelled`), bytes freed, archive path and per-item results.
  "What did I delete?" has a permanent answer.

## How it works

The style borrows from [qeli](https://github.com/litvinovtd/qeli): a typed,
display-agnostic core library, and a thin TUI on top. The core never panics on
runtime data and never allocates per-file on hot paths; the UI never touches the
filesystem.

```
src/
  types.rs     Copy-only scalars: NodeId(u32), Bytes, UnixTime
  arena.rs     the tree = one Vec<Node> with u32 links + a byte pool for names;
               iterative bottom-up aggregation; delete = tombstone flag with
               O(depth+subtree) ancestor decrements — no rebuild, no rescan
  walk.rs      parallel BFS: rayon task per directory, one lock per batch;
               junk classification happens during the walk (it already has
               sibling context: target + Cargo.toml ⇒ rust target)
  suggest.rs   one O(N) pass over the arena into a caller-owned scratch vec
  archive.rs   tar → zstd streaming; flat memory regardless of input size
  action.rs    plan (dedup + safety net) → execute on a worker thread over
               owned data, progress via atomics, cancel between items
  ui/          ratatui: screens, modals, jobs; visible-window rendering;
               scratch buffers reused across frames
```

A node costs ~72 bytes — a million files is ~70 MB plus the name pool, and the
render path reuses its buffers, so the UI stays flat-fingered while the worker
threads do the heavy lifting.

## CLI reference

``` text
metla [ROOT] [FLAGS]

ROOT                    directory to index (default: $HOME)
--light / --dark        theme (default: dark)
--archive-dir DIR       where archives go (default: ~/metla-archive)
--no-hardlinks          skip hardlink deduplication while indexing
--scan-only             index, print hogs + suggestions to stdout, exit
```

## FAQ

**Why is deletion permanent?**
Because a sweeper that hides everything in Trash hasn't actually freed anything,
and macOS Trash makes bulk deletion uselessly slow. `a` is the safe path — same
freed bytes, everything still recoverable.

**Where are my iCloud files in the scan?**
Skipped on purpose (see [above](#real-sizes-not-phantom-terabytes)). Scan the
cloud folder directly if you want it indexed.

**The scan seems slow inside `~/Library`.**
Either Full Disk Access is missing (macOS answers slowly or hides things) or a
File Provider tree is being traversed — those `opendir` calls talk to a daemon.
Both are covered above; neither is metla grinding.

**I deleted a colima/Docker disk and nothing was freed.**
The VM was running and still holds the file descriptor. Stop the VM first
(`colima stop`) — then delete, or better, let the tool remove its own disks.

**Anything big I should know about before deleting?**
The confirm dialog, the safety net and the log exist exactly for that. Read the
dialog; trust the net; check `~/.metla/metla.log` afterwards.

## Roadmap

- index cache for instant re-open, rescan only what changed
- per-extension view (video / archives / build artifacts: how much, where)
- duplicate finder (size + hash)
- config file for thresholds and the protect list
- mouse support

## Contributing

PRs welcome. The gate is `make check` — rustfmt, clippy `-D warnings`, and the
test suite. If you touch the walker or the arena, add a test that fails without
you; both modules exist to make wrong numbers visible.

## License

[MIT](LICENSE) — the shortest license that works.
