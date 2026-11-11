//! The FSEvents journal: which folders changed since an event id.
//!
//! macOS logs, per volume, every folder whose direct contents changed. A stream
//! created with a past `sinceWhen` replays that history and then sends an event
//! flagged HistoryDone. The journal is only trustworthy while the volume's
//! FSEvents UUID is unchanged; a new UUID means the history was reset.

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

type CFRef = *const c_void;

#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type Callback = extern "C" fn(*const c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(dev: libc::dev_t) -> CFRef;
    fn FSEventStreamCreate(alloc: CFRef, cb: Callback, ctx: *mut StreamContext, paths: CFRef, since: u64, latency: f64, flags: u32) -> *mut c_void;
    fn FSEventStreamSetDispatchQueue(stream: *mut c_void, queue: *mut c_void);
    fn FSEventStreamStart(stream: *mut c_void) -> u8;
    fn FSEventStreamStop(stream: *mut c_void);
    fn FSEventStreamInvalidate(stream: *mut c_void);
    fn FSEventStreamRelease(stream: *mut c_void);
}

#[repr(C)]
struct UuidBytes([u8; 16]);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: c_void;
    fn CFStringCreateWithCString(alloc: CFRef, s: *const c_char, encoding: u32) -> CFRef;
    fn CFArrayCreate(alloc: CFRef, values: *const CFRef, n: isize, callbacks: *const c_void) -> CFRef;
    fn CFUUIDGetUUIDBytes(uuid: CFRef) -> UuidBytes;
    fn CFRelease(r: CFRef);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_release(obj: *mut c_void);
}

const UTF8: u32 = 0x0800_0100;
const MUST_SCAN_SUBDIRS: u32 = 0x01;
const USER_DROPPED: u32 = 0x02;
const KERNEL_DROPPED: u32 = 0x04;
const IDS_WRAPPED: u32 = 0x08;
const HISTORY_DONE: u32 = 0x10;
const ROOT_CHANGED: u32 = 0x20;
const MOUNT: u32 = 0x40;
const UNMOUNT: u32 = 0x80;
const CREATE_NO_DEFER: u32 = 0x02;

/// The newest event id; capture it before reading the disk so nothing that
/// happens during the read is missed next time.
pub fn current_id() -> u64 {
    unsafe { FSEventsGetCurrentEventId() }
}

/// The volume's journal UUID, or None if it keeps no journal.
pub fn uuid(dev: libc::dev_t) -> Option<[u8; 16]> {
    let u = unsafe { FSEventsCopyUUIDForDevice(dev) };
    if u.is_null() {
        return None;
    }
    let bytes = unsafe { CFUUIDGetUUIDBytes(u) }.0;
    unsafe { CFRelease(u) };
    Some(bytes)
}

/// A folder to bring up to date: just its own listing, or everything under it.
#[derive(Debug, PartialEq)]
pub struct Change {
    pub path: PathBuf,
    pub recursive: bool,
}

#[derive(Default)]
struct State {
    changes: Vec<Change>,
    lost: bool,
    done: bool,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

extern "C" fn callback(_: *const c_void, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const u32, _: *const u64) {
    let shared = unsafe { &*(info as *const Shared) };
    let paths = paths as *const *const c_char;
    let mut st = shared.state.lock().unwrap();
    for i in 0..n {
        let f = unsafe { *flags.add(i) };
        if f & HISTORY_DONE != 0 {
            st.done = true;
            continue;
        }
        if f & (USER_DROPPED | KERNEL_DROPPED | IDS_WRAPPED | ROOT_CHANGED | MOUNT | UNMOUNT) != 0 {
            st.lost = true;
        }
        let p = unsafe { CStr::from_ptr(*paths.add(i)) }.to_string_lossy();
        let path = PathBuf::from(p.trim_end_matches('/').to_string()).components().collect::<PathBuf>();
        st.changes.push(Change { path, recursive: f & MUST_SCAN_SUBDIRS != 0 });
    }
    if st.done {
        shared.cv.notify_all();
    }
}

/// Every folder under `root` changed after event `since`, or None when the
/// journal can't account for the gap (dropped events, a remount, a timeout)
/// and only a full scan can be trusted.
pub fn changes_since(root: &Path, since: u64, timeout: Duration) -> Option<Vec<Change>> {
    let shared = Box::new(Shared { state: Mutex::new(State::default()), cv: Condvar::new() });
    let root_c = CString::new(root.as_os_str().as_encoded_bytes()).ok()?;
    let finished = unsafe {
        let s = CFStringCreateWithCString(std::ptr::null(), root_c.as_ptr(), UTF8);
        let paths = CFArrayCreate(std::ptr::null(), &s, 1, &kCFTypeArrayCallBacks);
        CFRelease(s);
        let mut ctx = StreamContext {
            version: 0,
            info: &*shared as *const Shared as *mut c_void,
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
        };
        let stream = FSEventStreamCreate(std::ptr::null(), callback, &mut ctx, paths, since, 0.0, CREATE_NO_DEFER);
        CFRelease(paths);
        if stream.is_null() {
            return None;
        }
        let queue = dispatch_queue_create(c"apdu.fsevents".as_ptr(), std::ptr::null());
        FSEventStreamSetDispatchQueue(stream, queue);
        let started = FSEventStreamStart(stream) != 0;
        let finished = started && {
            let st = shared.state.lock().unwrap();
            let (st, _) = shared.cv.wait_timeout_while(st, timeout, |s| !s.done).unwrap();
            st.done
        };
        if started {
            FSEventStreamStop(stream);
        }
        FSEventStreamInvalidate(stream);
        FSEventStreamRelease(stream);
        dispatch_release(queue);
        finished
    };
    let st = shared.state.into_inner().unwrap();
    (finished && !st.lost).then_some(st.changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_folder_written_after_the_mark() {
        let dir = std::env::temp_dir().canonicalize().unwrap().join(format!("apdu-fsevents-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let mark = current_id();
        std::fs::write(dir.join("sub/f.txt"), "x").unwrap();
        // fseventsd batches writes to its journal; give it time to flush.
        let mut found = false;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            let changes = changes_since(&dir, mark, Duration::from_secs(10)).expect("journal readable");
            if changes.iter().any(|c| c.path == dir.join("sub")) {
                found = true;
                break;
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(found, "the write under sub/ shows up as a change to sub/");
    }

    #[test]
    fn the_data_volume_has_a_journal() {
        let st = std::fs::metadata(std::env::temp_dir()).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert!(uuid(st.dev() as libc::dev_t).is_some());
    }
}
