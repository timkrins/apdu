//! The last scan, saved so the next start only rescans what changed.
//!
//! Layout: a plain header (magic, root, one_fs, FSEvents id and UUID), then the
//! tree pre-order as zstd-compressed varints. Folders named in COLLAPSE are kept
//! as totals only; their contents are scanned again when someone looks inside.

use crate::fsevents::Change;
use crate::scan::{self, Kind, Node, Progress};
use rayon::prelude::*;
use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;

const MAGIC: &[u8; 8] = b"APDUC\0\0\x02";
pub const COLLAPSE: [&str; 2] = [".git", "node_modules"];

pub struct Meta {
    pub event_id: u64,
    pub uuid: [u8; 16],
    pub saved: std::time::SystemTime,
}

/// A folder kept as totals only: its children were left out of the cache.
pub fn collapsed(n: &Node) -> bool {
    n.kind == Kind::Dir && n.children.is_empty() && n.items > 1
}

pub fn file(root: &Path, one_fs: bool) -> Option<PathBuf> {
    if unsafe { libc::geteuid() } == 0 {
        return None;
    }
    let home = std::env::var_os("HOME")?;
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in root.as_os_str().as_encoded_bytes().iter().chain([one_fs as u8].iter()) {
        h = (h ^ *b as u64).wrapping_mul(0x100_0000_01b3);
    }
    Some(PathBuf::from(home).join(format!("Library/Caches/apdu/{h:016x}.bin")))
}

fn put(w: &mut impl Write, mut v: u64) -> io::Result<()> {
    let mut buf = [0u8; 10];
    let mut i = 0;
    while v >= 0x80 {
        buf[i] = v as u8 | 0x80;
        v >>= 7;
        i += 1;
    }
    buf[i] = v as u8;
    w.write_all(&buf[..=i])
}

fn get(r: &mut impl Read) -> io::Result<u64> {
    let (mut v, mut shift) = (0u64, 0);
    loop {
        let mut b = [0u8];
        r.read_exact(&mut b)?;
        v |= ((b[0] & 0x7f) as u64) << shift;
        if b[0] < 0x80 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            return Err(io::Error::other("bad varint"));
        }
    }
}

fn put_i(w: &mut impl Write, v: i64) -> io::Result<()> {
    put(w, ((v << 1) ^ (v >> 63)) as u64)
}

fn get_i(r: &mut impl Read) -> io::Result<i64> {
    let v = get(r)?;
    Ok((v >> 1) as i64 ^ -((v & 1) as i64))
}

fn put_bytes(w: &mut impl Write, b: &[u8]) -> io::Result<()> {
    put(w, b.len() as u64)?;
    w.write_all(b)
}

fn get_bytes(r: &mut impl Read) -> io::Result<Vec<u8>> {
    let n = get(r)? as usize;
    if n > 1 << 20 {
        return Err(io::Error::other("bad length"));
    }
    let mut b = vec![0; n];
    r.read_exact(&mut b)?;
    Ok(b)
}

fn kind_code(k: Kind) -> u8 {
    match k {
        Kind::File => 0,
        Kind::Dir => 1,
        Kind::OtherFs => 2,
        Kind::Duplicate => 3,
    }
}

fn write_node(w: &mut impl Write, n: &Node, collapse: bool) -> io::Result<()> {
    w.write_all(&[kind_code(n.kind) | (n.error as u8) << 4])?;
    put_bytes(w, n.name.as_bytes())?;
    put(w, n.alloc)?;
    put(w, n.private)?;
    put(w, n.items)?;
    put(w, n.files)?;
    put_i(w, n.created)?;
    put_i(w, n.modified)?;
    if n.kind == Kind::Dir {
        let children: &[Node] = if collapse && COLLAPSE.contains(&&*n.name) { &[] } else { &n.children };
        put(w, children.len() as u64)?;
        for c in children {
            write_node(w, c, collapse)?;
        }
    }
    Ok(())
}

