use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::HashSet;
use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_DEVID: u32 = 0x0000_0002;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_CRTIME: u32 = 0x0000_0200;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
const FSOPT_NOFOLLOW: u64 = 0x0000_0001;
const FSOPT_ATTR_CMN_EXTENDED: u64 = 0x0000_0020;
const VDIR: u32 = 2;
const BUF_LEN: usize = 256 * 1024;

#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

unsafe extern "C" {
    fn getattrlistbulk(fd: i32, attrs: *mut AttrList, buf: *mut u8, size: usize, options: u64) -> i32;
    fn getattrlist(path: *const libc::c_char, attrs: *mut AttrList, buf: *mut u8, size: usize, options: u32) -> i32;
}

fn attr_list(private: bool) -> AttrList {
    AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS
            | ATTR_CMN_ERROR
            | ATTR_CMN_NAME
            | ATTR_CMN_DEVID
            | ATTR_CMN_OBJTYPE
            | ATTR_CMN_CRTIME
            | ATTR_CMN_MODTIME
            | ATTR_CMN_FILEID,
        volattr: 0,
        dirattr: 0,
        fileattr: ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE,
        forkattr: if private { ATTR_CMNEXT_PRIVATESIZE } else { 0 },
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    OtherFs,
    /// A folder already scanned at another path, such as /Users and its
    /// firmlink target /System/Volumes/Data/Users: shown, counted as zero.
    Duplicate,
}

pub struct Node {
    pub name: Box<str>,
    pub kind: Kind,
    pub error: bool,
    pub alloc: u64,
    pub private: u64,
    pub items: u64,
    /// Files on or under this node, not counting folders.
    pub files: u64,
    pub created: i64,
    pub modified: i64,
    pub children: Vec<Node>,
}

impl Node {
    pub(crate) fn leaf(name: Box<str>, kind: Kind, alloc: u64, private: u64) -> Node {
        Node { name, kind, error: false, alloc, private, items: 1, files: (kind == Kind::File) as u64, created: 0, modified: 0, children: Vec::new() }
    }

    pub(crate) fn with_times(mut self, (created, modified): (i64, i64)) -> Node {
        self.created = created;
        self.modified = modified;
        self
    }
}

#[derive(Default)]
pub struct Progress {
    pub items: AtomicU64,
    pub alloc: AtomicU64,
    pub errors: AtomicU64,
    pub dirs: AtomicU64,
    pub current: Mutex<String>,
    pub stage: Mutex<&'static str>,
}

struct Ctx<'a> {
    dev: i32,
    one_fs: bool,
    private: bool,
    seen: Mutex<HashSet<u64>>,
    dirs: Mutex<HashSet<(i32, u64)>>,
    progress: &'a Progress,
}

struct Entry {
    name: Box<str>,
    dev: i32,
    fileid: u64,
    is_dir: bool,
    alloc: u64,
    private: u64,
    times: (i64, i64),
}

thread_local! {
    static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0; BUF_LEN]);
}

fn rd<T: Copy>(buf: &[u8], at: usize) -> T {
    unsafe { std::ptr::read_unaligned(buf.as_ptr().add(at) as *const T) }
}

fn read_entries(dir: &Path, ctx: &Ctx) -> Option<Vec<Entry>> {
    let c = CString::new(dir.as_os_str().as_bytes()).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut ok = true;
    BUF.with_borrow_mut(|buf| {
        let mut attrs = attr_list(ctx.private);
        loop {
            let n = unsafe { getattrlistbulk(fd, &mut attrs, buf.as_mut_ptr(), buf.len(), FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED) };
            if n <= 0 {
                ok = n == 0;
                break;
            }
            let mut base = 0usize;
            for _ in 0..n {
                let len: u32 = rd(buf, base);
                if let Some(e) = parse_entry(&buf[base..base + len as usize], ctx) {
                    out.push(e);
                }
                base += len as usize;
            }
        }
    });
    unsafe { libc::close(fd) };
    if !ok && out.is_empty() { None } else { Some(out) }
}

