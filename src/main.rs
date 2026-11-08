mod agent;
mod cache;
mod fsevents;
mod mcp;
mod scan;
mod treemap;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap};
use agent::{Json, Obj, err, obj};
use scan::{Kind, Node, Progress};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_NAME");

const USAGE: &str = concat!("usage: ", env!("CARGO_BIN_NAME"), " [--cross-fs] [--no-cache] [--fast] [--mcp | --mcp-lan] [--mcp-port N]
       [--print [N] [--bytes]] [dir]

Browse disk usage like ncdu, counting APFS clones properly.
  --cross-fs   descend into other mounted filesystems (default: stay on one)
  --no-cache   scan everything; by default the last scan is reused and only
               folders FSEvents reports as changed are rescanned
  --fast       scan with every core at normal priority; by default a scan uses
               80% of the cores at low priority so the rest of the Mac stays usable
  --print [N]  scan, print the N biggest children by private size (default 20), exit
  --bytes      with --print, show exact byte counts
  --mcp        serve MCP (streamable HTTP) at http://127.0.0.1:7377/mcp, so agents
               can browse this scan and suggest deletions; off by default
  --mcp-lan    the same, but listening on every interface, for other machines
               on the network (plain HTTP: only on a network you trust)
  --mcp-port N serve MCP on port N instead of 7377 (implies --mcp)
               Every MCP request needs `Authorization: Bearer <token>`, with the
               token from ~/Library/Application Support/apdu/mcp-token (made on
               first use; delete it to get a new one).

Marked for deletion (x in the TUI marks, D there deletes them):
  ", env!("CARGO_BIN_NAME"), " marked                 list everything marked, in every folder
  ", env!("CARGO_BIN_NAME"), " delete-marked [--yes]  delete everything marked (asks first unless --yes)

Driving a running TUI (for LLMs and agents):
  Each TUI listens on a Unix socket at $TMPDIR/", env!("CARGO_BIN_NAME"), "-<uid>/<pid>.sock.
  `", env!("CARGO_BIN_NAME"), " <command>` sends it one line of JSON and prints the JSON answer
  (errors go to stderr, exit 1). With a <path>, it talks to the TUI browsing the
  deepest folder that holds it (the newest on a tie); suggestions and clear
  with no path go to every TUI. --pid N picks one TUI instead.
  Suggestions are kept between runs; a TUI starts with the ones under its folder.

  goto <path>              open the folder holding <path> and select it
  suggest <path> <reason>  suggest <path> to the user, with a reason
  sizes <path> [--children N] [--sort modified|created]
                           private/allocated bytes and created/modified times of
                           <path> and its N (default 20) biggest, or oldest,
                           children
  find <name> [path] [--limit N]
                           every file or folder under <path> (default: the
                           current folder) named <name> (* matches anything,
                           case ignored), biggest first. It does not look inside a
                           match, and can miss what is inside .git and
                           node_modules (the saved scan keeps only their totals)
  suggestions              list the suggestions, and the suggested paths that
                           were since deleted in ", env!("CARGO_BIN_NAME"), " (don't suggest those again)
  clear [path]             remove the suggestions (or just the one on <path>)
  instances                list the running TUIs (pid and folder)

  In the TUI the user steps through suggestions with ] and [, and pages through
  them with S, marking the ones to delete. None of these commands deletes or
  marks anything.");

const HELP: &str = "\
up/k down/j      move          PgUp/PgDn Home/g End/G
Enter/l/Right    open directory
h/Left/Backspace parent directory
s                sort by size (again: reverse)
n                sort by name (again: reverse)
m / c            sort by modified / created, oldest first
f                sort by files inside (again: reverse)
a                switch size column: private <-> allocated
d                delete selected item now (asks first)
x                mark or unmark it for deletion later (red x;
                 + a folder holding some)    M list marked
D                delete everything marked (asks first)
r                rescan this directory
] [              next / previous agent suggestion (* marks one,
                 + a folder holding some)
S                suggestions page: x marks for deletion, - dismisses
P                pair an agent with the MCP server (--mcp)
t                treemap view: arrows/hjkl move between tiles,
                 Enter or click opens, Backspace or right-click goes up
q                quit           ? close this help

modified   newest change anywhere inside (for a folder)
allocated  bytes the files occupy on disk (what du reports)
private    bytes no APFS clone shares: what deleting frees
           (a clone shared only by files inside one directory
           counts as shared, so a directory can free a bit more)
Hard links and firmlinked folders (/Users is also
/System/Volumes/Data/Users) count once. ! = unreadable,
> = other filesystem, = = folder already counted elsewhere.
Not in the tree: APFS snapshots and purgeable space.";

#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Size,
    Name,
    Modified,
    Created,
    Files,
}

#[derive(Clone, Copy, PartialEq)]
enum Col {
    Private,
    Alloc,
}

#[derive(Clone, Copy, PartialEq)]
enum TileKind {
    Child(usize),
    Free,
    Others(usize),
}

struct Tile {
    rect: Rect,
    kind: TileKind,
    size: u64,
}

const PALETTE: [(u8, u8, u8); 8] =
    [(38, 70, 120), (30, 100, 100), (90, 60, 120), (100, 90, 35), (120, 70, 40), (40, 100, 60), (110, 40, 60), (70, 80, 110)];
const MARKED: (u8, u8, u8) = (170, 35, 35);

struct App {
    root: Node,
    root_path: PathBuf,
    path: Vec<usize>,
    list: ListState,
    sort: Sort,
    desc: bool,
    col: Col,
    one_fs: bool,
    map: bool,
    tiles: Vec<Tile>,
    confirm: bool,
    help: bool,
    message: Option<String>,
    free: (u64, u64),
    snapshots: Arc<Mutex<Option<String>>>,
    mcp: Option<mcp::Running>,
    pairing: Option<String>,
    suggestions: Vec<Suggestion>,
    sugg_file: Option<PathBuf>,
    deleted_file: Option<PathBuf>,
    marked: Vec<PathBuf>,
    marked_file: Option<PathBuf>,
    confirm_marked: bool,
    deleting: Option<Deleting>,
    refreshing: Option<Refreshing>,
    page: Page,
    page_list: ListState,
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Tree,
    Suggestions,
    Marked,
}

/// A delete running on another thread.
struct Deleting {
    targets: Vec<PathBuf>,
    items: u64,
    bytes: u64,
    done: Arc<DeleteProgress>,
    worker: std::thread::JoinHandle<Vec<Result<(), String>>>,
    then: After,
}

#[derive(Default)]
struct DeleteProgress {
    items: std::sync::atomic::AtomicU64,
    bytes: std::sync::atomic::AtomicU64,
    folder: Mutex<PathBuf>,
}

/// A rescan of one folder running on another thread.
struct Refreshing {
    target: PathBuf,
    items: u64,
    started: Instant,
    progress: Arc<Progress>,
    worker: std::thread::JoinHandle<Node>,
}

/// What to show once a delete is done.
enum After {
    One(usize),
    Marked { here: PathBuf, sel: Option<PathBuf>, page: Page, refused: Vec<String> },
}

struct Suggestion {
    path: PathBuf,
    reason: String,
}

fn tm(secs: i64, utc: bool) -> Option<libc::tm> {
    if secs <= 0 {
        return None;
    }
    let mut t: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { if utc { libc::gmtime_r(&secs, &mut t) } else { libc::localtime_r(&secs, &mut t) } };
    (!ok.is_null()).then_some(t)
}

fn date(secs: i64) -> String {
    tm(secs, false).map_or_else(|| "-".into(), |t| format!("{}-{:02}-{:02}", t.tm_year + 1900, t.tm_mon + 1, t.tm_mday))
}

fn iso(secs: i64) -> Json {
    tm(secs, true).map_or(Json::Null, |t| {
        format!("{}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.tm_year + 1900, t.tm_mon + 1, t.tm_mday, t.tm_hour, t.tm_min, t.tm_sec).into()
    })
}

/// Every node under `n` whose name matches `pattern` (lowercase, `*` matches
/// anything; names are compared lowercased), without looking inside a match, so no byte is counted twice.
fn find_named<'a>(n: &'a Node, at: &Path, pattern: &str, out: &mut Vec<(PathBuf, &'a Node)>) {
    for c in &n.children {
        let p = at.join(&*c.name);
        if glob(pattern, &c.name.to_lowercase()) {
            out.push((p, c));
        } else {
            find_named(c, &p, pattern, out);
        }
    }
}

fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((head, rest)) => name.strip_prefix(head).is_some_and(|n| (0..=n.len()).filter(|&i| n.is_char_boundary(i)).any(|i| glob(rest, &n[i..]))),
    }
}

/// Deletes `p` and everything in it, without following symlinks, counting
/// what it has deleted in `progress`.
fn delete_tree(p: &Path, progress: &DeleteProgress) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = p.symlink_metadata()?;
    if m.is_dir() {
        *progress.folder.lock().unwrap() = p.to_path_buf();
        for e in std::fs::read_dir(p)? {
            delete_tree(&e?.path(), progress)?;
        }
        std::fs::remove_dir(p)?;
    } else {
        std::fs::remove_file(p)?;
    }
    progress.items.fetch_add(1, Relaxed);
    progress.bytes.fetch_add(m.blocks() * 512, Relaxed);
    Ok(())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn human(b: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{b} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// A count in at most 6 characters: 9999, 12.3k, 123k, 1.2M.
fn count(n: u64) -> String {
    match n {
        0..10_000 => n.to_string(),
        10_000..100_000 => format!("{:.1}k", n as f64 / 1e3),
        100_000..1_000_000 => format!("{}k", n / 1000),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

fn volume_space(path: &Path) -> (u64, u64) {
    let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return (0, 0) };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return (0, 0);
    }
    let bs = st.f_bsize as u64;
    (st.f_bavail * bs, st.f_blocks * bs)
}

fn count_snapshots() -> Option<String> {
    let out = std::process::Command::new("tmutil").args(["listlocalsnapshots", "/"]).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.lines().filter(|l| l.contains("com.apple")).count().to_string())
}

impl App {
    fn size(&self, n: &Node) -> u64 {
        match self.col {
            Col::Private => n.private,
            Col::Alloc => n.alloc,
        }
    }

    fn cur(&self) -> &Node {
        self.path.iter().fold(&self.root, |n, &i| &n.children[i])
    }

    fn cur_mut(&mut self) -> &mut Node {
        let mut n = &mut self.root;
        for &i in &self.path {
            n = &mut n.children[i];
        }
        n
    }

    fn selected(&self) -> usize {
        self.list.selected().unwrap_or(0)
    }