fn read_node(r: &mut impl Read) -> io::Result<Node> {
    let mut tag = [0u8];
    r.read_exact(&mut tag)?;
    let kind = match tag[0] & 0x0f {
        0 => Kind::File,
        1 => Kind::Dir,
        2 => Kind::OtherFs,
        3 => Kind::Duplicate,
        _ => return Err(io::Error::other("bad kind")),
    };
    let name: Box<str> = String::from_utf8_lossy(&get_bytes(r)?).into();
    let mut n = Node::leaf(name, kind, get(r)?, get(r)?);
    n.error = tag[0] & 0x10 != 0;
    n.items = get(r)?;
    n.files = get(r)?;
    n.created = get_i(r)?;
    n.modified = get_i(r)?;
    if kind == Kind::Dir {
        let count = get(r)? as usize;
        n.children = Vec::with_capacity(count.min(1 << 16));
        for _ in 0..count {
            n.children.push(read_node(r)?);
        }
    }
    Ok(n)
}

pub fn save(path: &Path, root: &Path, one_fs: bool, meta: &Meta, tree: &Node) -> io::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let result = (|| {
        let mut w = BufWriter::new(std::fs::File::create(&tmp)?);
        w.write_all(MAGIC)?;
        put_bytes(&mut w, root.as_os_str().as_encoded_bytes())?;
        w.write_all(&[one_fs as u8])?;
        w.write_all(&meta.event_id.to_le_bytes())?;
        w.write_all(&meta.uuid)?;
        let secs = meta.saved.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        w.write_all(&secs.to_le_bytes())?;
        let mut z = zstd::Encoder::new(w, 3)?;
        z.multithread(4)?;
        write_node(&mut z, tree, true)?;
        z.finish()?.into_inner().map_err(|e| e.into_error())?.sync_all()
    })();
    match result {
        Ok(()) => std::fs::rename(&tmp, path),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn read_header(r: &mut impl Read, root: &Path, one_fs: bool) -> io::Result<Meta> {
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC || get_bytes(r)? != root.as_os_str().as_encoded_bytes() {
        return Err(io::Error::other("different cache"));
    }
    let mut b = [0u8; 1 + 8 + 16 + 8];
    r.read_exact(&mut b)?;
    if b[0] != one_fs as u8 {
        return Err(io::Error::other("different cache"));
    }
    Ok(Meta {
        event_id: u64::from_le_bytes(b[1..9].try_into().unwrap()),
        uuid: b[9..25].try_into().unwrap(),
        saved: std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::from_le_bytes(b[25..33].try_into().unwrap())),
    })
}

pub fn load(path: &Path, root: &Path, one_fs: bool) -> io::Result<(Meta, Node)> {
    let mut r = BufReader::new(std::fs::File::open(path)?);
    let meta = read_header(&mut r, root, one_fs)?;
    let mut z = BufReader::with_capacity(1 << 20, zstd::Decoder::new(r)?);
    let tree = read_node(&mut z)?;
    Ok((meta, tree))
}

/// The node at `path`, or the collapsed folder that holds it; `true` with a
/// collapsed folder, whose total can only be redone by scanning all of it.
fn locate<'a>(tree: &'a mut Node, root: &Path, path: &Path) -> Option<(&'a mut Node, PathBuf, bool)> {
    let rel = path.strip_prefix(root).ok()?;
    let mut n = tree;
    let mut at = root.to_path_buf();
    for c in rel.components() {
        let Component::Normal(name) = c else { return None };
        if collapsed(n) {
            return Some((n, at, true));
        }
        let name = name.to_str()?;
        n = n.children.iter_mut().find(|c| &*c.name == name && c.kind == Kind::Dir)?;
        at.push(name);
    }
    let whole = collapsed(n);
    Some((n, at, whole))
}