fn parse_entry(b: &[u8], ctx: &Ctx) -> Option<Entry> {
    let common: u32 = rd(b, 4);
    let file: u32 = rd(b, 4 + 12);
    let fork: u32 = rd(b, 4 + 16);
    let mut p = 24;
    if common & ATTR_CMN_ERROR != 0 {
        let err: u32 = rd(b, p);
        p += 4;
        if err != 0 {
            ctx.progress.errors.fetch_add(1, Relaxed);
            return None;
        }
    }
    let mut name: Box<str> = "".into();
    if common & ATTR_CMN_NAME != 0 {
        let off: i32 = rd(b, p);
        let len: u32 = rd(b, p + 4);
        let start = (p as i64 + off as i64) as usize;
        let raw = &b[start..start + len as usize];
        name = CStr::from_bytes_until_nul(raw).map(|s| s.to_string_lossy().into()).unwrap_or_default();
        p += 8;
    }
    let mut dev = ctx.dev;
    if common & ATTR_CMN_DEVID != 0 {
        dev = rd(b, p);
        p += 4;
    }
    let mut objtype = 0u32;
    if common & ATTR_CMN_OBJTYPE != 0 {
        objtype = rd(b, p);
        p += 4;
    }
    let mut times = (0i64, 0i64);
    if common & ATTR_CMN_CRTIME != 0 {
        times.0 = rd(b, p);
        p += 16;
    }
    if common & ATTR_CMN_MODTIME != 0 {
        times.1 = rd(b, p);
        p += 16;
    }
    let mut fileid = 0u64;
    if common & ATTR_CMN_FILEID != 0 {
        fileid = rd(b, p);
        p += 8;
    }
    if objtype == VDIR {
        return Some(Entry { name, dev, fileid, is_dir: true, alloc: 0, private: 0, times });
    }
    let mut nlink = 1u32;
    if file & ATTR_FILE_LINKCOUNT != 0 {
        nlink = rd(b, p);
        p += 4;
    }
    let mut alloc = 0u64;
    if file & ATTR_FILE_ALLOCSIZE != 0 {
        alloc = rd::<i64>(b, p).max(0) as u64;
        p += 8;
    }
    let private = if fork & ATTR_CMNEXT_PRIVATESIZE != 0 { rd::<i64>(b, p).max(0) as u64 } else { alloc };
    if nlink > 1 && !ctx.seen.lock().unwrap().insert(fileid) {
        return Some(Entry { name, dev, fileid, is_dir: false, alloc: 0, private: 0, times });
    }
    Some(Entry { name, dev, fileid, is_dir: false, alloc, private, times })
}

fn scan_dir(name: Box<str>, path: PathBuf, times: (i64, i64), ctx: &Ctx) -> Node {
    let mut node = Node::leaf(name, Kind::Dir, 0, 0).with_times(times);
    let Some(entries) = read_entries(&path, ctx) else {
        node.error = true;
        ctx.progress.errors.fetch_add(1, Relaxed);
        return node;
    };
    if ctx.progress.dirs.fetch_add(1, Relaxed).is_multiple_of(256) {
        *ctx.progress.current.lock().unwrap() = path.to_string_lossy().into_owned();
    }
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut subdirs = Vec::new();
    for e in entries {
        if e.is_dir {
            if ctx.one_fs && e.dev != ctx.dev {
                node.children.push(Node::leaf(e.name, Kind::OtherFs, 0, 0).with_times(e.times));
            } else if !ctx.dirs.lock().unwrap().insert((e.dev, e.fileid)) {
                node.children.push(Node::leaf(e.name, Kind::Duplicate, 0, 0).with_times(e.times));
            } else {
                subdirs.push((e.name, e.times));
            }
        } else {
            files += 1;
            bytes += e.alloc;
            node.children.push(Node::leaf(e.name, Kind::File, e.alloc, e.private).with_times(e.times));
        }
    }
    ctx.progress.items.fetch_add(files + 1, Relaxed);
    ctx.progress.alloc.fetch_add(bytes, Relaxed);
    let dirs: Vec<Node> = subdirs
        .into_par_iter()
        .map(|(n, t)| {
            let p = path.join(&*n);
            scan_dir(n, p, t, ctx)
        })
        .collect();
    node.children.extend(dirs);
    node.children.shrink_to_fit();
    for c in &node.children {
        node.alloc += c.alloc;
        node.private += c.private;
        node.items += c.items;
        node.files += c.files;
        node.modified = node.modified.max(c.modified);
    }
    node
}

fn lstat(path: &Path) -> Option<libc::stat> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    (unsafe { libc::lstat(c.as_ptr(), &mut st) } == 0).then_some(st)
}

fn single_file(path: &Path) -> Node {
    let name: Box<str> = path.to_string_lossy().into();
    let mut attrs = attr_list(true);
    attrs.commonattr = ATTR_CMN_RETURNED_ATTRS | ATTR_CMN_CRTIME | ATTR_CMN_MODTIME;
    attrs.fileattr = ATTR_FILE_ALLOCSIZE;
    let mut buf = [0u8; 96];
    let c = CString::new(path.as_os_str().as_bytes()).unwrap();
    let rc = unsafe { getattrlist(c.as_ptr(), &mut attrs, buf.as_mut_ptr(), buf.len(), (FSOPT_NOFOLLOW | FSOPT_ATTR_CMN_EXTENDED) as u32) };
    if rc != 0 {
        let mut n = Node::leaf(name, Kind::File, 0, 0);
        n.error = true;
        return n;
    }
    let fork: u32 = rd(&buf, 4 + 16);
    let alloc = rd::<i64>(&buf, 56).max(0) as u64;
    let private = if fork & ATTR_CMNEXT_PRIVATESIZE != 0 { rd::<i64>(&buf, 64).max(0) as u64 } else { alloc };
    Node::leaf(name, Kind::File, alloc, private).with_times((rd(&buf, 24), rd(&buf, 40)))
}