    fn sort_cur(&mut self) {
        let (sort, desc, col) = (self.sort, self.desc, self.col);
        let sel_name = self.cur().children.get(self.selected()).map(|c| c.name.clone());
        let key = move |n: &Node| match col {
            Col::Private => n.private,
            Col::Alloc => n.alloc,
        };
        let dir = self.cur_mut();
        match sort {
            Sort::Size => dir.children.sort_by(|a, b| key(b).cmp(&key(a)).then_with(|| a.name.cmp(&b.name))),
            Sort::Name => dir.children.sort_by(|a, b| a.name.cmp(&b.name)),
            Sort::Modified => dir.children.sort_by(|a, b| a.modified.cmp(&b.modified).then_with(|| a.name.cmp(&b.name))),
            Sort::Created => dir.children.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name))),
            Sort::Files => dir.children.sort_by(|a, b| b.files.cmp(&a.files).then_with(|| a.name.cmp(&b.name))),
        }
        if !desc {
            dir.children.reverse();
        }
        let pos = sel_name.and_then(|name| dir.children.iter().position(|c| c.name == name));
        self.list.select(Some(pos.unwrap_or(0)));
    }

    fn fs_path(&self) -> PathBuf {
        let mut p = self.root_path.clone();
        let mut n = &self.root;
        for &i in &self.path {
            n = &n.children[i];
            p.push(&*n.name);
        }
        p
    }

    fn enter(&mut self) {
        let i = self.selected();
        if self.cur().children.get(i).is_some_and(|c| c.kind == Kind::Dir && (!c.children.is_empty() || cache::collapsed(c))) {
            self.path.push(i);
            if cache::collapsed(self.cur()) {
                self.rescan_at(&self.path.clone());
            }
            self.list = ListState::default().with_selected(Some(0));
            self.sort_cur();
            self.list.select(Some(0));
        }
    }

    fn up(&mut self) {
        if let Some(i) = self.path.pop() {
            self.list.select(Some(i));
            self.sort_cur();
        }
    }

    fn delete(&mut self) {
        let i = self.selected();
        let Some(child) = self.cur().children.get(i) else { return };
        let target = self.fs_path().join(&*child.name);
        match self.locate(&target) {
            Ok(_) => self.start_delete(vec![target], After::One(i)),
            Err(e) => self.message = Some(format!(" {e}")),
        }
    }

    /// Deletes `targets` from disk on another thread, so the screen can show
    /// how far it has got; `finish_delete` updates the tree once it is done.
    fn start_delete(&mut self, targets: Vec<PathBuf>, then: After) {
        let (mut items, mut bytes) = (0, 0);
        for n in targets.iter().filter_map(|t| self.find(t).ok()) {
            items += n.items;
            bytes += n.alloc;
        }
        let done = Arc::new(DeleteProgress::default());
        let (paths, progress) = (targets.clone(), done.clone());
        let worker = std::thread::spawn(move || {
            paths
                .iter()
                .map(|p| match delete_tree(p, &progress) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("could not delete {}: {e}", p.display())),
                    _ => Ok(()),
                })
                .collect()
        });
        self.deleting = Some(Deleting { targets, items, bytes, done, worker, then });
    }

    /// Once the delete thread is done, takes what it deleted out of the tree.
    /// Returns whether the screen needs redrawing.
    fn finish_delete(&mut self) -> bool {
        match &self.deleting {
            Some(d) if d.worker.is_finished() => {}
            busy => return busy.is_some(),
        }
        let d = self.deleting.take().unwrap();
        let n = d.targets.len();
        let results = d.worker.join().unwrap_or_else(|_| vec![Err("the delete crashed".into()); n]);
        let (mut freed, mut alloc, mut deleted, mut failed) = (0, 0, 0, Vec::new());
        for (p, r) in d.targets.iter().zip(results) {
            match r.and_then(|_| self.forget(p)) {
                Ok(n) => {
                    freed += n.private;
                    alloc += n.alloc;
                    deleted += 1;
                }
                Err(e) => failed.push(e),
            }
        }
        match d.then {
            After::One(i) => {
                self.message = Some(match failed.first() {
                    Some(e) => format!(" {e}; r rescans"),
                    None if alloc > freed => format!("deleted {} (freed about {}); its clones elsewhere may now be private, r rescans", d.targets[0].display(), human(freed)),
                    None => format!("deleted {} (freed about {})", d.targets[0].display(), human(freed)),
                });
                let len = self.cur().children.len();
                self.list.select(Some(i.min(len.saturating_sub(1))));
            }
            After::Marked { here, sel, page, refused } => {
                failed.splice(0..0, refused);
                self.show_after_delete(&here, sel, page);
                let mut msg = format!(" deleted {deleted} marked item{} (freed about {})", if deleted == 1 { "" } else { "s" }, human(freed));
                if let Some(e) = failed.first() {
                    msg.push_str(&format!("; {} failed, still marked: {e}", failed.len()));
                }
                self.message = Some(msg);
            }
        }
        true
    }

    /// Where `target` is in the tree, if apdu may delete it.
    fn locate(&self, target: &Path) -> Result<Vec<usize>, String> {
        let mut at = Vec::new();
        let mut n = &self.root;
        for name in self.components(target)? {
            let i = n.children.iter().position(|c| *c.name == *name).ok_or_else(|| format!("{} is not in the scan", target.display()))?;
            at.push(i);
            n = &n.children[i];
        }
        if at.is_empty() {
            return Err("cannot delete the folder apdu is browsing".into());
        }
        deletable(n)?;
        Ok(at)
    }

    /// Takes `target`, now gone from disk, out of the tree, with any
    /// suggestions and marks on or under it.
    fn forget(&mut self, target: &Path) -> Result<Node, String> {
        let at = self.locate(target)?;
        let (&last, parents) = at.split_last().unwrap();
        let mut d = &mut self.root;
        for &i in parents {
            d = &mut d.children[i];
        }
        let removed = d.children.remove(last);
        let mut d = &mut self.root;
        for k in 0..=parents.len() {
            d.alloc -= removed.alloc;
            d.private -= removed.private;
            d.items -= removed.items;
            d.files -= removed.files;
            if let Some(&i) = parents.get(k) {
                d = &mut d.children[i];
            }
        }
        if let (Some(s), Some(d)) = (&self.sugg_file, &self.deleted_file) {
            if let Err(e) = cache::record_deleted(s, d, &[target.to_path_buf()], unix_now()) {
                self.message = Some(format!(" could not record deleted suggestions in {}: {e}", d.display()));
            }
        }
        self.suggestions.retain(|s| !s.path.starts_with(target));
        self.marked.retain(|p| !p.starts_with(target));
        self.save_suggestions();
        self.save_marked();
        Ok(removed)
    }

    fn marked_under(&self, p: &Path) -> Option<&PathBuf> {
        self.marked.iter().find(|m| p.starts_with(m))
    }

    /// Marks `p` for deletion, or unmarks it. A marked folder takes the place
    /// of marks inside it, so every byte is counted once.
    fn toggle_mark(&mut self, p: &Path) {
        self.reload_marked();
        if let Some(i) = self.marked.iter().position(|m| m == p) {
            self.marked.remove(i);
            self.message = Some(format!(" unmarked {}", self.short(p)));
        } else if let Some(m) = self.marked_under(p) {
            self.message = Some(format!(" already marked: {} is marked", self.short(m)));
            return;
        } else {
            match self.find(p).and_then(|n| deletable(n).map(|_| n.private)) {
                Ok(private) => {
                    self.marked.retain(|m| !m.starts_with(p));
                    self.marked.push(p.to_path_buf());
                    self.message = Some(format!(" marked {} ({}) for deletion   D deletes everything marked", self.short(p), human(private)));
                }
                Err(e) => {
                    self.message = Some(format!(" {e}"));
                    return;
                }
            }
        }
        self.save_marked();
    }

    /// Picks up marks another apdu made or removed, so D never deletes
    /// something unmarked elsewhere.
    fn reload_marked(&mut self) {
        if let Some(f) = &self.marked_file {
            self.marked = cache::load_paths(f, &self.root_path).into_iter().map(|(p, _)| p).collect();
        }
    }

    fn marked_private(&self) -> u64 {
        self.marked.iter().filter_map(|p| self.find(p).ok()).map(|n| n.private).sum()
    }

    fn save_marked(&mut self) {
        if let Some(f) = &self.marked_file {
            if let Err(e) = cache::save_paths(f, &self.root_path, self.marked.iter().map(|p| (p.as_path(), ""))) {
                self.message = Some(format!(" could not save marks to {}: {e}", f.display()));
            }
        }
    }

    fn delete_marked(&mut self) {
        let here = self.fs_path();
        let sel = self.cur().children.get(self.selected()).map(|c| here.join(&*c.name));
        let page = self.page;
        let (mut go, mut refused) = (Vec::new(), Vec::new());
        for p in self.marked.clone() {
            self.expand(&p);
            match self.locate(&p) {
                Ok(_) => go.push(p),
                Err(e) => refused.push(e),
            }
        }
        self.start_delete(go, After::Marked { here, sel, page, refused });
    }

    /// Reopens the view after deleting marks: the selected item if it is
    /// still there, else the nearest folder that is.
    fn show_after_delete(&mut self, here: &Path, sel: Option<PathBuf>, page: Page) {
        match sel.filter(|s| self.find(s).is_ok()) {
            Some(s) => {
                let _ = self.goto(&s);
            }
            None => {
                let open = here.ancestors().find(|a| self.find(a).is_ok()).unwrap_or(&self.root_path).to_path_buf();
                let _ = self.goto(&open);
                if open != self.root_path {
                    self.enter();
                }
            }
        }
        self.page = page;
        self.page_list.select(Some(0));
    }

    fn adjust(&mut self, depth: usize, f: impl Fn(&mut Node)) {
        let path = self.path.clone();
        let mut n = &mut self.root;
        for (d, idx) in std::iter::once(None).chain(path.into_iter().map(Some)).enumerate().take(depth) {
            if d > 0 {
                n = &mut n.children[idx.unwrap()];
            }
            f(n);
        }
    }

    /// Scans the folder at `at` (indexes from the root) again and puts the
    /// result in place, carrying the change in size up to the root.
    fn rescan_at(&mut self, at: &[usize]) {
        let saved = std::mem::replace(&mut self.path, at.to_vec());
        let fresh = scan::scan_on(&self.fs_path(), scan::dev_of(&self.root_path), self.one_fs, &Progress::default());
        self.path = saved;
        self.replace_at(at, fresh);
    }

    /// Puts `fresh` in place of the folder at `at`, carrying the change in
    /// size up to the root.
    fn replace_at(&mut self, at: &[usize], mut fresh: Node) {
        let saved = std::mem::replace(&mut self.path, at.to_vec());
        let old = self.cur();
        fresh.name = old.name.clone();
        let fresh_modified = fresh.modified;
        let (da, dp, di) = (
            fresh.alloc as i64 - old.alloc as i64,
            fresh.private as i64 - old.private as i64,
            fresh.items as i64 - old.items as i64,
        );
        let df = fresh.files as i64 - old.files as i64;
        self.adjust(self.path.len(), |n| {
            n.alloc = n.alloc.saturating_add_signed(da);
            n.private = n.private.saturating_add_signed(dp);
            n.items = n.items.saturating_add_signed(di);
            n.files = n.files.saturating_add_signed(df);
            n.modified = n.modified.max(fresh_modified);
        });
        *self.cur_mut() = fresh;
        self.path = saved;
    }

    /// Fills in any folder on the way to `p` that the cache kept as a total only.
    fn expand(&mut self, p: &Path) {
        let Ok(names) = self.components(p) else { return };
        let mut at = Vec::new();
        let mut n = &self.root;
        for k in 0..=names.len() {
            if cache::collapsed(n) {
                self.rescan_at(&at);
                n = at.iter().fold(&self.root, |n, &i| &n.children[i]);
            }
            let Some(name) = names.get(k) else { break };
            let Some(i) = n.children.iter().position(|c| *c.name == **name) else { break };
            at.push(i);
            n = &n.children[i];
        }
    }

    /// Rescans the folder on screen on another thread, so the screen can show
    /// how far it has got; `finish_refresh` puts the result in place.
    fn rescan(&mut self) {
        let target = self.fs_path();
        let progress = Arc::new(Progress::default());
        let (p, t, dev, one_fs) = (progress.clone(), target.clone(), scan::dev_of(&self.root_path), self.one_fs);
        let worker = std::thread::spawn(move || scan::scan_on(&t, dev, one_fs, &p));
        self.refreshing = Some(Refreshing { target, items: self.cur().items, started: Instant::now(), progress, worker });
    }

    /// Once the rescan thread is done, puts what it found in the tree.
    /// Returns whether the screen needs redrawing.
    fn finish_refresh(&mut self) -> bool {
        match &self.refreshing {
            Some(r) if r.worker.is_finished() => {}
            busy => return busy.is_some(),
        }
        let r = self.refreshing.take().unwrap();
        let fresh = match r.worker.join() {
            Ok(n) => n,
            Err(_) => {
                self.message = Some(format!(" the rescan of {} crashed", r.target.display()));
                return true;
            }
        };
        match self.index_path(&r.target) {
            Some(at) => {
                let sel = self.cur().children.get(self.selected()).map(|c| c.name.clone());
                self.replace_at(&at, fresh);
                let pos = sel.and_then(|s| self.cur().children.iter().position(|c| c.name == s));
                self.list.select(Some(pos.unwrap_or(0)));
                self.free = volume_space(&self.root_path);
                self.sort_cur();
                self.message = Some(format!(" rescanned in {:.1}s", r.started.elapsed().as_secs_f64()));
            }
            None => self.message = Some(format!(" {} is no longer in the scan", r.target.display())),
        }
        true
    }

    /// Indexes from the root to the node at `p`.
    fn index_path(&self, p: &Path) -> Option<Vec<usize>> {
        let mut at = Vec::new();
        let mut n = &self.root;
        for name in self.components(p).ok()? {
            let i = n.children.iter().position(|c| *c.name == *name)?;
            at.push(i);
            n = &n.children[i];
        }
        Some(at)
    }

    fn short(&self, p: &Path) -> String {
        p.strip_prefix(&self.root_path).map_or_else(|_| p.display().to_string(), |r| r.display().to_string())
    }

    fn components(&self, p: &Path) -> Result<Vec<String>, String> {
        let rel = p.strip_prefix(&self.root_path).map_err(|_| format!("{} is not inside {}", p.display(), self.root_path.display()))?;
        Ok(rel.iter().map(|c| c.to_string_lossy().into_owned()).collect())
    }

    fn find(&self, p: &Path) -> Result<&Node, String> {
        let mut n = &self.root;
        for name in self.components(p)? {
            n = n
                .children
                .iter()
                .find(|c| *c.name == *name)
                .ok_or_else(|| format!("{} is not in the scan (new since it ran? r in the TUI rescans)", p.display()))?;
        }
        Ok(n)
    }

    fn goto(&mut self, p: &Path) -> Result<(), String> {
        self.find(p)?;
        let names = self.components(p)?;
        self.confirm = false;
        self.help = false;
        self.confirm_marked = false;
        self.page = Page::Tree;
        self.path.clear();
        self.list.select(Some(0));
        self.sort_cur();
        for (k, name) in names.iter().enumerate() {
            let i = self.cur().children.iter().position(|c| *c.name == **name).unwrap_or(0);
            if k + 1 == names.len() {
                self.list.select(Some(i));
            } else {
                self.path.push(i);
                self.list.select(Some(0));
                self.sort_cur();
            }
        }
        Ok(())
    }

    fn save_suggestions(&mut self) {
        if let Some(f) = &self.sugg_file {
            let marks = self.suggestions.iter().map(|s| (s.path.as_path(), s.reason.as_str()));
            if let Err(e) = cache::save_paths(f, &self.root_path, marks) {
                self.message = Some(format!(" could not save suggestions to {}: {e}", f.display()));
            }
        }
    }

    fn step_suggestion(&mut self, forward: bool) {
        let n = self.suggestions.len();
        if n == 0 {
            self.message = Some(" no agent suggestions".into());
            return;
        }
        let here = self.cur().children.get(self.selected()).map(|c| self.fs_path().join(&*c.name));
        let at = self.suggestions.iter().position(|s| Some(&s.path) == here.as_ref());
        let next = match (at, forward) {
            (Some(i), true) => (i + 1) % n,
            (Some(i), false) => (i + n - 1) % n,
            (None, true) => 0,
            (None, false) => n - 1,
        };
        self.open_suggestion(next);
    }

    fn open_suggestion(&mut self, i: usize) {
        let path = self.suggestions[i].path.clone();
        if let Err(e) = self.goto(&path) {
            self.message = Some(format!(" {e}"));
        }
    }

    fn mark(&self, p: &Path) -> Option<char> {
        if self.suggestions.iter().any(|s| s.path == p) {
            Some('*')
        } else if self.suggestions.iter().any(|s| s.path.starts_with(p)) {
            Some('+')
        } else {
            None
        }
    }

    /// Suggestions deleted in apdu whose paths are still gone.
    fn deleted(&self) -> Vec<(PathBuf, String, i64)> {
        let all = self.deleted_file.as_ref().map(|f| cache::load_deleted(f, &self.root_path)).unwrap_or_default();
        all.into_iter().filter(|(p, ..)| p.symlink_metadata().is_err()).collect()
    }

    fn describe(&self, p: &Path, n: &Node) -> Obj {
        let kind = match n.kind {
            Kind::File => "file",
            Kind::Dir => "dir",
            Kind::OtherFs => "other_fs",
            Kind::Duplicate => "duplicate",
        };
        let mut o = Obj::new();
        o.insert("path".into(), p.to_string_lossy().into_owned().into());
        o.insert("kind".into(), kind.into());
        o.insert("private".into(), n.private.into());
        o.insert("allocated".into(), n.alloc.into());
        o.insert("items".into(), n.items.into());
        o.insert("files".into(), n.files.into());
        o.insert("created".into(), Json::Num(n.created as f64));
        o.insert("created_iso".into(), iso(n.created));
        o.insert("modified".into(), Json::Num(n.modified as f64));
        o.insert("modified_iso".into(), iso(n.modified));
        if n.error {
            o.insert("unreadable".into(), true.into());
        }
        if self.marked_under(p).is_some() {
            o.insert("marked_for_deletion".into(), true.into());
        }
        o
    }

    fn handle(&mut self, req: &Obj) -> Json {
        let cmd = req.get("cmd").and_then(Json::str).unwrap_or("");
        let path = req.get("path").and_then(Json::str).map(PathBuf::from);
        let need_path = || path.clone().ok_or_else(|| format!("{cmd} needs a path"));
        if let Some(p) = &path {
            self.expand(p);
        }
        let result: Result<Json, String> = (|| match cmd {
            "hello" => Ok(obj([("ok", true.into()), ("root", self.root_path.to_string_lossy().into_owned().into())])),
            "goto" if self.deleting.is_some() => Err("apdu is deleting something; try again once it is done".into()),
            "goto" if self.refreshing.is_some() => Err("apdu is rescanning; try again once it is done".into()),
            "goto" => {
                let p = need_path()?;
                self.goto(&p)?;
                self.message = Some(format!(" agent: went to {}", self.short(&p)));
                Ok(obj([("ok", true.into())]))
            }
            "suggest" => {
                let p = need_path()?;
                self.find(&p)?;
                if let Some(d) = &self.deleted_file {
                    let _ = cache::forget_deleted(d, &p);
                }
                let reason = req.get("reason").and_then(Json::str).unwrap_or("").trim().to_string();
                self.suggestions.retain(|s| s.path != p);
                self.message = Some(format!(" agent suggests {}: {reason}   ] goes there", self.short(&p)));
                self.suggestions.push(Suggestion { path: p, reason });
                self.save_suggestions();
                Ok(obj([("ok", true.into()), ("suggestions", (self.suggestions.len() as u64).into())]))
            }
            "suggestions" => {
                let gone = self.deleted();
                self.suggestions.retain(|s| !gone.iter().any(|(d, ..)| s.path.starts_with(d)));
                let deleted = gone
                    .into_iter()
                    .map(|(p, reason, at)| obj([("path", p.to_string_lossy().into_owned().into()), ("reason", reason.into()), ("deleted_iso", iso(at))]))
                    .collect();
                let list = self
                    .suggestions
                    .iter()
                    .map(|s| {
                        let marked = self.marked_under(&s.path).is_some();
                        obj([("path", s.path.to_string_lossy().into_owned().into()), ("reason", s.reason.clone().into()), ("marked_for_deletion", marked.into())])
                    })
                    .collect();
                Ok(obj([("ok", true.into()), ("suggestions", Json::Arr(list)), ("deleted", Json::Arr(deleted))]))
            }
            "clear" => {
                let before = self.suggestions.len();
                match &path {
                    Some(p) => self.suggestions.retain(|s| s.path != *p),
                    None => self.suggestions.clear(),
                }
                self.save_suggestions();
                self.page_list.select(Some(0));
                Ok(obj([("ok", true.into()), ("removed", ((before - self.suggestions.len()) as u64).into())]))
            }
            "find" => {
                let name = req.get("name").and_then(Json::str).filter(|n| !n.is_empty()).ok_or("find needs a name")?;
                let base = path.clone().unwrap_or_else(|| self.root_path.clone());
                let limit = match req.get("limit") {
                    Some(Json::Num(k)) => *k as usize,
                    _ => 100,
                };
                let mut hits = Vec::new();
                find_named(self.find(&base)?, &base, &name.to_lowercase(), &mut hits);
                hits.sort_by(|a, b| b.1.private.cmp(&a.1.private).then_with(|| a.0.cmp(&b.0)));
                let private: u64 = hits.iter().map(|h| h.1.private).sum();
                let total = hits.len() as u64;
                let list = hits.into_iter().take(limit).map(|(p, n)| Json::Obj(self.describe(&p, n))).collect();
                Ok(obj([("ok", true.into()), ("total", total.into()), ("private_sum", private.into()), ("matches", Json::Arr(list))]))
            }
            "sizes" => {
                let p = need_path()?;
                let n = self.find(&p)?;
                let limit = match req.get("children") {
                    Some(Json::Num(k)) => *k as usize,
                    _ => 20,
                };
                let mut kids: Vec<&Node> = n.children.iter().collect();
                match req.get("sort").and_then(Json::str) {
                    Some("modified") => kids.sort_by_key(|c| c.modified),
                    Some("created") => kids.sort_by_key(|c| c.created),
                    _ => kids.sort_by(|a, b| b.private.cmp(&a.private).then_with(|| a.name.cmp(&b.name))),
                }
                let mut o = self.describe(&p, n);
                o.insert("ok".into(), true.into());
                o.insert("children_total".into(), (kids.len() as u64).into());
                let kids = kids.into_iter().take(limit).map(|c| Json::Obj(self.describe(&p.join(&*c.name), c))).collect();
                o.insert("children".into(), Json::Arr(kids));
                Ok(Json::Obj(o))
            }
            _ => Err(format!("unknown command {cmd:?}")),
        })();
        let gone = || path.as_ref().and_then(|p| self.deleted().into_iter().find(|(d, ..)| p.starts_with(d)));
        result
            .map_err(|e| match gone() {
                Some((d, _, at)) => format!("{} was deleted in apdu on {} after it was suggested; it is gone, don't suggest it again", d.display(), date(at)),
                None => e,
            })
            .unwrap_or_else(err)
    }

    fn compute_tiles(&self, area: Rect) -> Vec<Tile> {
        let dir = self.cur();
        let mut items: Vec<(TileKind, u64)> =
            dir.children.iter().enumerate().map(|(i, c)| (TileKind::Child(i), self.size(c))).filter(|&(_, s)| s > 0).collect();
        if self.free_tile() {
            items.push((TileKind::Free, self.free.0));
        }
        items.sort_by(|a, b| b.1.cmp(&a.1));
        let total: u64 = items.iter().map(|i| i.1).sum();
        let cells = area.width as u64 * area.height as u64;
        let shown = items.iter().take(300).take_while(|i| i.1 as u128 * cells as u128 >= 6 * total as u128).count();
        if shown < items.len() {
            let rest = items.split_off(shown);
            items.push((TileKind::Others(rest.len()), rest.iter().map(|i| i.1).sum()));
            items.sort_by(|a, b| b.1.cmp(&a.1));
        }
        let sizes: Vec<u64> = items.iter().map(|i| i.1).collect();
        treemap::layout(&sizes, area)
            .into_iter()
            .zip(items)
            .filter(|(r, _)| r.width > 0 && r.height > 0)
            .map(|(rect, (kind, size))| Tile { rect, kind, size })
            .collect()
    }

    fn free_tile(&self) -> bool {
        self.path.is_empty() && self.free.0 <= self.size(self.cur())
    }

    fn selected_tile(&self) -> Option<&Tile> {
        self.tiles.iter().find(|t| t.kind == TileKind::Child(self.selected()))
    }

    fn move_tile(&mut self, dx: i32, dy: i32) {
        let center = |r: &Rect| (r.x as i32 * 2 + r.width as i32, (r.y as i32 * 2 + r.height as i32) * 2);
        let Some(from) = self.selected_tile().map(|t| t.rect) else {
            if let Some(TileKind::Child(i)) = self.tiles.iter().map(|t| t.kind).find(|k| matches!(k, TileKind::Child(_))) {
                self.list.select(Some(i));
            }
            return;
        };
        let best = self
            .tiles
            .iter()
            .filter_map(|t| match t.kind {
                TileKind::Child(i) => Some((i, t.rect)),
                _ => None,
            })
            .filter(|(_, r)| match (dx, dy) {
                (1, _) => r.x >= from.right(),
                (-1, _) => r.right() <= from.x,
                (_, 1) => r.y >= from.bottom(),
                _ => r.bottom() <= from.y,
            })
            .map(|(i, r)| {
                let ((x, y), (fx, fy)) = (center(&r), center(&from));
                let (along, across) = if dx != 0 { ((x - fx).abs(), (y - fy).abs()) } else { ((y - fy).abs(), (x - fx).abs()) };
                (along + 2 * across, i)
            })
            .min();
        if let Some((_, i)) = best {
            self.list.select(Some(i));
        }
    }

    fn mouse(&mut self, m: MouseEvent) {
        let hit = self.tiles.iter().find(|t| t.rect.contains((m.column, m.row).into())).map(|t| t.kind);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(TileKind::Child(i)) = hit {
                    if self.selected() == i {
                        self.enter();
                    } else {
                        self.list.select(Some(i));
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right) => self.up(),
            _ => {}
        }
    }

    fn draw_map(&mut self, f: &mut ratatui::Frame, area: Rect) {
        self.tiles = self.compute_tiles(area);
        if self.tiles.is_empty() {
            let why = if self.col == Col::Private && self.cur().alloc > 0 {
                "Nothing here is private: every byte is shared with an APFS clone. Press a to size by allocated."
            } else {
                "Nothing here takes up space."
            };
            f.render_widget(Paragraph::new(why).wrap(Wrap { trim: false }), area);
            return;
        }
        let total: u64 = self.tiles.iter().map(|t| t.size).sum::<u64>().max(1);
        let selected = self.selected();
        let dir = self.cur();
        let here = self.fs_path();
        for (n, t) in self.tiles.iter().enumerate() {
            let (name, (r, g, b)) = match t.kind {
                TileKind::Child(i) => {
                    let c = &dir.children[i];
                    let name = if c.kind == Kind::File { c.name.to_string() } else { format!("{}/", c.name) };
                    let color = if self.marked_under(&here.join(&*c.name)).is_some() {
                        MARKED
                    } else if c.kind == Kind::File {
                        (75, 75, 82)
                    } else {
                        PALETTE[n % PALETTE.len()]
                    };
                    (name, color)
                }
                TileKind::Free => ("free space".to_string(), (25, 25, 25)),
                TileKind::Others(k) => (format!("{k} smaller items"), (55, 55, 60)),
            };
            let is_sel = t.kind == TileKind::Child(selected);
            let style = Style::new().bg(Color::Rgb(r, g, b)).fg(if t.kind == TileKind::Free { Color::Gray } else { Color::White });
            let text = vec![
                Line::from(name).bold(),
                Line::from(human(t.size)),
                Line::from(format!("{:.1}%", t.size as f64 * 100.0 / total as f64)).dim(),
            ];
            let mut p = Paragraph::new(text).style(style);
            if t.rect.width >= 4 && t.rect.height >= 3 {
                let border = if is_sel {
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(Color::Rgb(r.saturating_add(60), g.saturating_add(60), b.saturating_add(60)))
                };
                let kind = if is_sel { BorderType::Thick } else { BorderType::Rounded };
                p = p.block(Block::bordered().border_type(kind).border_style(border));
            } else if is_sel {
                p = p.style(style.bg(Color::Yellow).fg(Color::Black));
            }
            f.render_widget(p, t.rect);
        }
    }

    fn key(&mut self, code: KeyCode) -> bool {
        self.message = None;
        if self.deleting.is_some() {
            self.message = Some(" still deleting: keys work again once it is done".into());
            return true;
        }
        if self.refreshing.is_some() {
            if matches!(code, KeyCode::Char('q') | KeyCode::Esc) {
                return false;
            }
            self.message = Some(" still rescanning: keys work again once it is done (q quits)".into());
            return true;
        }
        if self.confirm {
            self.confirm = false;
            if matches!(code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.delete();
            }
            return true;
        }
        if self.help {
            self.help = false;
            return !matches!(code, KeyCode::Char('q'));
        }
        if self.pairing.take().is_some() {
            if let Some(m) = &self.mcp {
                m.cancel_pairing();
            }
            self.message = Some(" pairing cancelled: P opens a new pairing window".into());
            return true;
        }
        if code == KeyCode::Char('P') {
            match &self.mcp {
                Some(m) => self.pairing = Some(m.new_pairing()),
                None => self.message = Some(" MCP is off: start apdu with --mcp-lan to pair another computer, or --mcp for this one".into()),
            }
            return true;
        }
        if self.confirm_marked {
            self.confirm_marked = false;
            if matches!(code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.delete_marked();
            }
            return true;
        }
        if self.page != Page::Tree {
            self.page_key(code);
            return true;
        }
        let len = self.cur().children.len();
        let page = 20;
        let i = self.selected();
        let step = match code {
            KeyCode::Up | KeyCode::Char('k') => Some((0, -1)),
            KeyCode::Down | KeyCode::Char('j') => Some((0, 1)),
            KeyCode::Left | KeyCode::Char('h') => Some((-1, 0)),
            KeyCode::Right | KeyCode::Char('l') => Some((1, 0)),
            _ => None,
        };
        if self.map
            && let Some((dx, dy)) = step
        {
            self.move_tile(dx, dy);
            return true;
        }
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Char('t') => self.map = !self.map,
            KeyCode::Up | KeyCode::Char('k') => self.list.select(Some(i.saturating_sub(1))),
            KeyCode::Down | KeyCode::Char('j') => self.list.select(Some((i + 1).min(len.saturating_sub(1)))),
            KeyCode::PageUp => self.list.select(Some(i.saturating_sub(page))),
            KeyCode::PageDown => self.list.select(Some((i + page).min(len.saturating_sub(1)))),
            KeyCode::Home | KeyCode::Char('g') => self.list.select(Some(0)),
            KeyCode::End | KeyCode::Char('G') => self.list.select(Some(len.saturating_sub(1))),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.enter(),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.up(),
            KeyCode::Char('s') => {
                self.desc = if self.sort == Sort::Size { !self.desc } else { true };
                self.sort = Sort::Size;
                self.sort_cur();
            }
            KeyCode::Char('n') => {
                self.desc = if self.sort == Sort::Name { !self.desc } else { true };
                self.sort = Sort::Name;
                self.sort_cur();
            }
            KeyCode::Char('m') => {
                self.desc = if self.sort == Sort::Modified { !self.desc } else { true };
                self.sort = Sort::Modified;
                self.sort_cur();
            }
            KeyCode::Char('c') => {
                self.desc = if self.sort == Sort::Created { !self.desc } else { true };
                self.sort = Sort::Created;
                self.sort_cur();
            }
            KeyCode::Char('f') => {
                self.desc = if self.sort == Sort::Files { !self.desc } else { true };
                self.sort = Sort::Files;
                self.sort_cur();
            }
            KeyCode::Char('a') => {
                self.col = if self.col == Col::Private { Col::Alloc } else { Col::Private };
                self.sort_cur();
            }
            KeyCode::Char('d') if len > 0 => self.confirm = true,
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('r') => self.rescan(),
            KeyCode::Char(']') => self.step_suggestion(true),
            KeyCode::Char('[') => self.step_suggestion(false),
            KeyCode::Char('x') if len > 0 => self.toggle_mark(&self.fs_path().join(&*self.cur().children[i].name)),
            KeyCode::Char('D') => self.ask_delete_marked(),
            KeyCode::Char('S') => self.open_page(Page::Suggestions),
            KeyCode::Char('M') => self.open_page(Page::Marked),
            _ => {}
        }
        true
    }

    fn ask_delete_marked(&mut self) {
        self.reload_marked();
        if self.marked.is_empty() {
            self.message = Some(" nothing is marked for deletion: x marks the selected item".into());
        } else {
            self.confirm_marked = true;
        }
    }

    fn open_page(&mut self, page: Page) {
        self.page = if self.page == page { Page::Tree } else { page };
        self.page_list.select(Some(0));
    }

    fn page_paths(&self) -> Vec<PathBuf> {
        match self.page {
            Page::Tree => Vec::new(),
            Page::Suggestions => self.suggestions.iter().map(|s| s.path.clone()).collect(),
            Page::Marked => self.marked.clone(),
        }
    }

    fn page_key(&mut self, code: KeyCode) {
        let paths = self.page_paths();
        let (n, i) = (paths.len(), self.page_list.selected().unwrap_or(0));
        let last = n.saturating_sub(1);
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.page_list.select(Some(i.saturating_sub(1))),
            KeyCode::Down | KeyCode::Char('j') => self.page_list.select(Some((i + 1).min(last))),
            KeyCode::PageUp => self.page_list.select(Some(i.saturating_sub(10))),
            KeyCode::PageDown => self.page_list.select(Some((i + 10).min(last))),
            KeyCode::Home | KeyCode::Char('g') => self.page_list.select(Some(0)),
            KeyCode::End | KeyCode::Char('G') => self.page_list.select(Some(last)),
            KeyCode::Enter if i < n => {
                if let Err(e) = self.goto(&paths[i]) {
                    self.message = Some(format!(" {e}"));
                }
            }
            KeyCode::Char('x') if i < n => {
                self.toggle_mark(&paths[i]);
                if self.page == Page::Marked {
                    self.page_list.select(Some(i.min(last.saturating_sub(1))));
                }
            }
            KeyCode::Char('-') | KeyCode::Delete if i < n && self.page == Page::Suggestions => {
                self.suggestions.remove(i);
                self.save_suggestions();
                self.page_list.select(Some(i.min(last.saturating_sub(1))));
            }
            KeyCode::Char('D') => self.ask_delete_marked(),
            KeyCode::Char('S') => self.open_page(Page::Suggestions),
            KeyCode::Char('M') => self.open_page(Page::Marked),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.page = Page::Tree,
            _ => {}
        }
    }

    fn draw(&mut self, f: &mut ratatui::Frame) {
        let [header, columns, body, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
        let snaps = self.snapshots.lock().unwrap().clone().unwrap_or_else(|| "…".into());
        f.render_widget(
            Paragraph::new(format!(
                " {BIN}  {}   free {} of {}   {snaps} local snapshots + purgeable not counted   ? help",
                self.fs_path().display(),
                human(self.free.0),
                human(self.free.1)
            ))
            .style(Style::new().bg(Color::Blue).fg(Color::White)),
            header,
        );
        let mut badges = Vec::new();
        if let Some(m) = &self.mcp {
            let lan = if m.lan { " (LAN)" } else { "" };
            badges.push(Span::styled(format!(" MCP{lan} "), Style::new().bg(Color::Green).fg(Color::Black).bold()));
            if let (Some(_), mcp::PairState::Open(d)) = (&self.pairing, m.pair_state()) {
                let left = format!(" PAIRING {}:{:02} ", d.as_secs() / 60, d.as_secs() % 60);
                badges.push(Span::styled(left, Style::new().bg(Color::Magenta).fg(Color::White).bold()));
            }
        }
        if !self.marked.is_empty() {
            let text = format!(" {} marked, frees {} (M, D deletes) ", self.marked.len(), human(self.marked_private()));
            badges.push(Span::styled(text, Style::new().bg(Color::Red).fg(Color::White).bold()));
        }
        if let n @ 1.. = self.suggestions.len() {
            let text = format!(" {n} agent suggestion{} (S) ", if n == 1 { "" } else { "s" });
            badges.push(Span::styled(text, Style::new().bg(Color::Yellow).fg(Color::Black).bold()));
        }
        f.render_widget(Line::from(badges).right_aligned(), header);
        if self.page != Page::Tree {
            self.draw_page(f, columns, body, footer);
        } else if self.map {
            let by = if self.col == Col::Private { "private" } else { "allocated" };
            let free = if self.path.is_empty() && !self.free_tile() { "   free space is bigger than this folder, so not drawn" } else { "" };
            f.render_widget(
                Line::from(format!(" tiles sized by {by} (a switches)   arrows/hjkl move   Enter/click opens   Backspace/right-click up{free}")).dim(),
                columns,
            );
        } else {
        let mark = |c: Col| if self.col == c { Modifier::BOLD | Modifier::UNDERLINED } else { Modifier::DIM };
        f.render_widget(
            Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:>10}", "private"), mark(Col::Private)),
                Span::raw(" "),
                Span::styled(format!("{:>10}", "allocated"), mark(Col::Alloc)),
                Span::raw(" "),
                Span::styled(format!("{:>10}", "modified"), if self.sort == Sort::Modified { Modifier::BOLD | Modifier::UNDERLINED } else { Modifier::DIM }),
                Span::raw(" "),
                Span::styled(format!("{:>10}", "created"), if self.sort == Sort::Created { Modifier::BOLD | Modifier::UNDERLINED } else { Modifier::DIM }),
                Span::raw(" "),
                Span::styled(format!("{:>8}", "files"), if self.sort == Sort::Files { Modifier::BOLD | Modifier::UNDERLINED } else { Modifier::DIM }),
                Span::raw("                    name").dim(),
            ]),
            columns,
        );
        }

        if self.page == Page::Tree && self.map {
            self.draw_map(f, body);
        } else if self.page == Page::Tree {
        let dir = self.cur();
        let total = self.size(dir).max(1);
        let biggest = dir.children.iter().map(|c| self.size(c)).max().unwrap_or(0).max(1);
        let here = self.fs_path();
        let items: Vec<ListItem> = dir
            .children
            .iter()
            .map(|c| {
                let s = self.size(c);
                let filled = ((s as f64 / biggest as f64) * 10.0).round() as usize;
                let flag = if c.error { '!' } else if c.kind == Kind::OtherFs { '>' } else if c.kind == Kind::Duplicate { '=' } else { ' ' };
                let slash = if c.kind == Kind::File { "" } else { "/" };
                let p = here.join(&*c.name);
                let mark = match self.mark(&p) {
                    Some('*') => Span::raw("*").yellow().bold(),
                    Some(m) => Span::raw(m.to_string()).yellow(),
                    None => Span::raw(" "),
                };
                ListItem::new(Line::from(vec![
                    self.del_mark(&p),
                    mark,
                    Span::raw(format!(
                        "{:>10} {:>10} {:>10} {:>10} {:>8} [{:<10}] {:>5.1}% {flag}{}{slash}",
                        human(c.private),
                        human(c.alloc),
                        date(c.modified),
                        date(c.created),
                        if c.kind == Kind::File { String::new() } else { count(c.files) },
                        "#".repeat(filled),
                        s as f64 * 100.0 / total as f64,
                        c.name
                    )),
                ]))
            })
            .collect();
        let list = List::new(items).highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
        let mut state = self.list.clone();
        f.render_stateful_widget(list, body, &mut state);
        self.list = state;
        }

        let dir = self.cur();
        let sort = match (self.sort, self.desc) {
            (Sort::Size, true) => "size desc",
            (Sort::Size, false) => "size asc",
            (Sort::Name, true) => "name asc",
            (Sort::Name, false) => "name desc",
            (Sort::Modified, true) => "modified oldest first",
            (Sort::Modified, false) => "modified newest first",
            (Sort::Created, true) => "created oldest first",
            (Sort::Created, false) => "created newest first",
            (Sort::Files, true) => "files most first",
            (Sort::Files, false) => "files fewest first",
        };
        let status = self.message.clone().unwrap_or_else(|| {
            format!(
                " private {}   allocated {}   shared by clones {}   items {}   files {}   newest change {}   sort {sort}{}",
                human(dir.private),
                human(dir.alloc),
                human(dir.alloc.saturating_sub(dir.private)),
                dir.items,
                dir.files,
                date(dir.modified),
                match (self.sort, self.col) {
                    (Sort::Size, Col::Private) => " by private",
                    (Sort::Size, Col::Alloc) => " by allocated",
                    _ => "",
                }
            )
        });
        let status = match (self.map, self.message.is_none(), dir.children.get(self.selected())) {
            (true, true, Some(c)) => format!(
                " {}{}   private {}   allocated {}   items {}   files {}   modified {}   created {}   Enter opens, Backspace goes up, t list view",
                c.name,
                if c.kind == Kind::File { "" } else { "/" },
                human(c.private),
                human(c.alloc),
                c.items,
                c.files,
                date(c.modified),
                date(c.created)
            ),
            _ => status,
        };
        let selected = dir.children.get(self.selected()).map(|c| self.fs_path().join(&*c.name));
        let reason = self.suggestions.iter().find(|s| Some(&s.path) == selected.as_ref()).filter(|_| self.message.is_none());
        let footer_style = Style::new().bg(Color::Blue).fg(Color::White);
        match reason.filter(|_| self.page == Page::Tree) {
            Some(s) => f.render_widget(
                Paragraph::new(format!(" agent: {}   ] [ next/prev, S list", s.reason)).style(footer_style.bg(Color::Yellow).fg(Color::Black)),
                footer,
            ),
            None if self.page == Page::Tree => f.render_widget(Paragraph::new(status).style(footer_style), footer),
            None => {}
        }

        if self.help {
            let area = centered(f.area(), 68, HELP.lines().count() as u16 + 2);
            f.render_widget(Clear, area);
            f.render_widget(Paragraph::new(HELP).block(Block::bordered().title(format!(" {BIN} help "))), area);
        }
        if self.confirm {
            let name = dir.children.get(self.selected()).map(|c| c.name.to_string()).unwrap_or_default();
            let area = centered(f.area(), 60, 5);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(format!("Delete {}?\n\ny = yes, anything else = no", self.fs_path().join(name).display()))
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title(" confirm delete ").red()),
                area,
            );
        }
        if let (Some(m), Some(code)) = (&self.mcp, &self.pairing) {
            self.draw_pairing(f, m, code);
        }
        if self.confirm_marked {
            let shown = 8;
            let mut text = format!(
                "Delete {} marked item{}, freeing about {}?\n\n",
                self.marked.len(),
                if self.marked.len() == 1 { "" } else { "s" },
                human(self.marked_private())
            );
            for p in self.marked.iter().take(shown) {
                text.push_str(&format!("  {}\n", self.short(p)));
            }
            if self.marked.len() > shown {
                text.push_str(&format!("  and {} more (M lists them)\n", self.marked.len() - shown));
            }
            text.push_str("\ny = yes, anything else = no");
            let w = f.area().width.saturating_sub(8).min(90);
            let inner = w.saturating_sub(2).max(1) as usize;
            let h = text.lines().map(|l| l.chars().count().max(1).div_ceil(inner)).sum::<usize>() as u16 + 2;
            let area = centered(f.area(), w, h);
            f.render_widget(Clear, area);
            f.render_widget(
                Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::bordered().title(" delete everything marked ").red()),
                area,
            );
        }
        if let Some(d) = &self.deleting {
            self.draw_deleting(f, d);
        }
        if let Some(r) = &self.refreshing {
            self.draw_refreshing(f, r);
        }
    }

    fn draw_refreshing(&self, f: &mut ratatui::Frame, r: &Refreshing) {
        let p = &r.progress;
        let items = p.items.load(Relaxed);
        let current = PathBuf::from(p.current.lock().unwrap().clone());
        let name = match self.short(&r.target) {
            s if s.is_empty() => r.target.display().to_string(),
            s => s,
        };
        let lines = vec![
            Line::from(format!(" Rescanning {name}")),
            Line::from(format!(
                " {items} of about {} items, {} allocated, {} errors, {:.1}s",
                r.items,
                human(p.alloc.load(Relaxed)),
                p.errors.load(Relaxed),
                r.started.elapsed().as_secs_f64()
            )),
            Line::from(format!(" in {}", self.short(&current))).dim(),
        ];
        let area = centered(f.area(), f.area().width.saturating_sub(8).min(80), 7);
        let block = Block::bordered().title(" refreshing ").cyan();
        let inner = block.inner(area);
        f.render_widget(Clear, area);
        f.render_widget(block, area);
        let [text, bar] = Layout::vertical([Constraint::Length(4), Constraint::Length(1)]).areas(inner);
        f.render_widget(Paragraph::new(lines), text);
        let ratio = if r.items > 0 { (items as f64 / r.items as f64).min(1.0) } else { 0.0 };
        f.render_widget(Gauge::default().ratio(ratio).gauge_style(Style::new().cyan()), bar.inner(ratatui::layout::Margin::new(1, 0)));
    }

    fn draw_deleting(&self, f: &mut ratatui::Frame, d: &Deleting) {
        let (items, bytes) = (d.done.items.load(Relaxed), d.done.bytes.load(Relaxed));
        let folder = d.done.folder.lock().unwrap().clone();
        let what = match d.targets.as_slice() {
            [one] => self.short(one),
            many => format!("{} marked items", many.len()),
        };
        let lines = vec![
            Line::from(format!(" Deleting {what}")),
            Line::from(format!(" {items} of {} items, {} of {}", d.items, human(bytes), human(d.bytes))),
            Line::from(format!(" in {}", self.short(&folder))).dim(),
        ];
        let area = centered(f.area(), f.area().width.saturating_sub(8).min(80), 7);
        let block = Block::bordered().title(" deleting ").red();
        let inner = block.inner(area);
        f.render_widget(Clear, area);
        f.render_widget(block, area);
        let [text, bar] = Layout::vertical([Constraint::Length(4), Constraint::Length(1)]).areas(inner);
        f.render_widget(Paragraph::new(lines), text);
        let ratio = if d.items > 0 { (items as f64 / d.items as f64).min(1.0) } else { 0.0 };
        f.render_widget(Gauge::default().ratio(ratio).gauge_style(Style::new().red()), bar.inner(ratatui::layout::Margin::new(1, 0)));
    }

    /// Returns whether the screen needs redrawing.
    fn tick(&mut self) -> bool {
        self.finish_delete() | self.finish_refresh() | self.tick_pairing()
    }

    /// Closes the pairing window once a computer has paired or it has run
    /// out. Returns whether anything changed.
    fn tick_pairing(&mut self) -> bool {
        let (Some(m), Some(_)) = (&self.mcp, &self.pairing) else { return false };
        let msg = match m.pair_state() {
            mcp::PairState::Open(_) => return false,
            mcp::PairState::Paired(from) => format!(" paired with {from}: apdu is now an MCP server in Claude Code there"),
            mcp::PairState::Closed => " the pairing window closed unused: P opens a new one".into(),
        };
        m.cancel_pairing();
        self.pairing = None;
        self.message = Some(msg);
        true
    }

    fn draw_pairing(&self, f: &mut ratatui::Frame, m: &mcp::Running, code: &str) {
        let left = match m.pair_state() {
            mcp::PairState::Open(d) => format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60),
            _ => "0:00".into(),
        };
        let mut lines = vec![
            Line::from(if m.lan { " On the other computer, type:" } else { " In a terminal on this Mac, type:" }),
            Line::from(""),
            Line::from(format!("   {}", m.pair_command(&m.host, code))).bold().green(),
            Line::from(""),
        ];
        if let Some(ip) = m.ip.as_ref().filter(|_| m.lan) {
            lines.push(Line::from(" or, if .local names don't resolve there:"));
            lines.push(Line::from(format!("   {}", m.pair_command(ip, code))).green());
            lines.push(Line::from(""));
        }
        lines.push(Line::from(" It adds apdu to Claude Code there. Without Claude Code, it prints"));
        lines.push(Line::from(" the URL and token for another MCP client."));
        if !m.lan {
            lines.push(Line::from(" (This server only listens on this Mac; --mcp-lan pairs another computer.)").dim());
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw(" Code "),
            Span::raw(code.to_string()).bold().yellow(),
            Span::raw(format!(" works once. Pairing closes in {left}, or on any key.")),
        ]));
        let area = centered(f.area(), 78, lines.len() as u16 + 2);
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" pair an agent with apdu ").green()), area);
    }

    fn del_mark(&self, p: &Path) -> Span<'static> {
        match self.marked_under(p) {
            Some(m) if m == p => Span::raw("x").red().bold(),
            Some(_) => Span::raw("x").red().dim(),
            None if self.marked.iter().any(|m| m.starts_with(p)) => Span::raw("+").red(),
            None => Span::raw(" "),
        }
    }

    fn draw_page(&mut self, f: &mut ratatui::Frame, columns: Rect, body: Rect, footer: Rect) {
        let sugg = self.page == Page::Suggestions;
        let title = if sugg {
            " agent suggestions: files and folders agents think you could delete"
        } else {
            " marked for deletion: D deletes all of them (it asks first)"
        };
        f.render_widget(Line::from(title).bold(), columns);
        let paths = self.page_paths();
        let rows: Vec<ListItem> = if paths.is_empty() {
            let empty = if sugg {
                " no suggestions yet: agents add them with `apdu suggest <path> <reason>` or over MCP"
            } else {
                " nothing marked: x marks the selected item, here or in the tree"
            };
            vec![ListItem::new(empty).dim()]
        } else {
            paths
                .iter()
                .map(|p| {
                    let node = self.find(p).ok();
                    let size = node.map(|n| human(n.private)).unwrap_or_else(|| "?".into());
                    let slash = if node.is_some_and(|n| n.kind != Kind::File) { "/" } else { "" };
                    let mut line = vec![self.del_mark(p), Span::raw(format!(" {size:>10}  ")), Span::raw(format!("{}{slash}", self.short(p))).bold()];
                    if node.is_none() {
                        line.push(Span::raw("   not in the scan, r rescans").dim());
                    }
                    let reason = self.suggestions.iter().find(|s| s.path == *p).map(|s| s.reason.as_str());
                    match reason {
                        Some(r) if sugg || !r.is_empty() => {
                            ListItem::new(vec![Line::from(line), Line::from(format!("              {}", if sugg { r.to_string() } else { format!("agent: {r}") })).dim()])
                        }
                        _ => ListItem::new(Line::from(line)),
                    }
                })
                .collect()
        };
        let list = List::new(rows).highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD));
        let mut state = self.page_list.clone();
        f.render_stateful_widget(list, body, &mut state);
        self.page_list = state;
        let keys = if sugg {
            " x mark/unmark for deletion   - dismiss   Enter show in tree   D delete marked   M marked   Esc back"
        } else {
            " x unmark   Enter show in tree   D delete marked   S suggestions   Esc back"
        };
        let text = self.message.clone().unwrap_or_else(|| keys.into());
        f.render_widget(Paragraph::new(text).style(Style::new().bg(Color::Blue).fg(Color::White)), footer);
    }
}