/// Brings a loaded tree up to date with the folders FSEvents says changed.
/// A changed folder is listed again; its subfolders keep their old totals
/// unless they are new or were replaced (a different birth time), since any
/// change inside one is reported as its own event. Its files keep their saved
/// private size while their size and times match, since asking APFS for it
/// is most of the cost of relisting a folder of many files.
pub fn refresh(tree: &mut Node, root: &Path, mut changes: Vec<Change>, one_fs: bool, progress: &Progress) -> usize {
    let dev = scan::dev_of(root);
    changes.sort_by(|a, b| a.path.components().count().cmp(&b.path.components().count()).then_with(|| a.path.cmp(&b.path)));
    changes.dedup_by(|later, kept| {
        later.path == kept.path && {
            kept.recursive |= later.recursive;
            true
        }
    });
    let mut fresh: Vec<PathBuf> = Vec::new();
    let mut done = 0;
    for c in changes {
        let c = Change { path: on_data_volume(root, c.path), ..c };
        if fresh.iter().any(|f| c.path.starts_with(f)) {
            continue;
        }
        let Some((node, at, whole)) = locate(tree, root, &c.path) else { continue };
        *progress.current.lock().unwrap() = at.to_string_lossy().into_owned();
        done += 1;
        if whole || c.recursive {
            let mut n = scan::scan_on(&at, dev, one_fs, progress);
            n.name = std::mem::take(&mut node.name);
            *node = n;
            fresh.push(at);
            continue;
        }
        let Some(listing) = scan::list(&at, dev, one_fs) else { continue };
        progress.items.fetch_add(listing.len() as u64, Relaxed);
        let mut old: HashMap<Box<str>, Node> = std::mem::take(&mut node.children).into_iter().map(|c| (c.name.clone(), c)).collect();
        let mut to_size = Vec::new();
        for e in listing {
            let child = match (e.kind, old.remove(&e.name)) {
                (Kind::File, Some(o)) if o.kind == Kind::File && (o.alloc, o.created, o.modified) == (e.alloc, e.created, e.modified) => {
                    Node { private: o.private, ..e }
                }
                (Kind::File, _) if e.alloc > 0 => {
                    to_size.push(node.children.len());
                    e
                }
                (Kind::Dir, Some(o)) if matches!(o.kind, Kind::Dir | Kind::Duplicate) && o.created == e.created => o,
                (Kind::Dir, _) => {
                    let p = at.join(&*e.name);
                    let mut n = scan::scan_on(&p, dev, one_fs, progress);
                    n.name = e.name;
                    n.created = e.created;
                    fresh.push(p);
                    n
                }
                _ => e,
            };
            node.children.push(child);
        }
        let sizes: Vec<Option<u64>> = to_size.par_iter().map(|&i| scan::private_size(&at.join(&*node.children[i].name))).collect();
        for (i, size) in to_size.into_iter().zip(sizes) {
            node.children[i].private = size.unwrap_or(node.children[i].alloc);
        }
        if let Ok(m) = std::fs::symlink_metadata(&at) {
            use std::os::unix::fs::MetadataExt;
            node.modified = m.mtime();
        }
    }
    retotal(tree);
    done
}

/// FSEvents names a file on the data volume by its firmlinked path (/Users/..),
/// even to a stream watching /System/Volumes/Data.
fn on_data_volume(root: &Path, path: PathBuf) -> PathBuf {
    let data = Path::new("/System/Volumes/Data");
    if path.starts_with(root) || !root.starts_with(data) {
        return path;
    }
    data.join(path.strip_prefix("/").unwrap_or(&path))
}

fn retotal(n: &mut Node) {
    if n.kind != Kind::Dir || n.children.is_empty() {
        return;
    }
    let (mut alloc, mut private, mut items, mut files, mut modified) = (0, 0, 1, 0, n.modified);
    for c in &mut n.children {
        retotal(c);
        alloc += c.alloc;
        private += c.private;
        items += c.items;
        files += c.files;
        modified = modified.max(c.modified);
    }
    (n.alloc, n.private, n.items, n.files, n.modified) = (alloc, private, items, files, modified);
}

pub fn suggestions_file() -> Option<PathBuf> {
    file(Path::new("/"), true).map(|f| f.with_file_name("suggestions.tsv"))
}

pub fn marked_file() -> Option<PathBuf> {
    file(Path::new("/"), true).map(|f| f.with_file_name("marked.tsv"))
}

pub fn deleted_file() -> Option<PathBuf> {
    file(Path::new("/"), true).map(|f| f.with_file_name("deleted.tsv"))
}

/// Every saved path under `root`, with its text, whether or not it still exists.
pub fn load_all(file: &Path, root: &Path) -> Vec<(PathBuf, String)> {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    text.lines().filter_map(|l| l.split_once('\t')).map(|(p, r)| (PathBuf::from(p), r.to_string())).filter(|(p, _)| p.starts_with(root)).collect()
}