pub fn scan(root: &Path, one_fs: bool, progress: &Progress) -> Node {
    scan_on(root, lstat(root).map_or(0, |s| s.st_dev), one_fs, progress)
}

/// Scans `root`, treating `dev` as the filesystem to stay on: a folder inside an
/// earlier scan is rescanned against that scan's filesystem, not its own.
pub fn scan_on(root: &Path, dev: i32, one_fs: bool, progress: &Progress) -> Node {
    if !root.is_dir() || root.is_symlink() {
        return single_file(root);
    }
    let st = lstat(root);
    let dirs = st.map(|s| HashSet::from([(s.st_dev, s.st_ino)])).unwrap_or_default();
    let ctx = Ctx { dev, one_fs, private: true, seen: Mutex::new(HashSet::new()), dirs: Mutex::new(dirs), progress };
    let times = st.map_or((0, 0), |s| (s.st_birthtime, s.st_mtime));
    scan_dir(root.to_string_lossy().into(), root.to_path_buf(), times, &ctx)
}

/// One folder's direct entries: files with their allocated size, subfolders as
/// empty Dir nodes carrying only their own times. A file's private size is left
/// equal to its allocated size, since asking APFS for it costs far more than the
/// listing; `private_size` fetches it. None if the folder can't be read.
pub fn list(dir: &Path, dev: i32, one_fs: bool) -> Option<Vec<Node>> {
    let progress = Progress::default();
    let ctx = Ctx { dev, one_fs, private: false, seen: Mutex::new(HashSet::new()), dirs: Mutex::new(HashSet::new()), progress: &progress };
    let entries = read_entries(dir, &ctx)?;
    Some(
        entries
            .into_iter()
            .map(|e| {
                let kind = match (e.is_dir, one_fs && e.dev != dev) {
                    (false, _) => Kind::File,
                    (true, true) => Kind::OtherFs,
                    (true, false) => Kind::Dir,
                };
                Node::leaf(e.name, kind, e.alloc, e.private).with_times(e.times)
            })
            .collect(),
    )
}

/// The bytes only this file holds, not shared with a clone; None if unreadable.
pub fn private_size(path: &Path) -> Option<u64> {
    let n = single_file(path);
    (!n.error).then_some(n.private)
}

pub fn dev_of(path: &Path) -> i32 {
    lstat(path).map_or(0, |s| s.st_dev)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_mtime(p: &Path, secs: i64) {
        let c = CString::new(p.as_os_str().as_bytes()).unwrap();
        let t = libc::timeval { tv_sec: secs, tv_usec: 0 };
        assert_eq!(unsafe { libc::utimes(c.as_ptr(), [t, t].as_ptr()) }, 0);
    }

    #[test]
    fn folders_take_the_newest_modification_inside() {
        let root = std::env::temp_dir().join(format!("apdu-scan-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("old.txt"), "a").unwrap();
        std::fs::write(root.join("sub/new.txt"), "b").unwrap();
        set_mtime(&root.join("old.txt"), 1_000_000_000);
        set_mtime(&root.join("sub/new.txt"), 4_000_000_000);
        set_mtime(&root.join("sub"), 1_200_000_000);
        set_mtime(&root, 1_100_000_000);

        let node = scan(&root, true, &Progress::default());
        let child = |n: &Node, name: &str| n.children.iter().find(|c| &*c.name == name).unwrap().modified;
        assert_eq!(node.modified, 4_000_000_000);
        assert_eq!(child(&node, "old.txt"), 1_000_000_000);
        assert_eq!(child(&node, "sub"), 4_000_000_000);
        let sub = node.children.iter().find(|c| &*c.name == "sub").unwrap();
        assert!((1_700_000_000..4_000_000_000).contains(&sub.children[0].created), "a future mtime leaves the birth time alone");
        let file = scan(&root.join("old.txt"), true, &Progress::default());
        assert_eq!(file.modified, 1_000_000_000);
        assert_eq!(file.created, 1_000_000_000, "an mtime before the birth time pulls it back");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sibling_folders_are_not_mistaken_for_duplicates() {
        let root = std::env::temp_dir().join(format!("apdu-dup-test-{}", std::process::id()));
        for d in ["a", "b", "a/c", "b/c"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("b/c/f.txt"), "x").unwrap();
        let node = scan(&root, true, &Progress::default());
        fn walk(n: &Node) -> usize {
            (n.kind == Kind::Duplicate) as usize + n.children.iter().map(walk).sum::<usize>()
        }
        assert_eq!(walk(&node), 0);
        assert!(node.alloc > 0, "the file under b/c is counted");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