fn deletable(n: &Node) -> Result<(), String> {
    match n.kind {
        Kind::OtherFs => Err("that is on another filesystem".into()),
        Kind::Duplicate => Err("that is already counted at another path; delete it there".into()),
        Kind::Dir | Kind::File => Ok(()),
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn ago(t: std::time::SystemTime) -> String {
    let s = t.elapsed().map_or(0, |d| d.as_secs());
    match s {
        0..120 => format!("{s}s"),
        120..7200 => format!("{}m", s / 60),
        7200..172_800 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

/// The tree for `root`: the cached scan brought up to date when FSEvents can
/// vouch for everything since it was saved, otherwise a full scan.
fn build(root: &Path, one_fs: bool, use_cache: bool, progress: &Progress) -> (Node, String) {
    let started = Instant::now();
    let file = cache::file(root, one_fs).filter(|_| use_cache);
    let id = fsevents::current_id();
    let uuid = fsevents::uuid(scan::dev_of(root) as libc::dev_t);
    let stage = |s| *progress.stage.lock().unwrap() = s;
    let save = |tree: &Node| {
        if let (Some(f), Some(uuid)) = (&file, uuid) {
            stage("Saving the scan of");
            let meta = cache::Meta { event_id: id, uuid, saved: std::time::SystemTime::now() };
            if let Err(e) = cache::save(f, root, one_fs, &meta, tree) {
                eprintln!("{BIN}: could not save the scan to {}: {e}", f.display());
            }
        }
    };
    if let (Some(f), Some(uuid)) = (&file, uuid) {
        stage("Loading the last scan of");
        if let Ok((meta, mut tree)) = cache::load(f, root, one_fs)
            && meta.uuid == uuid
            && meta.event_id <= id
        {
            stage("Checking what changed in");
            if let Some(changes) = fsevents::changes_since(root, meta.event_id, Duration::from_secs(60)) {
                stage("Rescanning what changed in");
                let n = cache::refresh(&mut tree, root, changes, one_fs, progress);
                save(&tree);
                let secs = started.elapsed().as_secs_f64();
                return (tree, format!(" scan from {} ago, {n} changed folders rescanned, ready in {secs:.1}s", ago(meta.saved)));
            }
        }
    }
    stage("Scanning");
    let tree = scan::scan(root, one_fs, progress);
    let secs = started.elapsed().as_secs_f64();
    save(&tree);
    (tree, format!(" scanned in {secs:.1}s"))
}

fn draw_progress(f: &mut ratatui::Frame, root: &Path, p: &Progress, started: Instant) {
    let text = format!(
        "{} {}\n\n{} items   {} allocated   {} errors   {:.0}s\n\n{}\n\nq to abort",
        Some(*p.stage.lock().unwrap()).filter(|s| !s.is_empty()).unwrap_or("Scanning"),
        root.display(),
        p.items.load(Relaxed),
        human(p.alloc.load(Relaxed)),
        p.errors.load(Relaxed),
        started.elapsed().as_secs_f64(),
        p.current.lock().unwrap()
    );
    let area = centered(f.area(), f.area().width.saturating_sub(4), 9);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).block(Block::bordered().title(format!(" {BIN} "))), area);
}

struct Opts {
    one_fs: bool,
    use_cache: bool,
    fast: bool,
    /// Serve MCP: (on the LAN, port).
    mcp: Option<(bool, u16)>,
}

fn run(terminal: &mut DefaultTerminal, root_path: PathBuf, opts: Opts) -> std::io::Result<()> {
    let Opts { one_fs, use_cache, fast, mcp } = opts;
    let snapshots = Arc::new(Mutex::new(None));
    let snap = snapshots.clone();
    std::thread::spawn(move || *snap.lock().unwrap() = Some(count_snapshots().unwrap_or_else(|| "?".into())));

    let (tx, rx) = std::sync::mpsc::channel();
    let _server = agent::listen(tx.clone()).ok();
    let mcp = mcp.map(|(lan, port)| mcp::start(lan, port, root_path.clone(), tx));
    let requests = || rx.try_iter();

    let progress = Arc::new(Progress::default());
    let (p, rp) = (progress.clone(), root_path.clone());
    let worker = std::thread::spawn(move || {
        background(fast);
        build(&rp, one_fs, use_cache, &p)
    });
    let started = Instant::now();
    while !worker.is_finished() {
        for r in requests() {
            let answer = match r.body.get("cmd").and_then(Json::str) {
                Some("hello") => obj([("ok", true.into()), ("root", root_path.to_string_lossy().into_owned().into()), ("scanning", true.into())]),
                _ => err(format!("apdu is still scanning {} ({} items so far); try again when it finishes", root_path.display(), progress.items.load(Relaxed))),
            };
            let _ = r.reply.send(answer);
        }
        terminal.draw(|f| draw_progress(f, &root_path, &progress, started))?;
        if event::poll(Duration::from_millis(100))?
            && let Event::Key(k) = event::read()?
            && k.kind == KeyEventKind::Press
            && matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            return Ok(());
        }
    }
    let (root, built) = worker.join().expect("scan thread panicked");

    let mut app = App {
        root,
        root_path: root_path.clone(),
        path: Vec::new(),
        list: ListState::default().with_selected(Some(0)),
        sort: Sort::Size,
        desc: true,
        col: Col::Private,
        one_fs,
        map: false,
        tiles: Vec::new(),
        confirm: false,
        help: false,
        message: Some(match &mcp {
            Some(Ok(m)) => format!("{built}   MCP on {}   P pairs an agent", m.url()),
            Some(Err(e)) => format!("{built}   MCP is off: {e}"),
            None => built,
        }),
        free: volume_space(&root_path),
        snapshots,
        mcp: mcp.and_then(|m| m.ok()),
        pairing: None,
        suggestions: cache::suggestions_file()
            .map(|f| cache::load_paths(&f, &root_path).into_iter().map(|(path, reason)| Suggestion { path, reason }).collect())
            .unwrap_or_default(),
        sugg_file: cache::suggestions_file(),
        deleted_file: cache::deleted_file(),
        marked: cache::marked_file().map(|f| cache::load_paths(&f, &root_path).into_iter().map(|(p, _)| p).collect()).unwrap_or_default(),
        marked_file: cache::marked_file(),
        confirm_marked: false,
        deleting: None,
        refreshing: None,
        page: Page::Tree,
        page_list: ListState::default(),
    };
    let saved: Vec<PathBuf> = app.marked.iter().chain(app.suggestions.iter().map(|s| &s.path)).cloned().collect();
    for p in &saved {
        app.expand(p);
    }
    app.sort_cur();
    app.list.select(Some(0));
    let mut drawn = Instant::now() - Duration::from_secs(1);
    let mut dirty = true;
    loop {
        if dirty || drawn.elapsed() >= Duration::from_millis(500) {
            terminal.draw(|f| app.draw(f))?;
            drawn = Instant::now();
            dirty = false;
        }
        for r in requests() {
            let _ = r.reply.send(app.handle(&r.body));
            dirty = true;
        }
        dirty |= app.tick();
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        dirty = true;
        let was_map = app.map;
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press && !app.key(k.code) => return Ok(()),
            Event::Mouse(m) if app.map => app.mouse(m),
            _ => {}
        }
        if app.map != was_map {
            if app.map {
                execute!(std::io::stdout(), EnableMouseCapture)?;
            } else {
                execute!(std::io::stdout(), DisableMouseCapture)?;
            }
        }
    }
}