/// Copies the saved suggestions on or under `gone` into the deleted record,
/// as "<unix secs> <reason>", so agents can tell they were deleted.
pub fn record_deleted(suggestions: &Path, deleted: &Path, gone: &[PathBuf], at: i64) -> io::Result<()> {
    let new: Vec<(PathBuf, String)> = load_all(suggestions, Path::new("/")).into_iter().filter(|(p, _)| gone.iter().any(|g| p.starts_with(g))).collect();
    if new.is_empty() {
        return Ok(());
    }
    let mut all: Vec<(PathBuf, String)> = load_all(deleted, Path::new("/")).into_iter().filter(|(p, _)| !new.iter().any(|(n, _)| n == p)).collect();
    all.extend(new.into_iter().map(|(p, r)| (p, format!("{at} {r}"))));
    save_paths(deleted, Path::new("/"), all.iter().map(|(p, r)| (p.as_path(), r.as_str())))
}

/// The deleted suggestions under `root`: path, reason and when.
pub fn load_deleted(file: &Path, root: &Path) -> Vec<(PathBuf, String, i64)> {
    load_all(file, root)
        .into_iter()
        .map(|(p, r)| {
            let (at, reason) = r.split_once(' ').unwrap_or((&r, ""));
            (p.clone(), reason.to_string(), at.parse().unwrap_or(0))
        })
        .collect()
}

/// Drops `path` from the deleted record, for when it exists again.
pub fn forget_deleted(file: &Path, path: &Path) -> io::Result<()> {
    let all = load_all(file, Path::new("/"));
    if !all.iter().any(|(p, _)| p == path) {
        return Ok(());
    }
    save_paths(file, Path::new("/"), all.iter().filter(|(p, _)| p != path).map(|(p, r)| (p.as_path(), r.as_str())))
}

/// The saved paths (suggestions or deletion marks) under `root` whose paths still exist.
pub fn load_paths(file: &Path, root: &Path) -> Vec<(PathBuf, String)> {
    load_all(file, root).into_iter().filter(|(p, _)| p.symlink_metadata().is_ok()).collect()
}

