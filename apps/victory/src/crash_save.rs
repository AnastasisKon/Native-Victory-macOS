//! Diagnostics: `VICTORY_CRASH_SAVE=<rva>:<len>,...` on the native host.
//!
//! The Windows path saves the same ranges from its vectored exception handler
//! (`run::diagnostics`, `victory_loader::save_on_crash`). Here they are written
//! from the fatal-signal handler (`crate::log_fatal_signals`), which runs on
//! the faulting thread: it may allocate nothing and take no lock, since the
//! fault can have happened inside the allocator or the file system. So every
//! path is turned into a `CString` and every range into an address and a
//! length at startup, and the handler only calls `open`, `write` and `close`.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// One title range and the file it is written to on a crash.
struct Save {
    path: CString,
    addr: usize,
    len: usize,
}

static SAVES: std::sync::OnceLock<Vec<Save>> = std::sync::OnceLock::new();

/// Reads `VICTORY_CRASH_SAVE` and keeps what it names, bounded by the title
/// image `base` and `size`: an entry that does not parse, and a range past the
/// image, are left out, as the Windows path leaves them out. Each range is
/// written to `crash-<rva>.bin` under `cache_dir`, and only on a crash.
pub fn configure(base: usize, size: usize, cache_dir: &Path) {
    set(
        base,
        size,
        cache_dir,
        &std::env::var("VICTORY_CRASH_SAVE").unwrap_or_default(),
    );
}

/// [`configure`]'s work, on a list it is given, so that a test can name memory
/// of its own instead of the title's.
fn set(base: usize, size: usize, cache_dir: &Path, value: &str) {
    let saves = parse(value, size)
        .into_iter()
        .filter_map(|(rva, len)| {
            let path = cache_dir.join(format!("crash-{rva:x}.bin"));
            Some(Save {
                path: CString::new(path.as_os_str().as_bytes()).ok()?,
                addr: base + rva,
                len,
            })
        })
        .collect();
    let _ = SAVES.set(saves);
}

/// The `(rva, len)` pairs of a `VICTORY_CRASH_SAVE` list (hex, `0x` optional,
/// comma-separated), leaving out what does not parse and what runs past `size`.
fn parse(value: &str, size: usize) -> Vec<(usize, usize)> {
    let hex = |s: &str| usize::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    value
        .split(',')
        .filter_map(|r| r.split_once(':'))
        .filter_map(|(rva, len)| Some((hex(rva)?, hex(len)?)))
        .filter(|&(rva, len)| rva.checked_add(len).is_some_and(|end| end <= size))
        .collect()
}

/// Writes every configured range: async-signal-safe (no allocation, no lock,
/// only `open`, `write` and `close`).
///
/// # Safety
///
/// Every range must name memory that is readable: they were read from the
/// title's image, which the title does not unmap.
pub unsafe fn write_all() {
    if let Some(saves) = SAVES.get() {
        // SAFETY: the caller's contract; the list was made at startup and is never changed.
        unsafe { write_list(saves) }
    }
}

/// # Safety
///
/// As [`write_all`].
unsafe fn write_list(saves: &[Save]) {
    for s in saves {
        // SAFETY: `path` is NUL-terminated and outlives the call.
        let fd = unsafe {
            libc::open(
                s.path.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                0o644,
            )
        };
        if fd < 0 {
            continue;
        }
        let (mut at, end) = (s.addr, s.addr + s.len);
        while at < end {
            let n = (end - at).min(1 << 20);
            // SAFETY: the caller of `configure` named readable title memory.
            let wrote = unsafe { libc::write(fd, at as *const libc::c_void, n) };
            if wrote <= 0 {
                break;
            }
            at += wrote as usize;
        }
        // SAFETY: `fd` is the one opened above.
        unsafe { libc::close(fd) };
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, set, write_all};

    #[test]
    fn crash_save_takes_hex_pairs_inside_the_image() {
        assert_eq!(
            parse("23cf683:40, 0x55e56f8:100", 0x1000_0000),
            [(0x23cf683, 0x40), (0x55e56f8, 0x100)]
        );
        // Past the image, unparseable, or not a pair: left out.
        assert!(parse("fffffff0:20", 0x1000).is_empty());
        assert!(parse("zz:10,20", 0x1000).is_empty());
        assert!(parse("", 0x1000).is_empty());
        // A range that ends exactly at the image's end is kept.
        assert_eq!(parse("ff0:10", 0x1000), [(0xff0, 0x10)]);
    }

    /// The handler's path end to end: the ranges the switch names are written,
    /// from the addresses they name, when the process reports a crash. This test
    /// stands in the title image with a static array, so it names its own memory.
    #[test]
    fn a_saved_range_is_the_memory_it_named() {
        static MEMORY: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
        let dir = std::env::temp_dir().join(format!("victory-crash-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        set(MEMORY.as_ptr() as usize, MEMORY.len(), &dir, "2:4");
        // SAFETY: the range is this test's own array, which outlives the call.
        unsafe { write_all() };
        assert_eq!(
            std::fs::read(dir.join("crash-2.bin")).unwrap(),
            [3, 4, 5, 6]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