fn print(root_path: &Path, one_fs: bool, use_cache: bool, n: usize, bytes: bool) {
    let human = |b: u64| if bytes { b.to_string() } else { human(b) };
    let (mut root, built) = build(root_path, one_fs, use_cache, &Progress::default());
    let (free, total) = volume_space(root_path);
    println!(
        "{}  private {}  allocated {}  items {}  free {} of {}\n{}",
        root_path.display(),
        human(root.private),
        human(root.alloc),
        root.items,
        human(free),
        human(total),
        built.trim()
    );
    root.children.sort_by(|a, b| b.private.cmp(&a.private));
    println!("{:>10} {:>10} {:>10}  name", "private", "allocated", "modified");
    for c in root.children.iter().take(n) {
        let slash = if c.kind == Kind::File { "" } else { "/" };
        println!("{:>10} {:>10} {:>10}  {}{slash}", human(c.private), human(c.alloc), date(c.modified), c.name);
    }
}

fn absolute(p: &str) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| std::path::absolute(p).unwrap_or_else(|_| p.into()))
}

fn client(args: &[String]) -> i32 {
    let fail = |msg: String| {
        eprintln!("{BIN}: {msg}");
        1
    };
    let mut pid = None;
    let mut children = None;
    let mut sort = None;
    let mut limit = None;
    let mut rest = Vec::new();
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--pid" => pid = it.next().and_then(|n| n.parse::<u32>().ok()),
            "--children" => children = it.next().and_then(|n| n.parse::<u64>().ok()),
            "--sort" => sort = it.next().cloned(),
            "--limit" => limit = it.next().and_then(|n| n.parse::<u64>().ok()),
            _ => rest.push(a.as_str()),
        }
    }
    let cmd = args[0].as_str();
    let mut req = Obj::new();
    req.insert("cmd".into(), cmd.into());
    let path = match (cmd, rest.as_slice()) {
        ("goto" | "sizes", [p]) => Some(absolute(p)),
        ("suggest", [p, reason @ ..]) if !reason.is_empty() => {
            req.insert("reason".into(), reason.join(" ").into());
            Some(absolute(p))
        }
        ("find", [name, p @ ..]) if p.len() <= 1 => {
            req.insert("name".into(), (*name).into());
            Some(absolute(p.first().copied().unwrap_or(".")))
        }
        ("clear", [p]) => Some(absolute(p)),
        ("clear" | "suggestions" | "instances", []) => None,
        _ => {
            eprintln!("{USAGE}");
            return 2;
        }
    };
    if let Some(p) = &path {
        req.insert("path".into(), p.to_string_lossy().into_owned().into());
    }
    if let Some(n) = children {
        req.insert("children".into(), n.into());
    }
    if let Some(s) = sort {
        req.insert("sort".into(), s.into());
    }
    if let Some(n) = limit {
        req.insert("limit".into(), n.into());
    }

    let all = agent::instances();
    if all.is_empty() {
        return fail(format!("no {BIN} TUI is running; start one in a terminal with `{BIN} <dir>`"));
    }
    let roots = || all.iter().map(|i| format!("pid {} browsing {}", i.pid, i.root.display())).collect::<Vec<_>>().join(", ");
    if cmd == "instances" {
        let list = all.iter().map(|i| obj([("pid", (i.pid as u64).into()), ("root", i.root.to_string_lossy().into_owned().into())]));
        println!("{}", obj([("ok", true.into()), ("instances", Json::Arr(list.collect()))]));
        return 0;
    }
    let targets: Vec<&agent::Instance> = match (pid, &path) {
        (Some(pid), _) => match all.iter().find(|i| i.pid == pid) {
            Some(i) => vec![i],
            None => return fail(format!("no {BIN} TUI with pid {pid} (running: {})", roots())),
        },
        (None, Some(p)) => match agent::pick(&all, Some(p)) {
            Some(i) => vec![i],
            None => return fail(format!("no running {BIN} TUI is browsing a folder holding {} (running: {})", p.display(), roots())),
        },
        (None, None) => all.iter().collect(),
    };

    let req = Json::Obj(req);
    let mut merged = Vec::new();
    let mut deleted: Vec<Json> = Vec::new();
    let mut removed = 0.0;
    for inst in &targets {
        let answer = match agent::call(&inst.socket, &req) {
            Ok(Json::Obj(o)) => o,
            Ok(_) => return fail("unexpected answer from the TUI".into()),
            Err(e) => return fail(format!("could not reach the TUI (pid {}): {e}", inst.pid)),
        };
        if answer.get("ok") != Some(&Json::Bool(true)) {
            return fail(answer.get("error").and_then(Json::str).unwrap_or("request failed").into());
        }
        if targets.len() == 1 {
            println!("{}", Json::Obj(answer));
            return 0;
        }
        if let Some(Json::Arr(a)) = answer.get("deleted") {
            deleted.extend(a.iter().filter(|d| !deleted.contains(d)).cloned().collect::<Vec<_>>());
        }
        match (answer.get("suggestions"), answer.get("removed")) {
            (Some(Json::Arr(a)), _) => merged.extend(a.iter().cloned()),
            (_, Some(Json::Num(n))) => removed += n,
            _ => {}
        }
    }
    if cmd == "clear" {
        println!("{}", obj([("ok", true.into()), ("removed", Json::Num(removed))]));
    } else {
        println!("{}", obj([("ok", true.into()), ("suggestions", Json::Arr(merged)), ("deleted", Json::Arr(deleted))]));
    }
    0
}