/// Replaces the saved paths under `root` with `marks`, keeping the ones
/// other scans made elsewhere.
pub fn save_paths<'a>(file: &Path, root: &Path, marks: impl Iterator<Item = (&'a Path, &'a str)>) -> io::Result<()> {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let old = std::fs::read_to_string(file).unwrap_or_default();
    let mut out: String = old.lines().filter(|l| l.split_once('\t').is_some_and(|(p, _)| !Path::new(p).starts_with(root))).map(|l| format!("{l}\n")).collect();
    for (p, r) in marks {
        out.push_str(&format!("{}\t{}\n", clean(&p.to_string_lossy()), clean(r)));
    }
    std::fs::create_dir_all(file.parent().unwrap())?;
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, out)?;
    std::fs::rename(&tmp, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().canonicalize().unwrap().join(format!("apdu-cache-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn find<'a>(n: &'a Node, name: &str) -> &'a Node {
        n.children.iter().find(|c| &*c.name == name).unwrap()
    }

    #[test]
    fn suggestions_survive_and_other_roots_keep_theirs() {
        let d = tmp("marks");
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::create_dir_all(d.join("b")).unwrap();
        let f = d.join("suggestions.tsv");
        let other = Path::new("/elsewhere/x");
        save_paths(&f, Path::new("/elsewhere"), [(other, "theirs")].into_iter()).unwrap();
        save_paths(&f, &d, [(d.join("a").as_path(), "big\tcache"), (d.join("b").as_path(), "old")].into_iter()).unwrap();
        std::fs::remove_dir(d.join("b")).unwrap();

        let back = load_paths(&f, &d);
        assert_eq!(back, vec![(d.join("a"), "big cache".to_string())], "b is gone, and tabs can't split a line");
        assert!(std::fs::read_to_string(&f).unwrap().contains("/elsewhere/x\ttheirs"));
        save_paths(&f, &d, std::iter::empty()).unwrap();
        assert!(load_paths(&f, &d).is_empty());
        assert!(std::fs::read_to_string(&f).unwrap().contains("theirs"), "clearing one root leaves the others");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn round_trips_and_collapses_git_and_node_modules() {
        let d = tmp("trip");
        for p in ["src", ".git/objects", "node_modules/left-pad"] {
            std::fs::create_dir_all(d.join(p)).unwrap();
        }
        std::fs::write(d.join("src/a.rs"), vec![1u8; 9000]).unwrap();
        std::fs::write(d.join(".git/objects/x"), vec![2u8; 9000]).unwrap();
        std::fs::write(d.join("node_modules/left-pad/i.js"), "x").unwrap();
        let tree = scan::scan(&d, true, &Progress::default());
        let file = d.with_extension("bin");
        let meta = Meta { event_id: 42, uuid: [7; 16], saved: std::time::SystemTime::now() };
        save(&file, &d, true, &meta, &tree).unwrap();

        let (m, back) = load(&file, &d, true).unwrap();
        assert_eq!((m.event_id, m.uuid), (42, [7; 16]));
        assert_eq!((back.alloc, back.private, back.items, back.files), (tree.alloc, tree.private, tree.items, tree.files));
        assert_eq!(find(&back, "src").children.len(), 1);
        let git = find(&back, ".git");
        assert!(collapsed(git) && git.alloc == find(&tree, ".git").alloc);
        assert!(collapsed(find(&back, "node_modules")));
        assert!(load(&file, &d, false).is_err(), "another one_fs setting is another cache");
        std::fs::remove_dir_all(&d).unwrap();
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn refresh_relists_changed_folders_and_keeps_the_rest() {
        let d = tmp("refresh");
        for p in ["keep/deep", "gone", "edit", "node_modules/pkg"] {
            std::fs::create_dir_all(d.join(p)).unwrap();
        }
        std::fs::write(d.join("keep/deep/big"), vec![1u8; 50_000]).unwrap();
        std::fs::write(d.join("gone/f"), vec![1u8; 50_000]).unwrap();
        std::fs::write(d.join("node_modules/pkg/f"), "x").unwrap();
        let file = d.with_extension("bin");
        let meta = Meta { event_id: 1, uuid: [0; 16], saved: std::time::SystemTime::now() };
        save(&file, &d, true, &meta, &scan::scan(&d, true, &Progress::default())).unwrap();
        let (_, mut tree) = load(&file, &d, true).unwrap();

        std::fs::remove_dir_all(d.join("gone")).unwrap();
        std::fs::create_dir_all(d.join("new/inner")).unwrap();
        std::fs::write(d.join("new/inner/f"), vec![1u8; 70_000]).unwrap();
        std::fs::write(d.join("edit/f"), vec![1u8; 30_000]).unwrap();
        std::fs::write(d.join("node_modules/pkg/g"), vec![1u8; 20_000]).unwrap();
        let changes = [d.clone(), d.join("edit"), d.join("new"), d.join("new/inner"), d.join("node_modules/pkg")]
            .into_iter()
            .map(|path| Change { path, recursive: false })
            .collect();
        let n = refresh(&mut tree, &d, changes, true, &Progress::default());

        let truth = scan::scan(&d, true, &Progress::default());
        assert_eq!((tree.alloc, tree.private, tree.items, tree.files), (truth.alloc, truth.private, truth.items, truth.files));
        assert!(tree.children.iter().all(|c| &*c.name != "gone"));
        assert_eq!(find(&tree, "keep").alloc, find(&truth, "keep").alloc);
        assert_eq!(find(find(&tree, "keep"), "deep").children.len(), 1);
        assert_eq!(find(&tree, "node_modules").children.len(), 1, "a change inside a collapsed folder rescans all of it");
        assert_eq!(n, 3, "new/ and new/inner come with relisting the root");
        std::fs::remove_dir_all(&d).unwrap();
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn relisting_reuses_saved_private_sizes_and_counts_each_folder_once() {
        let d = tmp("reuse");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a"), vec![1u8; 9000]).unwrap();
        let mut tree = scan::scan(&d, true, &Progress::default());
        tree.children.iter_mut().find(|c| &*c.name == "a").unwrap().private = 1234;
        std::fs::write(d.join("b"), vec![1u8; 20_000]).unwrap();

        let progress = Progress::default();
        let changes = [d.clone(), d.join("sub"), d.clone()].into_iter().map(|path| Change { path, recursive: false }).collect();
        let n = refresh(&mut tree, &d, changes, true, &progress);

        assert_eq!(find(&tree, "a").private, 1234, "an unchanged file keeps the size the index saved");
        assert_eq!(find(&tree, "b").private, find(&scan::scan(&d, true, &Progress::default()), "b").private);
        assert!(find(&tree, "b").private > 0);
        assert_eq!(n, 2, "a folder named twice is relisted once");
        assert_eq!(progress.items.load(Relaxed), 3, "a, b and sub are counted as they are listed");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