/// `marked` and `delete-marked`: works on the saved marks, with no TUI needed.
fn marked_cli(delete: bool, yes: bool) -> i32 {
    let Some(file) = cache::marked_file() else {
        eprintln!("{BIN}: marks are not saved when running as root or without $HOME");
        return 1;
    };
    let marked: Vec<PathBuf> = cache::load_paths(&file, Path::new("/")).into_iter().map(|(p, _)| p).collect();
    if marked.is_empty() {
        println!("nothing is marked for deletion (x in the TUI marks the selected item)");
        return 0;
    }
    for p in &marked {
        println!("{}", p.display());
    }
    if !delete {
        return 0;
    }
    if !yes {
        eprint!("delete {} item{}? [y/N] ", marked.len(), if marked.len() == 1 { "" } else { "s" });
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            eprintln!("nothing deleted");
            return 1;
        }
    }
    let mut failed = 0;
    for p in &marked {
        let done = match p.symlink_metadata() {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
            _ => std::fs::remove_file(p),
        };
        match done {
            Ok(()) => println!("deleted {}", p.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!("{BIN}: could not delete {}: {e}", p.display());
                failed += 1;
            }
        }
    }
    let left: Vec<&PathBuf> = marked.iter().filter(|p| p.symlink_metadata().is_ok()).collect();
    let gone: Vec<PathBuf> = marked.iter().filter(|p| p.symlink_metadata().is_err()).cloned().collect();
    if let (Some(s), Some(d)) = (cache::suggestions_file(), cache::deleted_file()) {
        if let Err(e) = cache::record_deleted(&s, &d, &gone, unix_now()) {
            eprintln!("{BIN}: could not record deleted suggestions in {}: {e}", d.display());
        }
    }
    if let Err(e) = cache::save_paths(&file, Path::new("/"), left.iter().map(|p| (p.as_path(), ""))) {
        eprintln!("{BIN}: could not save marks to {}: {e}", file.display());
    }
    if !agent::instances().is_empty() {
        eprintln!("a running {BIN} TUI still shows the old sizes; r there rescans");
    }
    (failed > 0) as i32
}

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos: u32, relpri: i32) -> i32;
    fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
}

const QOS_CLASS_UTILITY: u32 = 0x11;
const IOPOL_TYPE_DISK: i32 = 0;
const IOPOL_SCOPE_PROCESS: i32 = 0;
const IOPOL_UTILITY: i32 = 4;

/// By default a scan leaves room for everything else: 80% of the cores, at
/// utility priority (still on performance cores, unlike background), with
/// its disk reads yielding to other apps. The TUI thread keeps its priority.
fn pace(fast: bool) {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let threads = if fast { cores } else { (cores * 4 / 5).max(2) };
    if !fast {
        unsafe { setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_PROCESS, IOPOL_UTILITY) };
    }
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .start_handler(move |_| {
            if !fast {
                unsafe { pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0) };
            }
        })
        .build_global();
}

fn background(fast: bool) {
    if !fast {
        unsafe { pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0) };
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| ["goto", "suggest", "sizes", "find", "suggestions", "clear", "instances"].contains(&a.as_str())) {
        std::process::exit(client(&args));
    }
    if let Some(cmd @ ("marked" | "delete-marked")) = args.first().map(String::as_str) {
        let yes = args[1..].iter().any(|a| a == "--yes" || a == "-y");
        std::process::exit(marked_cli(cmd == "delete-marked", yes));
    }
    let mut one_fs = true;
    let mut use_cache = true;
    let mut fast = false;
    let mut print_n = None;
    let mut mcp = false;
    let mut mcp_lan = false;
    let mut mcp_port = mcp::DEFAULT_PORT;
    let mut bytes = false;
    let mut dir = None;
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cross-fs" => one_fs = false,
            "--no-cache" => use_cache = false,
            "--fast" => fast = true,
            "--mcp" => mcp = true,
            "--mcp-lan" => (mcp, mcp_lan) = (true, true),
            "--mcp-port" => match args.next().and_then(|n| n.parse().ok()) {
                Some(p) => (mcp, mcp_port) = (true, p),
                None => {
                    eprintln!("{BIN}: --mcp-port needs a port number");
                    std::process::exit(2);
                }
            },
            "-x" => one_fs = true,
            "--bytes" | "-b" => bytes = true,
            "--print" | "-p" => {
                print_n = Some(args.next_if(|n| n.parse::<usize>().is_ok()).map_or(20, |n| n.parse().unwrap()));
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            _ if a.starts_with('-') => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
            _ => dir = Some(a),
        }
    }
    let root_path = match std::fs::canonicalize(dir.as_deref().unwrap_or(".")) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{BIN}: {e}");
            std::process::exit(1);
        }
    };
    pace(fast);
    if let Some(n) = print_n {
        background(fast);
        print(&root_path, one_fs, use_cache, n, bytes);
        return;
    }
    let mut terminal = ratatui::init();
    let opts = Opts { one_fs, use_cache, fast, mcp: mcp.then_some((mcp_lan, mcp_port)) };
    let result = run(&mut terminal, root_path, opts);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if let Err(e) = result {
        eprintln!("{BIN}: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, private: u64, children: Vec<Node>) -> Node {
        let kind = if children.is_empty() { Kind::File } else { Kind::Dir };
        let private = private + children.iter().map(|c| c.private).sum::<u64>();
        let modified = children.iter().map(|c: &Node| c.modified).max().unwrap_or(private as i64);
        let files = if kind == Kind::File { 1 } else { children.iter().map(|c| c.files).sum() };
        Node { name: name.into(), kind, error: false, alloc: private * 2, private, items: 1, files, created: 1, modified, children }
    }

    fn app() -> App {
        let root = node(
            "/r",
            0,
            vec![
                node("big", 0, vec![node("a.bin", 50, vec![]), node("b.bin", 70, vec![]), node("deep", 0, vec![node("x", 5, vec![])])]),
                node("small.txt", 10, vec![]),
            ],
        );
        let mut app = App {
            root,
            root_path: "/r".into(),
            path: Vec::new(),
            list: ListState::default().with_selected(Some(0)),
            sort: Sort::Size,
            desc: true,
            col: Col::Private,
            one_fs: true,
            map: false,
            tiles: Vec::new(),
            confirm: false,
            help: false,
            message: None,
            free: (0, 0),
            snapshots: Arc::new(Mutex::new(None)),
            mcp: None,
            pairing: None,
            suggestions: Vec::new(),
            sugg_file: None,
            deleted_file: None,
            marked: Vec::new(),
            marked_file: None,
            confirm_marked: false,
            deleting: None,
            refreshing: None,
            page: Page::Tree,
            page_list: ListState::default(),
        };
        app.sort_cur();
        app
    }

    fn settle(app: &mut App) {
        while app.deleting.is_some() || app.refreshing.is_some() {
            app.tick();
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn at<'a>(j: &'a Json, key: &str) -> Option<&'a Json> {
        let Json::Obj(o) = j else { return None };
        o.get(key)
    }

    fn ask(app: &mut App, req: Json) -> Obj {
        let Json::Obj(r) = req else { panic!() };
        let Json::Obj(o) = app.handle(&r) else { panic!() };
        o
    }

    fn selected_path(app: &App) -> PathBuf {
        app.fs_path().join(&*app.cur().children[app.selected()].name)
    }

    #[test]
    fn goto_opens_parent_and_selects() {
        let mut app = app();
        let o = ask(&mut app, obj([("cmd", "goto".into()), ("path", "/r/big/deep/x".into())]));
        assert_eq!(o["ok"], Json::Bool(true));
        assert_eq!(app.fs_path(), PathBuf::from("/r/big/deep"));
        assert_eq!(selected_path(&app), PathBuf::from("/r/big/deep/x"));
        ask(&mut app, obj([("cmd", "goto".into()), ("path", "/r/big/a.bin".into())]));
        assert_eq!(selected_path(&app), PathBuf::from("/r/big/a.bin"));
    }

    #[test]
    fn goto_cancels_a_pending_delete() {
        let mut app = app();
        app.confirm = true;
        ask(&mut app, obj([("cmd", "goto".into()), ("path", "/r/small.txt".into())]));
        assert!(!app.confirm);
    }

    #[test]
    fn unknown_paths_are_errors() {
        let mut app = app();
        for p in ["/r/nope", "/elsewhere"] {
            let o = ask(&mut app, obj([("cmd", "suggest".into()), ("path", p.into()), ("reason", "x".into())]));
            assert_eq!(o["ok"], Json::Bool(false));
        }
        assert!(app.suggestions.is_empty());
    }

    #[test]
    fn suggestions_mark_list_step_and_clear() {
        let mut app = app();
        ask(&mut app, obj([("cmd", "suggest".into()), ("path", "/r/big/b.bin".into()), ("reason", "old build".into())]));
        ask(&mut app, obj([("cmd", "suggest".into()), ("path", "/r/small.txt".into()), ("reason", "dup".into())]));
        ask(&mut app, obj([("cmd", "suggest".into()), ("path", "/r/small.txt".into()), ("reason", "duplicate".into())]));
        let o = ask(&mut app, obj([("cmd", "suggestions".into())]));
        let Json::Arr(list) = &o["suggestions"] else { panic!() };
        assert_eq!(list.len(), 2);
        assert_eq!(app.mark(Path::new("/r/big")), Some('+'));
        assert_eq!(app.mark(Path::new("/r/big/b.bin")), Some('*'));
        assert_eq!(app.mark(Path::new("/r/big/a.bin")), None);

        app.key(KeyCode::Char(']'));
        assert_eq!(selected_path(&app), PathBuf::from("/r/big/b.bin"));
        app.key(KeyCode::Char(']'));
        assert_eq!(selected_path(&app), PathBuf::from("/r/small.txt"));

        let o = ask(&mut app, obj([("cmd", "clear".into()), ("path", "/r/small.txt".into())]));
        assert_eq!(o["removed"], Json::Num(1.0));
        let o = ask(&mut app, obj([("cmd", "clear".into())]));
        assert_eq!(o["removed"], Json::Num(1.0));
        assert!(app.suggestions.is_empty());
    }

    #[test]
    fn collapsed_folders_fill_in_when_asked_about_or_entered() {
        let d = std::env::temp_dir().canonicalize().unwrap().join(format!("apdu-expand-{}", std::process::id()));
        std::fs::create_dir_all(d.join("node_modules/pkg")).unwrap();
        std::fs::write(d.join("node_modules/pkg/i.js"), vec![1u8; 9000]).unwrap();
        let file = d.with_extension("bin");
        let meta = cache::Meta { event_id: 0, uuid: [0; 16], saved: std::time::SystemTime::now() };
        cache::save(&file, &d, true, &meta, &scan::scan(&d, true, &Progress::default())).unwrap();
        let (_, tree) = cache::load(&file, &d, true).unwrap();
        let total = tree.alloc;

        let mut a = app();
        (a.root, a.root_path) = (tree, d.clone());
        assert!(cache::collapsed(&a.root.children[0]));
        let o = ask(&mut a, obj([("cmd", "sizes".into()), ("path", d.join("node_modules/pkg/i.js").to_string_lossy().into_owned().into())]));
        assert_eq!(o["ok"], Json::Bool(true));
        assert_eq!(a.root.alloc, total, "filling in keeps the totals");

        let mut app2 = app();
        let (_, tree) = cache::load(&file, &d, true).unwrap();
        (app2.root, app2.root_path) = (tree, d.clone());
        app2.list.select(Some(0));
        app2.enter();
        assert_eq!(app2.path, vec![0]);
        assert_eq!(app2.cur().children.len(), 1);
        std::fs::remove_dir_all(&d).unwrap();
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn sizes_reports_bytes_and_biggest_children() {
        let mut app = app();
        let o = ask(&mut app, obj([("cmd", "sizes".into()), ("path", "/r/big".into()), ("children", 2u64.into())]));
        assert_eq!(o["private"], Json::Num(125.0));
        assert_eq!(o["allocated"], Json::Num(250.0));
        assert_eq!(o["children_total"], Json::Num(3.0));
        let Json::Arr(kids) = &o["children"] else { panic!() };
        let names: Vec<_> = kids.iter().map(|k| if let Json::Obj(k) = k { k["path"].clone() } else { panic!() }).collect();
        assert_eq!(names, vec![Json::from("/r/big/b.bin"), Json::from("/r/big/a.bin")]);
    }

    #[test]
    fn sorts_by_modified_oldest_first_then_reverses() {
        let mut app = app();
        app.goto(Path::new("/r/big/a.bin")).unwrap();
        let names = |app: &App| app.cur().children.iter().map(|c| c.name.to_string()).collect::<Vec<_>>();
        app.key(KeyCode::Char('m'));
        assert_eq!(names(&app), ["deep", "a.bin", "b.bin"]);
        assert_eq!(selected_path(&app), PathBuf::from("/r/big/a.bin"));
        app.key(KeyCode::Char('m'));
        assert_eq!(names(&app), ["b.bin", "a.bin", "deep"]);
    }

    #[test]
    fn sorts_by_files_inside_most_first_then_reverses_and_shows_the_column() {
        let mut app = app();
        app.root.children.push(node("many", 0, vec![node("1", 1, vec![]), node("2", 1, vec![]), node("3", 1, vec![]), node("4", 1, vec![])]));
        app.root.files = app.root.children.iter().map(|c| c.files).sum();
        let names = |app: &App| app.cur().children.iter().map(|c| c.name.to_string()).collect::<Vec<_>>();
        app.key(KeyCode::Char('f'));
        assert_eq!(names(&app), ["many", "big", "small.txt"]);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 10)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let rows: Vec<String> = (0..10).map(|y| (0..160).map(|x| term.backend().buffer()[(x, y)].symbol().to_string()).collect()).collect();
        let screen = rows.join("\n");
        let header = rows.iter().find(|r| r.contains("created")).unwrap();
        let col = header.find("files").expect("a files column");
        let many = rows.iter().find(|r| r.contains("many/")).unwrap();
        assert_eq!(&many[col..col + 5], "    4", "{screen}");
        let big = rows.iter().find(|r| r.contains("big/")).unwrap();
        assert_eq!(&big[col..col + 5], "    3", "{screen}");
        assert!(screen.contains("files 8") && screen.contains("sort files most first"), "{screen}");
        app.key(KeyCode::Char('f'));
        assert_eq!(names(&app), ["small.txt", "big", "many"]);
        app.message = None;
        term.draw(|f| app.draw(f)).unwrap();
        let screen: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(screen.contains("sort files fewest first"), "{screen}");
        assert!(HELP.contains("f                sort by files"));
    }

    #[test]
    fn counts_files_compactly() {
        assert_eq!([count(0), count(9999), count(12_345), count(123_456), count(1_234_567)], ["0", "9999", "12.3k", "123k", "1.2M"]);
    }

    #[test]
    fn file_counts_follow_scans_rescans_and_deletes() {
        let d = std::env::temp_dir().canonicalize().unwrap().join(format!("apdu-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("a/b/c")).unwrap();
        for f in ["a/1", "a/b/2", "a/b/c/3", "a/b/c/4", "top"] {
            std::fs::write(d.join(f), b"x").unwrap();
        }
        let mut app = app();
        (app.root, app.root_path) = (scan::scan(&d, true, &Progress::default()), d.clone());
        app.sort_cur();
        assert_eq!((app.root.files, app.find(&d.join("a")).unwrap().files, app.find(&d.join("a/b")).unwrap().files), (5, 4, 3));

        std::fs::write(d.join("a/b/c/5"), b"x").unwrap();
        app.goto(&d.join("a/b/c/3")).unwrap();
        app.key(KeyCode::Char('r'));
        settle(&mut app);
        assert_eq!((app.root.files, app.find(&d.join("a/b")).unwrap().files), (6, 4), "a rescan carries the new count up");

        app.goto(&d.join("a/b")).unwrap();
        app.key(KeyCode::Char('d'));
        app.key(KeyCode::Char('y'));
        settle(&mut app);
        assert_eq!((app.root.files, app.find(&d.join("a")).unwrap().files), (2, 1), "a delete takes its files off");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn sizes_reports_times_and_sorts_by_age() {
        let mut app = app();
        let o = ask(&mut app, obj([("cmd", "sizes".into()), ("path", "/r/big".into()), ("sort", "modified".into())]));
        assert_eq!(o["modified"], Json::Num(70.0));
        assert_eq!(o["modified_iso"], Json::from("1970-01-01T00:01:10Z"));
        let Json::Arr(kids) = &o["children"] else { panic!() };
        let Json::Obj(first) = &kids[0] else { panic!() };
        assert_eq!(first["path"], Json::from("/r/big/deep"));
        assert_eq!(first["created"], Json::Num(1.0));
    }

    #[test]
    fn treemap_paints_marked_items_red() {
        let mut app = app();
        app.toggle_mark(Path::new("/r/big/a.bin"));
        app.key(KeyCode::Char('t'));
        let bg_of = |app: &mut App, name: &str| {
            let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            term.draw(|f| app.draw(f)).unwrap();
            let buf = term.backend().buffer().clone();
            let rows: Vec<String> = (0..30).map(|y| (0..100).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect();
            let (y, row) = rows.iter().enumerate().skip(2).find(|(_, r)| r.contains(name)).expect(name);
            let x = row.chars().take(row.find(name).unwrap()).count() as u16;
            buf[(x, y as u16)].bg
        };
        let (r, g, b) = MARKED;
        assert_ne!(bg_of(&mut app, "big/"), Color::Rgb(r, g, b));
        app.key(KeyCode::Enter);
        assert_eq!(bg_of(&mut app, "a.bin"), Color::Rgb(r, g, b));
        assert_ne!(bg_of(&mut app, "b.bin"), Color::Rgb(r, g, b));
    }

    #[test]
    fn marks_toggle_and_count_each_byte_once() {
        let mut app = app();
        app.toggle_mark(Path::new("/r/big/a.bin"));
        app.toggle_mark(Path::new("/r/small.txt"));
        assert_eq!(app.marked_private(), 60);
        assert_eq!(app.del_mark(Path::new("/r/big")).content, "+");

        app.toggle_mark(Path::new("/r/big"));
        assert_eq!(app.marked, vec![PathBuf::from("/r/small.txt"), PathBuf::from("/r/big")], "the folder replaces the mark inside it");
        assert_eq!(app.marked_private(), 135);
        app.toggle_mark(Path::new("/r/big/b.bin"));
        assert_eq!(app.marked.len(), 2, "inside a marked folder is already marked");
        assert_eq!(app.del_mark(Path::new("/r/big/b.bin")).content, "x");

        app.goto(Path::new("/r/small.txt")).unwrap();
        app.key(KeyCode::Char('x'));
        assert_eq!(app.marked, vec![PathBuf::from("/r/big")]);
        let o = ask(&mut app, obj([("cmd", "sizes".into()), ("path", "/r/big/deep".into())]));
        assert_eq!(o["marked_for_deletion"], Json::Bool(true));
    }

    #[test]
    fn delete_marked_asks_then_deletes_from_disk_and_tree() {
        let d = std::env::temp_dir().canonicalize().unwrap().join(format!("apdu-marked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("old/inner")).unwrap();
        std::fs::create_dir_all(d.join("keep")).unwrap();
        std::fs::write(d.join("old/inner/x"), vec![1u8; 40_000]).unwrap();
        std::fs::write(d.join("keep/y"), vec![1u8; 30_000]).unwrap();
        std::fs::write(d.join("f.bin"), vec![1u8; 20_000]).unwrap();
        let mut app = app();
        (app.root, app.root_path) = (scan::scan(&d, true, &Progress::default()), d.clone());
        app.sort_cur();
        app.toggle_mark(&d.join("old"));
        app.toggle_mark(&d.join("f.bin"));
        app.goto(&d.join("old/inner/x")).unwrap();

        app.key(KeyCode::Char('D'));
        assert!(app.confirm_marked);
        app.key(KeyCode::Char('n'));
        assert!(d.join("old").exists() && d.join("f.bin").exists());

        app.key(KeyCode::Char('D'));
        app.key(KeyCode::Char('y'));
        settle(&mut app);
        assert!(!d.join("old").exists() && !d.join("f.bin").exists() && d.join("keep/y").exists());
        assert!(app.marked.is_empty());
        assert_eq!(app.fs_path(), d, "the view leaves the deleted folder");
        let truth = scan::scan(&d, true, &Progress::default());
        assert_eq!((app.root.alloc, app.root.private, app.root.items), (truth.alloc, truth.private, truth.items));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_delete_shows_progress_and_holds_keys_until_it_is_done() {
        let d = std::env::temp_dir().canonicalize().unwrap().join(format!("apdu-progress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("big/sub")).unwrap();
        for i in 0..200 {
            std::fs::write(d.join(format!("big/sub/{i}")), vec![1u8; 5_000]).unwrap();
        }
        std::fs::write(d.join("keep"), b"k").unwrap();
        let mut app = app();
        (app.root, app.root_path) = (scan::scan(&d, true, &Progress::default()), d.clone());
        app.sort_cur();
        app.goto(&d.join("big")).unwrap();
        let total = app.find(&d.join("big")).unwrap().items;

        app.key(KeyCode::Char('d'));
        app.key(KeyCode::Char('y'));
        let progress = app.deleting.as_ref().expect("the delete runs in the background").done.clone();
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let screen: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(screen.contains("deleting") && screen.contains("Deleting big") && screen.contains(&format!("of {total} items")), "{screen}");

        app.key(KeyCode::Char('j'));
        assert!(app.message.as_deref().unwrap().contains("still deleting"));
        assert_eq!(selected_path(&app), d.join("big"), "keys wait for the delete");

        settle(&mut app);
        assert_eq!(progress.items.load(Relaxed), total, "every item is counted");
        assert!(!d.join("big").exists() && d.join("keep").exists());
        assert!(app.message.as_deref().unwrap().starts_with("deleted "), "{:?}", app.message);
        assert!(app.find(&d.join("big")).is_err());
        term.draw(|f| app.draw(f)).unwrap();
        let screen: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(!screen.contains("Deleting big"), "the box closes when it is done");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_rescan_shows_progress_and_holds_keys_until_it_is_done() {
        let d = std::env::temp_dir().canonicalize().unwrap().join(format!("apdu-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub/old"), vec![1u8; 5_000]).unwrap();
        std::fs::write(d.join("keep"), b"k").unwrap();
        let mut app = app();
        (app.root, app.root_path) = (scan::scan(&d, true, &Progress::default()), d.clone());
        app.sort_cur();
        app.goto(&d.join("sub/old")).unwrap();
        for i in 0..50 {
            std::fs::write(d.join(format!("sub/new{i}")), vec![1u8; 5_000]).unwrap();
        }

        app.key(KeyCode::Char('r'));
        assert!(app.refreshing.is_some(), "the rescan runs in the background");
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let screen: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(screen.contains("refreshing") && screen.contains("Rescanning sub") && screen.contains("of about 2 items"), "{screen}");

        app.key(KeyCode::Char('j'));
        assert!(app.message.as_deref().unwrap().contains("still rescanning"));
        assert_eq!(selected_path(&app), d.join("sub/old"), "keys wait for the rescan");

        settle(&mut app);
        assert!(app.message.as_deref().unwrap().starts_with(" rescanned in"), "{:?}", app.message);
        assert_eq!(app.cur().children.len(), 51);
        assert_eq!(app.root.items, 54, "the new items count up to the root");
        assert_eq!(selected_path(&app), d.join("sub/old"), "the selection stays put");
        term.draw(|f| app.draw(f)).unwrap();
        let screen: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(!screen.contains("Rescanning"), "the box closes when it is done");
        assert!(!app.key(KeyCode::Char('q')));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn q_quits_while_a_rescan_runs() {
        let mut app = app();
        app.root_path = std::env::temp_dir().canonicalize().unwrap();
        app.rescan();
        assert!(!app.key(KeyCode::Char('q')));
        settle(&mut app);
    }

    #[test]
    fn deleted_suggestions_are_recorded_so_agents_know_they_are_gone() {
        let d = std::env::temp_dir().canonicalize().unwrap().join(format!("apdu-deleted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let t = d.join("tree");
        std::fs::create_dir_all(t.join("old/inner")).unwrap();
        std::fs::write(t.join("old/inner/x"), vec![1u8; 40_000]).unwrap();
        std::fs::write(t.join("keep"), vec![1u8; 30_000]).unwrap();
        let mut app = app();
        (app.root, app.root_path) = (scan::scan(&t, true, &Progress::default()), t.clone());
        (app.sugg_file, app.deleted_file) = (Some(d.join("suggestions.tsv")), Some(d.join("deleted.tsv")));
        app.sort_cur();
        let path = |p: &Path| Json::from(p.to_string_lossy().into_owned());
        ask(&mut app, obj([("cmd", "suggest".into()), ("path", path(&t.join("old/inner"))), ("reason", "stale build".into())]));
        ask(&mut app, obj([("cmd", "suggest".into()), ("path", path(&t.join("keep"))), ("reason", "dup".into())]));

        app.toggle_mark(&t.join("old"));
        app.key(KeyCode::Char('D'));
        app.key(KeyCode::Char('y'));
        settle(&mut app);
        assert!(!t.join("old").exists());

        let o = ask(&mut app, obj([("cmd", "suggestions".into())]));
        let Json::Arr(list) = &o["suggestions"] else { panic!() };
        assert_eq!(list.len(), 1);
        assert_eq!(at(&list[0], "path"), Some(&path(&t.join("keep"))));
        let Json::Arr(gone) = &o["deleted"] else { panic!() };
        assert_eq!(gone.len(), 1);
        assert_eq!(at(&gone[0], "path"), Some(&path(&t.join("old/inner"))));
        assert_eq!(at(&gone[0], "reason"), Some(&Json::from("stale build")));
        assert!(matches!(at(&gone[0], "deleted_iso"), Some(Json::Str(_))));

        let o = ask(&mut app, obj([("cmd", "suggest".into()), ("path", path(&t.join("old/inner/x"))), ("reason", "again".into())]));
        let err = o["error"].str().unwrap();
        assert!(err.contains("was deleted in apdu") && err.contains("old/inner"), "{err}");

        app.root = scan::scan(&t, true, &Progress::default());
        std::fs::create_dir_all(t.join("old/inner")).unwrap();
        app.root = scan::scan(&t, true, &Progress::default());
        let o = ask(&mut app, obj([("cmd", "suggest".into()), ("path", path(&t.join("old/inner"))), ("reason", "back again".into())]));
        assert_eq!(o["ok"], Json::Bool(true));
        let o = ask(&mut app, obj([("cmd", "suggestions".into())]));
        assert_eq!(o["deleted"], Json::Arr(vec![]), "a path that exists again is no longer listed as deleted");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn find_matches_names_across_the_whole_scan() {
        let mut app = app();
        let o = ask(&mut app, obj([("cmd", "find".into()), ("name", "*.BIN".into())]));
        assert_eq!(o["total"], Json::Num(2.0));
        assert_eq!(o["private_sum"], Json::Num(120.0));
        let Json::Arr(m) = &o["matches"] else { panic!() };
        let paths: Vec<_> = m.iter().map(|m| at(m, "path").cloned().unwrap()).collect();
        assert_eq!(paths, vec![Json::from("/r/big/b.bin"), Json::from("/r/big/a.bin")], "biggest first");

        let o = ask(&mut app, obj([("cmd", "find".into()), ("name", "x".into())]));
        let Json::Arr(m) = &o["matches"] else { panic!() };
        assert_eq!(at(&m[0], "path"), Some(&Json::from("/r/big/deep/x")), "finds nested matches");

        let o = ask(&mut app, obj([("cmd", "find".into()), ("name", "b*".into())]));
        let Json::Arr(m) = &o["matches"] else { panic!() };
        assert_eq!(m.len(), 1, "does not look inside a match, so b.bin inside big is not counted twice");

        let o = ask(&mut app, obj([("cmd", "find".into()), ("name", "*.bin".into()), ("limit", 1.into())]));
        let Json::Arr(m) = &o["matches"] else { panic!() };
        assert_eq!((m.len(), &o["total"]), (1, &Json::Num(2.0)));

        let o = ask(&mut app, obj([("cmd", "find".into()), ("name", "*.bin".into()), ("path", "/r/small.txt".into())]));
        assert_eq!(o["total"], Json::Num(0.0));
        assert!(glob("a*b*c", "axxbyyc") && !glob("a*b*c", "axxbyy") && glob("*", ""));
    }

    #[test]
    fn suggestions_page_marks_and_dismisses() {
        let mut app = app();
        ask(&mut app, obj([("cmd", "suggest".into()), ("path", "/r/big/b.bin".into()), ("reason", "old build".into())]));
        ask(&mut app, obj([("cmd", "suggest".into()), ("path", "/r/big".into()), ("reason", "whole folder".into())]));
        app.key(KeyCode::Char('S'));
        assert!(app.page == Page::Suggestions);
        app.key(KeyCode::Char('x'));
        assert_eq!(app.marked, vec![PathBuf::from("/r/big/b.bin")]);
        app.key(KeyCode::Char('x'));
        assert!(app.marked.is_empty(), "x again unmarks");
        app.key(KeyCode::Char('x'));
        app.key(KeyCode::Char('-'));
        assert_eq!(app.suggestions.len(), 1);
        assert_eq!(app.marked, vec![PathBuf::from("/r/big/b.bin")], "dismissing a suggestion keeps its mark");
        app.key(KeyCode::Char('M'));
        assert!(app.page == Page::Marked);
        app.key(KeyCode::Enter);
        assert!(app.page == Page::Tree);
        assert_eq!(selected_path(&app), PathBuf::from("/r/big/b.bin"));
    }
}
