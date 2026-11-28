//! Files for the native host: the calls of `modules::files` that reach the
//! host's own kernel32 on Windows, on POSIX instead.
//!
//! The title's storage is drives (`G:\..`, `T:\..`, the persistent
//! storage's): a drive is a mounted host folder, and a path on one is
//! rewritten to the host path, as `modules::files` does for Windows (there
//! the rewritten path goes to the host's `CreateFileW`; here it goes to
//! `open`). A path on a drive that is not mounted is not found.
//!
//! Done: `CreateFile(W|A)`, `ReadFile`, `WriteFile`, `SetFilePointer(Ex)`,
//! `GetFileSize(Ex)`, `SetEndOfFile`, `FlushFileBuffers`,
//! `GetFileAttributes(W|A|ExW)`, `FindFirstFile(W|ExW)`, `FindNextFileW`,
//! `FindClose`, `DeleteFileW`, `CreateDirectoryW`, `RemoveDirectoryW`,
//! `GetOverlappedResult`. Overlapped reads and writes complete before the call
//! returns, which is a legal outcome. `CreateFileMapping(W)`,
//! `OpenFileMappingW`, `MapViewOfFile(Ex)`, `FlushViewOfFile` and
//! `UnmapViewOfFile` are here too, as a copy of the file in title memory (see
//! "File mappings" below).
//!
//! Not done: the C runtime's file functions (`fopen`, `_wopen`, `_stat64`,
//! ...), `MoveFileEx`, `GetDiskFreeSpace`, `CreateFile2`, `CreateFileMappingA`,
//! share modes and locking, and `dstorage.rs` (see `docs/native-host.md`).

#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, c_char, c_void};
use std::fs::{DirEntry, File, OpenOptions, ReadDir};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime};

use super::{HANDLES, Object, lock, new_handle, set_error};

const INVALID_HANDLE_VALUE: usize = usize::MAX;
const INVALID_FILE_SIZE: u32 = 0xFFFF_FFFF;
const INVALID_FILE_ATTRIBUTES: u32 = 0xFFFF_FFFF;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_PATH_NOT_FOUND: u32 = 3;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_NO_MORE_FILES: u32 = 18;
const ERROR_HANDLE_EOF: u32 = 38;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_ALREADY_EXISTS: u32 = 183;
const STATUS_END_OF_FILE: usize = 0xC000_0011;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const FILE_READ_DATA: u32 = 1;
const FILE_WRITE_DATA: u32 = 2;
const FILE_APPEND_DATA: u32 = 4;
const CREATE_NEW: u32 = 1;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const OPEN_ALWAYS: u32 = 4;
const TRUNCATE_EXISTING: u32 = 5;
const FILE_ATTRIBUTE_READONLY: u32 = 1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
/// Seconds from 1601 (FILETIME's epoch) to 1970.
const EPOCH_DIFFERENCE: u64 = 11_644_473_600;

// Drives.

static DRIVES: Mutex<Vec<(u8, PathBuf)>> = Mutex::new(Vec::new());

fn drives() -> MutexGuard<'static, Vec<(u8, PathBuf)>> {
    DRIVES.lock().unwrap_or_else(|e| e.into_inner())
}

/// Gives the host folder `root` a drive letter (the same one if it has one):
/// the highest free letter.
pub fn mount(root: &Path) -> Option<char> {
    let root = std::path::absolute(root).ok()?;
    let mut drives = drives();
    if let Some((letter, _)) = drives.iter().find(|(_, r)| *r == root) {
        return Some(char::from(*letter));
    }
    let letter = (b'D'..=b'Z')
        .rev()
        .find(|l| !drives.iter().any(|(m, _)| m == l))?;
    drives.push((letter, root));
    Some(char::from(letter))
}

/// Gives `root` the letter the console fixes (`G`, `T`).
pub fn mount_at(letter: char, root: &Path) {
    let Ok(letter) = u8::try_from(letter.to_ascii_uppercase()) else {
        return;
    };
    let Ok(root) = std::path::absolute(root) else {
        return;
    };
    let mut drives = drives();
    drives.retain(|(l, _)| *l != letter);
    drives.push((letter, root));
}

/// The host path for a title's path: a drive path goes under its folder,
/// with `.` and `..` resolved here so that `..` stops at the root; any
/// other path is taken as it is, its backslashes made slashes.
pub(crate) fn translate(path: &[u16]) -> Option<PathBuf> {
    let text = String::from_utf16_lossy(path);
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let letter = bytes[0].to_ascii_uppercase();
        let root = drives().iter().find(|(l, _)| *l == letter)?.1.clone();
        let mut parts: Vec<&str> = Vec::new();
        for part in text[2..].split(['\\', '/']) {
            match part {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                part => parts.push(part),
            }
        }
        return Some(parts.iter().fold(root, |p, c| p.join(c)));
    }
    Some(PathBuf::from(text.replace('\\', "/")))
}

/// A NUL-terminated wide string.
pub(super) fn wide_str(p: *const u16) -> Vec<u16> {
    // SAFETY: the title passes a NUL-terminated path.
    unsafe { std::slice::from_raw_parts(p, (0..).take_while(|&i| *p.add(i) != 0).count()) }.to_vec()
}

pub(crate) fn wide_path(p: *const u16) -> Option<PathBuf> {
    if p.is_null() {
        return None;
    }
    translate(&wide_str(p))
}

pub(super) fn narrow_path(p: *const c_char) -> Option<PathBuf> {
    if p.is_null() {
        return None;
    }
    // SAFETY: the title passes a NUL-terminated path.
    let text = String::from_utf8_lossy(unsafe { CStr::from_ptr(p) }.to_bytes()).into_owned();
    translate(&text.encode_utf16().collect::<Vec<_>>())
}

/// The Win32 error for an I/O failure ([`crate::win32_errors`], `ERROR_ACCESS_DENIED` for the
/// rest). A missing file whose parent folder is missing too is `ERROR_PATH_NOT_FOUND`, as
/// `CreateFileW` says.
pub(crate) fn win32_error(e: &io::Error, path: &Path) -> u32 {
    match crate::win32_errors::code(e) {
        Some(ERROR_FILE_NOT_FOUND)
            if path
                .parent()
                .is_some_and(|p| !p.as_os_str().is_empty() && !p.exists()) =>
        {
            ERROR_PATH_NOT_FOUND
        }
        Some(code) => code,
        None => ERROR_ACCESS_DENIED,
    }
}

// File handles.

/// An open file and its file pointer.
pub(crate) struct FileObject {
    file: File,
    position: Mutex<u64>,
    writable: bool,
    /// Opened for `FILE_APPEND_DATA` without `FILE_WRITE_DATA`: every write goes to the end.
    append: bool,
    /// The completion port this handle reports to, and the key it reports with.
    port: Mutex<Option<(Arc<Port>, usize)>>,
}

/// The host's metadata of the open file `handle`.
pub(crate) fn metadata_of(handle: *mut c_void) -> Option<std::fs::Metadata> {
    file_of(handle)?.file.metadata().ok()
}

fn file_of(handle: *mut c_void) -> Option<Arc<FileObject>> {
    match lock(&HANDLES).get(&(handle as usize)) {
        Some(Object::File(f)) => Some(f.clone()),
        _ => None,
    }
}

fn invalid_handle() -> *mut c_void {
    INVALID_HANDLE_VALUE as *mut c_void
}

/// A standard descriptor as a file object, so `GetStdHandle` can hand the title
/// a handle that `ReadFile` and `WriteFile` take.
pub(crate) fn standard(fd: i32) -> Option<Arc<FileObject>> {
    // SAFETY: a plain copy of a standard descriptor; the copy is ours to close.
    let copy = unsafe { libc::dup(fd) };
    if copy < 0 {
        return None;
    }
    // SAFETY: `copy` is a descriptor no other owner holds.
    let file = unsafe { <File as std::os::unix::io::FromRawFd>::from_raw_fd(copy) };
    Some(Arc::new(FileObject {
        file,
        position: Mutex::new(0),
        // Reading from the console is what fd 0 is for.
        writable: fd != 0,
        append: false,
        port: Mutex::new(None),
    }))
}

const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;
const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;
const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;

/// The handles handed out, kept so each call returns the same one, as Windows
/// does: a title may close one and expect the next call to fail.
static STD: Mutex<[usize; 3]> = Mutex::new([0; 3]);

pub extern "system" fn GetStdHandle(which: u32) -> *mut c_void {
    let fd = match which {
        STD_INPUT_HANDLE => 0,
        STD_OUTPUT_HANDLE => 1,
        STD_ERROR_HANDLE => 2,
        _ => {
            set_error(ERROR_INVALID_PARAMETER);
            return std::ptr::null_mut();
        }
    };
    let mut std = lock(&STD);
    if std[fd] == 0 {
        match standard(fd as i32) {
            Some(object) => std[fd] = new_handle(Object::File(object)) as usize,
            None => {
                set_error(ERROR_INVALID_HANDLE);
                return invalid_handle();
            }
        }
    }
    std[fd] as *mut c_void
}

fn create(path: Option<PathBuf>, access: u32, disposition: u32, flags: u32) -> *mut c_void {
    let Some(path) = path else {
        set_error(ERROR_PATH_NOT_FOUND);
        return invalid_handle();
    };
    if std::env::var_os("VICTORY_TRACE_FILES").is_some() {
        tracing::info!(path = %path.display(), access = format_args!("{access:#x}"), "CreateFile");
    }
    let all = access & GENERIC_ALL != 0;
    let read = all || access & (GENERIC_READ | FILE_READ_DATA) != 0;
    let write = all || access & (GENERIC_WRITE | FILE_WRITE_DATA | FILE_APPEND_DATA) != 0;
    let append =
        !all && access & (GENERIC_WRITE | FILE_WRITE_DATA) == 0 && access & FILE_APPEND_DATA != 0;
    let existed = path.exists();
    let mut options = OpenOptions::new();
    options.read(read || !write).write(write);
    match disposition {
        CREATE_NEW => {
            options.create_new(true).write(true);
        }
        CREATE_ALWAYS => {
            options.create(true).truncate(true).write(true);
        }
        OPEN_EXISTING => {}
        OPEN_ALWAYS => {
            // Darwin permits O_RDONLY | O_CREAT, including when the file already exists.
            const O_CREAT: i32 = 0x0200;
            options.custom_flags(O_CREAT);
        }
        TRUNCATE_EXISTING if access & (GENERIC_WRITE | GENERIC_ALL) != 0 => {
            options.truncate(true);
        }
        _ => {
            set_error(ERROR_INVALID_PARAMETER);
            return invalid_handle();
        }
    }
    match options.open(&path) {
        Ok(file) => {
            set_error(
                if existed && matches!(disposition, CREATE_ALWAYS | OPEN_ALWAYS) {
                    ERROR_ALREADY_EXISTS
                } else {
                    0
                },
            );
            let _ = flags;
            new_handle(Object::File(Arc::new(FileObject {
                file,
                position: Mutex::new(0),
                writable: write,
                append,
                port: Mutex::new(None),
            })))
        }
        Err(e) => {
            set_error(win32_error(&e, &path));
            invalid_handle()
        }
    }
}

pub extern "system" fn CreateFileW(
    path: *const u16,
    access: u32,
    _share: u32,
    _security: *const c_void,
    disposition: u32,
    flags: u32,
    _template: *mut c_void,
) -> *mut c_void {
    create(wide_path(path), access, disposition, flags)
}

pub extern "system" fn CreateFileA(
    path: *const c_char,
    access: u32,
    _share: u32,
    _security: *const c_void,
    disposition: u32,
    flags: u32,
    _template: *mut c_void,
) -> *mut c_void {
    create(narrow_path(path), access, disposition, flags)
}

/// The part of `OVERLAPPED` a synchronous completion fills in.
struct Overlapped(*mut u8);

impl Overlapped {
    fn offset(&self) -> u64 {
        // SAFETY: the title's OVERLAPPED: Offset, OffsetHigh at 16 and 20.
        unsafe { self.0.add(16).cast::<u64>().read_unaligned() }
    }

    fn complete(&self, status: usize, bytes: usize) {
        // SAFETY: the title's OVERLAPPED: Internal and InternalHigh at 0 and 8.
        unsafe {
            self.0.add(8).cast::<usize>().write(bytes);
            self.0.cast::<usize>().write(status);
            let event = self.0.add(24).cast::<usize>().read() & !1;
            if let Some(Object::Event(e)) = lock(&HANDLES).get(&event) {
                e.set();
                super::sync::signalled();
            }
        }
    }
}

/// Synchronous I/O holds the file position lock from offset selection through completion.
enum IoPosition<'a> {
    Synchronous(MutexGuard<'a, u64>),
    Overlapped(Overlapped),
}

impl<'a> IoPosition<'a> {
    fn new(file: &'a FileObject, overlapped: *mut u8) -> Self {
        if overlapped.is_null() {
            Self::Synchronous(lock(&file.position))
        } else {
            Self::Overlapped(Overlapped(overlapped))
        }
    }

    fn offset(&self) -> u64 {
        match self {
            Self::Synchronous(position) => **position,
            Self::Overlapped(ov) => ov.offset(),
        }
    }

    fn complete(self, offset: u64, bytes: usize) {
        match self {
            Self::Synchronous(mut position) => *position = offset + bytes as u64,
            Self::Overlapped(ov) => ov.complete(0, bytes),
        }
    }
}

pub extern "system" fn ReadFile(
    handle: *mut c_void,
    buffer: *mut u8,
    len: u32,
    read: *mut u32,
    overlapped: *mut u8,
) -> i32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if len > 0 && buffer.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if std::env::var_os("VICTORY_TRACE_FILES").is_some() {
        trace_read(&f.file, len);
    }
    let out = if len == 0 {
        &mut []
    } else {
        // SAFETY: the title's buffer of `len` bytes.
        unsafe { std::slice::from_raw_parts_mut(buffer, len as usize) }
    };
    let position = IoPosition::new(&f, overlapped);
    let at = position.offset();
    let mut done = 0;
    while done < out.len() {
        match f.file.read_at(&mut out[done..], at + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                set_error(ERROR_ACCESS_DENIED);
                return 0;
            }
        }
    }
    if !read.is_null() {
        // SAFETY: the title's out-pointer.
        unsafe { read.write(done as u32) };
    }
    if len > 0
        && done == 0
        && let IoPosition::Overlapped(ov) = &position
    {
        ov.complete(STATUS_END_OF_FILE, 0);
        f.post_completion(0, overlapped, STATUS_END_OF_FILE);
        set_error(ERROR_HANDLE_EOF);
        return 0;
    }
    position.complete(at, done);
    f.post_completion(done, overlapped, 0);
    1
}

pub extern "system" fn WriteFile(
    handle: *mut c_void,
    buffer: *const u8,
    len: u32,
    written: *mut u32,
    overlapped: *mut u8,
) -> i32 {
    if !written.is_null() {
        // SAFETY: the title's byte-count out-pointer.
        unsafe { written.write(0) };
    }
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if !f.writable {
        set_error(ERROR_ACCESS_DENIED);
        return 0;
    }
    if len > 0 && buffer.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let data = if len == 0 {
        &[]
    } else {
        // SAFETY: the title's buffer of `len` bytes.
        unsafe { std::slice::from_raw_parts(buffer, len as usize) }
    };
    let position = IoPosition::new(&f, overlapped);
    let at = if f.append {
        // Append-only access writes at the end whatever the offset, as Windows does.
        match f.file.metadata() {
            Ok(m) => m.len(),
            Err(_) => {
                set_error(ERROR_ACCESS_DENIED);
                return 0;
            }
        }
    } else {
        position.offset()
    };
    if f.file.write_all_at(data, at).is_err() {
        set_error(ERROR_ACCESS_DENIED);
        return 0;
    }
    position.complete(at, data.len());
    f.post_completion(data.len(), overlapped, 0);
    if !written.is_null() {
        // SAFETY: the title's out-pointer.
        unsafe { written.write(len) };
    }
    1
}

pub extern "system" fn GetOverlappedResult(
    _handle: *mut c_void,
    overlapped: *mut u8,
    bytes: *mut u32,
    _wait: i32,
) -> i32 {
    if !bytes.is_null() {
        // SAFETY: the title's OVERLAPPED and out-pointer.
        unsafe { bytes.write(overlapped.add(8).cast::<usize>().read() as u32) };
    }
    // SAFETY: Internal is the first pointer-sized field of the title's OVERLAPPED.
    if !overlapped.is_null() && unsafe { overlapped.cast::<usize>().read() } == STATUS_END_OF_FILE {
        set_error(ERROR_HANDLE_EOF);
        return 0;
    }
    1
}

fn seek(f: &FileObject, distance: i64, method: u32) -> Option<u64> {
    let mut position = lock(&f.position);
    let base = match method {
        0 => 0,
        1 => *position as i64,
        2 => f.file.metadata().ok()?.len() as i64,
        _ => return None,
    };
    let to = u64::try_from(base.checked_add(distance)?).ok()?;
    *position = to;
    Some(to)
}

pub extern "system" fn SetFilePointer(
    handle: *mut c_void,
    low: i32,
    high: *mut i32,
    method: u32,
) -> u32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return INVALID_FILE_SIZE;
    };
    let distance = if high.is_null() {
        i64::from(low)
    } else {
        // SAFETY: the title's high word.
        i64::from(unsafe { high.read() }) << 32 | i64::from(low as u32)
    };
    let Some(to) = seek(&f, distance, method) else {
        set_error(ERROR_INVALID_PARAMETER);
        return INVALID_FILE_SIZE;
    };
    set_error(0);
    if !high.is_null() {
        // SAFETY: as above.
        unsafe { high.write((to >> 32) as i32) };
    }
    to as u32
}

pub extern "system" fn SetFilePointerEx(
    handle: *mut c_void,
    distance: i64,
    new_position: *mut i64,
    method: u32,
) -> i32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let Some(to) = seek(&f, distance, method) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    if !new_position.is_null() {
        // SAFETY: the title's out-pointer.
        unsafe { new_position.write(to as i64) };
    }
    1
}

pub extern "system" fn GetFileSizeEx(handle: *mut c_void, size: *mut i64) -> i32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let Ok(m) = f.file.metadata() else {
        set_error(ERROR_ACCESS_DENIED);
        return 0;
    };
    // SAFETY: the title's out-pointer.
    unsafe { size.write(m.len() as i64) };
    1
}

pub extern "system" fn GetFileSize(handle: *mut c_void, high: *mut u32) -> u32 {
    let mut size = 0;
    if GetFileSizeEx(handle, &mut size) == 0 {
        return INVALID_FILE_SIZE;
    }
    set_error(0);
    if !high.is_null() {
        // SAFETY: the title's out-pointer.
        unsafe { high.write((size >> 32) as u32) };
    }
    size as u32
}

pub extern "system" fn SetEndOfFile(handle: *mut c_void) -> i32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if !f.writable || f.append {
        set_error(ERROR_ACCESS_DENIED);
        return 0;
    }
    let position = lock(&f.position);
    i32::from(f.file.set_len(*position).is_ok())
}

pub extern "system" fn FlushFileBuffers(handle: *mut c_void) -> i32 {
    let Some(f) = file_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    i32::from(f.file.sync_all().is_ok())
}

// Attributes and directories.

pub(crate) fn attributes_of(m: &std::fs::Metadata) -> u32 {
    let mut a = if m.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_NORMAL
    };
    if m.permissions().readonly() {
        a |= FILE_ATTRIBUTE_READONLY;
        a &= !FILE_ATTRIBUTE_NORMAL;
    }
    a
}

pub(crate) fn filetime(t: io::Result<SystemTime>) -> u64 {
    let since = t
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .unwrap_or_default();
    (since.as_secs() + EPOCH_DIFFERENCE) * 10_000_000 + u64::from(since.subsec_nanos()) / 100
}

fn attributes(path: Option<PathBuf>) -> u32 {
    let Some(path) = path else {
        set_error(ERROR_PATH_NOT_FOUND);
        return INVALID_FILE_ATTRIBUTES;
    };
    match std::fs::metadata(&path) {
        Ok(m) => attributes_of(&m),
        Err(e) => {
            set_error(win32_error(&e, &path));
            INVALID_FILE_ATTRIBUTES
        }
    }
}

pub extern "system" fn GetFileAttributesW(path: *const u16) -> u32 {
    attributes(wide_path(path))
}

pub extern "system" fn GetFileAttributesA(path: *const c_char) -> u32 {
    attributes(narrow_path(path))
}

/// `WIN32_FILE_ATTRIBUTE_DATA`: attributes, three times, the size.
pub extern "system" fn GetFileAttributesExW(path: *const u16, _level: u32, data: *mut u8) -> i32 {
    let Some(path) = wide_path(path) else {
        set_error(ERROR_PATH_NOT_FOUND);
        return 0;
    };
    match std::fs::metadata(&path) {
        Ok(m) => {
            // SAFETY: the title's 36-byte structure.
            unsafe {
                data.cast::<u32>().write_unaligned(attributes_of(&m));
                data.add(4)
                    .cast::<u64>()
                    .write_unaligned(filetime(m.created()));
                data.add(12)
                    .cast::<u64>()
                    .write_unaligned(filetime(m.accessed()));
                data.add(20)
                    .cast::<u64>()
                    .write_unaligned(filetime(m.modified()));
                data.add(28)
                    .cast::<u32>()
                    .write_unaligned((m.len() >> 32) as u32);
                data.add(32).cast::<u32>().write_unaligned(m.len() as u32);
            }
            1
        }
        Err(e) => {
            set_error(win32_error(&e, &path));
            0
        }
    }
}

pub extern "system" fn DeleteFileW(path: *const u16) -> i32 {
    let Some(path) = wide_path(path) else {
        set_error(ERROR_PATH_NOT_FOUND);
        return 0;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => 1,
        Err(e) => {
            set_error(win32_error(&e, &path));
            0
        }
    }
}

pub extern "system" fn CreateDirectoryW(path: *const u16, _security: *const c_void) -> i32 {
    let Some(path) = wide_path(path) else {
        set_error(ERROR_PATH_NOT_FOUND);
        return 0;
    };
    match std::fs::create_dir(&path) {
        Ok(()) => 1,
        Err(e) => {
            set_error(if e.kind() == io::ErrorKind::AlreadyExists {
                ERROR_ALREADY_EXISTS
            } else {
                win32_error(&e, &path)
            });
            0
        }
    }
}

pub extern "system" fn RemoveDirectoryW(path: *const u16) -> i32 {
    let Some(path) = wide_path(path) else {
        set_error(ERROR_PATH_NOT_FOUND);
        return 0;
    };
    match std::fs::remove_dir(&path) {
        Ok(()) => 1,
        Err(e) => {
            set_error(win32_error(&e, &path));
            0
        }
    }
}

// Directory searches.

#[cfg(test)]
thread_local! {
    pub(super) static FIND_METADATA_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn find_metadata(result: io::Result<std::fs::Metadata>) -> Option<std::fs::Metadata> {
    #[cfg(test)]
    FIND_METADATA_LOOKUPS.set(FIND_METADATA_LOOKUPS.get() + 1);
    result.ok()
}

/// What `FindFirstFile` found: its entries, and the next to give out.
pub(crate) struct Find {
    entries: Vec<(String, Option<std::fs::Metadata>)>,
    next: usize,
}

/// One element of a search mask, after Windows' DOS rewrite of `*`, `?` and `.`.
#[derive(Clone, Copy, PartialEq)]
enum Mask {
    Char(char),
    /// `*`: any run of characters.
    Star,
    /// `<`: any run of characters that ends at or before the name's last `.` (`*` before a
    /// final `.`).
    DosStar,
    /// `>`: one character, or none at a `.` or the end of the name (every `?`).
    DosQm,
    /// `"`: a `.`, or nothing at the end of the name (a `.` before `*` or `?`).
    DosDot,
}

/// The mask `FindFirstFile` hands the file system for `pattern`: trailing dots and spaces
/// are dropped, a `.` before `*` or `?` may match the end of the name, `?` may match nothing
/// at a `.` or the end, and a `*` that ended in dropped dots stops at the last `.`. The
/// three DOS wildcards may also be written as `<`, `>` and `"`.
fn dos_mask(pattern: &[char]) -> Vec<Mask> {
    let no_ext = pattern.last() == Some(&'.');
    let len = pattern.len()
        - pattern
            .iter()
            .rev()
            .take_while(|&&c| c == '.' || c == ' ')
            .count();
    let p = &pattern[..len];
    let mut mask: Vec<Mask> = p
        .iter()
        .enumerate()
        .map(|(i, &c)| match c {
            '.' if matches!(p.get(i + 1), Some('*' | '?')) => Mask::DosDot,
            '?' | '>' => Mask::DosQm,
            '*' => Mask::Star,
            '<' => Mask::DosStar,
            '"' => Mask::DosDot,
            c => Mask::Char(c),
        })
        .collect();
    if no_ext && mask.last() == Some(&Mask::Star) {
        *mask.last_mut().unwrap() = Mask::DosStar;
    }
    mask
}

/// Whether `name` matches a mask from `dos_mask`, ignoring case.
fn mask_matches(mask: &[Mask], name: &[char]) -> bool {
    let Some((&m, rest)) = mask.split_first() else {
        return name.is_empty();
    };
    match m {
        Mask::Char(c) => name.split_first().is_some_and(|(n, tail)| {
            c.to_lowercase().eq(n.to_lowercase()) && mask_matches(rest, tail)
        }),
        Mask::Star => (0..=name.len()).any(|i| mask_matches(rest, &name[i..])),
        Mask::DosStar => {
            // It may take the last `.` but nothing of the extension after it.
            let end = name
                .iter()
                .rposition(|&c| c == '.')
                .map_or(name.len(), |d| d + 1);
            (0..=end).any(|i| mask_matches(rest, &name[i..]))
        }
        Mask::DosQm => match name.first() {
            None => mask_matches(rest, name),
            Some('.') => {
                // Nothing at a `.`, unless the run of `?` ends the mask: then it takes the `.`.
                let after = &rest[rest.iter().take_while(|&&m| m == Mask::DosQm).count()..];
                if after.is_empty() {
                    mask_matches(after, &name[1..])
                } else {
                    mask_matches(after, name)
                }
            }
            Some(_) => mask_matches(rest, &name[1..]),
        },
        Mask::DosDot => match name.split_first() {
            None => mask_matches(rest, name),
            Some(('.', tail)) => mask_matches(rest, tail),
            Some(_) => false,
        },
    }
}

/// Whether `name` matches a `FindFirstFile` pattern as Windows matches long names, ignoring
/// case: `*.*` matches every name, `a.*` also `a`, `*.` only names without an extension.
fn wildcard(pattern: &[char], name: &[char]) -> bool {
    // The file system matches `..` as `.`.
    let name = if name == ['.', '.'] { &name[..1] } else { name };
    mask_matches(&dos_mask(pattern), name)
}

/// Dot entries come first and have no host `DirEntry`.
pub(super) fn matching_entries(
    read: ReadDir,
    pattern: &[char],
) -> impl Iterator<Item = (String, Option<DirEntry>)> + '_ {
    let dots = [".", ".."].into_iter().map(|name| (name.to_owned(), None));
    let children = read.flatten().map(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        (name, Some(entry))
    });
    dots.chain(children)
        .filter(|(name, _)| wildcard(pattern, &name.chars().collect::<Vec<_>>()))
}

fn fill_find_data(entry: &(String, Option<std::fs::Metadata>), data: *mut u8) {
    let (name, meta) = entry;
    // SAFETY: the title's 592-byte WIN32_FIND_DATAW.
    unsafe {
        std::ptr::write_bytes(data, 0, 592);
        if let Some(m) = meta {
            data.cast::<u32>().write_unaligned(attributes_of(m));
            data.add(4)
                .cast::<u64>()
                .write_unaligned(filetime(m.created()));
            data.add(12)
                .cast::<u64>()
                .write_unaligned(filetime(m.accessed()));
            data.add(20)
                .cast::<u64>()
                .write_unaligned(filetime(m.modified()));
            data.add(28)
                .cast::<u32>()
                .write_unaligned((m.len() >> 32) as u32);
            data.add(32).cast::<u32>().write_unaligned(m.len() as u32);
        } else {
            data.cast::<u32>().write_unaligned(FILE_ATTRIBUTE_DIRECTORY);
        }
        let out = data.add(44).cast::<u16>();
        for (i, unit) in name.encode_utf16().enumerate().take(259) {
            out.add(i).write_unaligned(unit);
        }
    }
}

fn find_first(path: Option<PathBuf>, data: *mut u8) -> *mut c_void {
    let Some(path) = path else {
        set_error(ERROR_PATH_NOT_FOUND);
        return invalid_handle();
    };
    let (dir, pattern) = match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => (dir.to_owned(), name.to_string_lossy().into_owned()),
        _ => {
            set_error(ERROR_PATH_NOT_FOUND);
            return invalid_handle();
        }
    };
    let dir = if dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir
    };
    let pattern: Vec<char> = pattern.chars().collect();
    let read = match std::fs::read_dir(&dir) {
        Ok(read) => read,
        Err(e) => {
            set_error(win32_error(&e, &path));
            return invalid_handle();
        }
    };
    let entries: Vec<_> = matching_entries(read, &pattern)
        .map(|(name, entry)| {
            let meta = match entry {
                Some(entry) => entry.metadata(),
                None if name == "." => std::fs::metadata(&dir),
                None => std::fs::metadata(dir.join("..")),
            };
            (name, find_metadata(meta))
        })
        .collect();
    if entries.is_empty() {
        set_error(ERROR_FILE_NOT_FOUND);
        return invalid_handle();
    }
    fill_find_data(&entries[0], data);
    new_handle(Object::Find(Arc::new(Mutex::new(Find {
        entries,
        next: 1,
    }))))
}

pub extern "system" fn FindFirstFileW(path: *const u16, data: *mut u8) -> *mut c_void {
    find_first(wide_path(path), data)
}

pub extern "system" fn FindFirstFileExW(
    path: *const u16,
    _level: u32,
    data: *mut u8,
    _operation: u32,
    _filter: *const c_void,
    _flags: u32,
) -> *mut c_void {
    find_first(wide_path(path), data)
}

pub extern "system" fn FindNextFileW(handle: *mut c_void, data: *mut u8) -> i32 {
    let find = match lock(&HANDLES).get(&(handle as usize)) {
        Some(Object::Find(f)) => Some(f.clone()),
        _ => None,
    };
    let Some(find) = find else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let mut find = lock(&find);
    let Some(entry) = find.entries.get(find.next) else {
        set_error(ERROR_NO_MORE_FILES);
        return 0;
    };
    fill_find_data(entry, data);
    find.next += 1;
    1
}

pub extern "system" fn FindClose(handle: *mut c_void) -> i32 {
    let mut handles = lock(&HANDLES);
    if matches!(handles.get(&(handle as usize)), Some(Object::Find(_))) {
        handles.remove(&(handle as usize));
        1
    } else {
        set_error(ERROR_INVALID_HANDLE);
        0
    }
}

// File mappings: `CreateFileMapping` makes a section over a file (or the
// pagefile) and `MapViewOfFile` makes a view of it.
//
// A view is title memory, not the host's own mapping of the file, because
// everything that reads title memory here - the GPU's page table, the descriptor
// readers, the tracing - knows the ranges `VirtualAlloc` makes and nothing else.
// The cost is one copy per view, which a title that maps a data file pays once
// where it would otherwise fault a page at a time.

/// What `CreateFileMapping` made: a file, or the pagefile, and its size.
pub(crate) struct Section {
    /// `None` for the pagefile-backed section.
    file: Option<Arc<FileObject>>,
    size: u64,
    /// The protection the caller asked for, which a view must fit.
    protect: u32,
    /// The pagefile's memory, made at its first view: every view of a pagefile
    /// section is a window on this one allocation, which is what makes it shared.
    backing: Mutex<Option<usize>>,
}

/// A view of a section, at the address the title was given.
struct View {
    section: Arc<Section>,
    offset: u64,
    len: usize,
    /// The title asked to write through, so the bytes go back to the file.
    write_back: bool,
    /// The view's memory is its own (`VirtualFree` on unmap); a pagefile view is
    /// part of its section's backing instead.
    owned: bool,
}

impl Drop for Section {
    fn drop(&mut self) {
        let Some(backing) = self.backing.lock().ok().and_then(|b| *b) else {
            return;
        };
        super::VirtualFree(backing as *mut c_void, 0, super::MEM_RELEASE);
    }
}

/// An offset `MapViewOfFile` accepts, as Windows requires.
const GRANULARITY: u64 = 64 << 10;
const FILE_MAP_COPY: u32 = 0x1;
const FILE_MAP_WRITE: u32 = 0x2;
const FILE_MAP_READ: u32 = 0x4;
const PAGE_READONLY: u32 = 0x02;
const PAGE_READWRITE: u32 = 0x04;
const PAGE_WRITECOPY: u32 = 0x08;
const PAGE_EXECUTE_READWRITE: u32 = 0x40;
const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
const ERROR_MAPPED_ALIGNMENT: u32 = 1136;

/// Views by address. A list, because two views of one section may land on the
/// same address: a pagefile section's views are windows on one allocation.
static VIEWS: LazyLock<Mutex<HashMap<usize, Vec<View>>>> = LazyLock::new(Default::default);
/// Named sections, so a second `CreateFileMapping` of a name finds the first.
static SECTIONS: LazyLock<Mutex<HashMap<Vec<u16>, Weak<Section>>>> = LazyLock::new(Default::default);

/// Whether a section's protection lets a view write through to the file.
fn writes_through(protect: u32) -> bool {
    matches!(
        protect & 0xFF,
        PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
    )
}

pub(crate) fn section_of(handle: *mut c_void) -> Option<Arc<Section>> {
    match lock(&HANDLES).get(&(handle as usize)) {
        Some(Object::Section(s)) => Some(s.clone()),
        _ => None,
    }
}

/// A section name as the wide string a title passes, `None` when it passes none.
fn wide_name(name: *const u16) -> Option<Vec<u16>> {
    if name.is_null() {
        return None;
    }
    let mut units = Vec::new();
    // SAFETY: the title's NUL-terminated wide string, read up to 32 KiB of units
    // so a missing terminator cannot walk off the mapping.
    unsafe {
        for i in 0..(32 << 10) {
            let unit = *name.add(i);
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
    }
    (!units.is_empty()).then_some(units)
}

/// `CreateFileMappingW`: a section over a file, or over the pagefile for
/// `INVALID_HANDLE_VALUE` and a size. A name that is already taken is the same
/// section, with `ERROR_ALREADY_EXISTS` set, as Windows reports it.
pub extern "system" fn CreateFileMappingW(
    file: *mut c_void,
    _security: *const c_void,
    protect: u32,
    high: u32,
    low: u32,
    name: *const u16,
) -> *mut c_void {
    if !matches!(
        protect & 0xFF,
        PAGE_READONLY
            | PAGE_READWRITE
            | PAGE_WRITECOPY
            | 0x20 // PAGE_EXECUTE_READ
            | PAGE_EXECUTE_READWRITE
            | PAGE_EXECUTE_WRITECOPY
    ) {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let named = wide_name(name);
    let existing = named.as_ref().and_then(|name| {
        let sections = lock(&SECTIONS);
        sections.get(name).and_then(Weak::upgrade)
    });
    if let Some(section) = existing {
        set_error(ERROR_ALREADY_EXISTS);
        return new_handle(Object::Section(section));
    }
    let asked = (u64::from(high) << 32) | u64::from(low);
    let (file, size) = if file as usize == INVALID_HANDLE_VALUE {
        // The pagefile: its size is the caller's to give, since there is no file.
        if asked == 0 {
            set_error(ERROR_INVALID_PARAMETER);
            return std::ptr::null_mut();
        }
        (None, asked)
    } else {
        let Some(object) = file_of(file) else {
            set_error(ERROR_INVALID_HANDLE);
            return std::ptr::null_mut();
        };
        let length = object.file.metadata().map_or(0, |m| m.len());
        (Some(object), if asked == 0 { length } else { asked })
    };
    if size == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let section = Arc::new(Section {
        file,
        size,
        protect,
        backing: Mutex::new(None),
    });
    if let Some(name) = named {
        let mut sections = lock(&SECTIONS);
        sections.retain(|_, s| s.strong_count() > 0);
        sections.insert(name, Arc::downgrade(&section));
    }
    new_handle(Object::Section(section))
}

/// `OpenFileMappingW`: the section a name already made, for a title that maps
/// memory another part of it named.
pub extern "system" fn OpenFileMappingW(_access: u32, _inherit: i32, name: *const u16) -> *mut c_void {
    let Some(name) = wide_name(name) else {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    };
    // The guard is dropped before the miss path takes the lock again.
    let found = lock(&SECTIONS).get(&name).and_then(Weak::upgrade);
    match found {
        Some(section) => new_handle(Object::Section(section)),
        None => {
            lock(&SECTIONS).remove(&name);
            set_error(ERROR_FILE_NOT_FOUND);
            std::ptr::null_mut()
        }
    }
}

fn map_view(
    section: *mut c_void,
    access: u32,
    offset: u64,
    count: usize,
    at: *mut c_void,
) -> *mut c_void {
    let Some(section) = section_of(section) else {
        set_error(ERROR_INVALID_HANDLE);
        return std::ptr::null_mut();
    };
    if offset % GRANULARITY != 0 {
        set_error(ERROR_MAPPED_ALIGNMENT);
        return std::ptr::null_mut();
    }
    if offset >= section.size {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let rest = (section.size - offset) as usize;
    let len = if count == 0 { rest } else { count };
    let write_back = access & FILE_MAP_WRITE != 0 && access & FILE_MAP_COPY == 0;
    if write_back && !writes_through(section.protect) {
        set_error(ERROR_ACCESS_DENIED);
        return std::ptr::null_mut();
    }
    if section.file.is_none() {
        // A pagefile section: the views are windows on the one allocation, so a
        // title that shares memory through it shares the bytes.
        let base = {
            let mut backing = lock(&section.backing);
            match *backing {
                Some(base) => base,
                None => {
                    // SAFETY: title memory, through the allocator every reader of it knows.
                    let base = super::VirtualAlloc(
                        std::ptr::null_mut(),
                        section.size as usize,
                        super::MEM_RESERVE | super::MEM_COMMIT,
                        PAGE_READWRITE,
                    ) as usize;
                    if base == 0 {
                        set_error(ERROR_NOT_ENOUGH_MEMORY);
                        return std::ptr::null_mut();
                    }
                    *backing = Some(base);
                    base
                }
            }
        };
        let view = (base + offset as usize) as *mut c_void;
        lock(&VIEWS).entry(view as usize).or_default().push(View {
            section,
            offset,
            len: len.min(rest),
            write_back,
            owned: false,
        });
        return view;
    }
    // SAFETY: title memory, through the same allocator every reader of title
    // memory knows, so the GPU and the descriptor readers see the view.
    let base = super::VirtualAlloc(
        at,
        len,
        super::MEM_RESERVE | super::MEM_COMMIT,
        PAGE_READWRITE,
    );
    if base.is_null() {
        return base;
    }
    if let Some(file) = &section.file {
        // SAFETY: the view just allocated, `len` bytes and ours to fill.
        let bytes = unsafe { std::slice::from_raw_parts_mut(base.cast::<u8>(), len) };
        let mut filled = 0;
        while filled < len {
            match file.file.read_at(&mut bytes[filled..], offset + filled as u64) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    set_error(win32_error(&e, Path::new("")));
                    // SAFETY: the view this call made.
                    super::VirtualFree(base, 0, super::MEM_RELEASE);
                    return std::ptr::null_mut();
                }
            }
        }
    }
    lock(&VIEWS).entry(base as usize).or_default().push(View {
        section,
        offset,
        len,
        write_back,
        owned: true,
    });
    base
}

/// `MapViewOfFile`: a view of a section at an offset, `count` bytes of it, or the
/// rest of the section when `count` is 0.
pub extern "system" fn MapViewOfFile(
    section: *mut c_void,
    access: u32,
    high: u32,
    low: u32,
    count: usize,
) -> *mut c_void {
    map_view(
        section,
        access,
        (u64::from(high) << 32) | u64::from(low),
        count,
        std::ptr::null_mut(),
    )
}

/// `MapViewOfFileEx`: as [`MapViewOfFile`], at `at` if that address is free.
pub extern "system" fn MapViewOfFileEx(
    section: *mut c_void,
    access: u32,
    high: u32,
    low: u32,
    count: usize,
    at: *mut c_void,
) -> *mut c_void {
    map_view(
        section,
        access,
        (u64::from(high) << 32) | u64::from(low),
        count,
        at,
    )
}

/// Writes a write-through view's bytes back to the file, clamped to what the
/// file had: Windows keeps the bytes past its end in memory only.
fn write_back(view: &View, base: usize) {
    let Some(file) = &view.section.file else {
        return;
    };
    let length = file.file.metadata().map_or(0, |m| m.len());
    let Some(after) = length.checked_sub(view.offset) else {
        return;
    };
    let len = view.len.min(after as usize);
    if len == 0 {
        return;
    }
    // SAFETY: the view is still mapped, `view.len` bytes, and `len` of them.
    let bytes = unsafe { std::slice::from_raw_parts(base as *const u8, len) };
    let mut done = 0;
    while done < len {
        match file.file.write_at(&bytes[done..], view.offset + done as u64) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

/// `FlushViewOfFile`: the file gets what the view holds now, rather than at the
/// unmap. `FlushFileBuffers` on the file would be the caller's next call.
pub extern "system" fn FlushViewOfFile(base: *const c_void, _count: usize) -> i32 {
    let views = lock(&VIEWS);
    let Some(list) = views.get(&(base as usize)) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    for view in list {
        if view.write_back {
            write_back(view, base as usize);
        }
    }
    1
}

/// `UnmapViewOfFile`: the view's bytes go back to the file if it writes through,
/// and the memory is given up.
pub extern "system" fn UnmapViewOfFile(base: *const c_void) -> i32 {
    let mut views = lock(&VIEWS);
    let Some(list) = views.get_mut(&(base as usize)) else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let Some(view) = list.pop() else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    if list.is_empty() {
        views.remove(&(base as usize));
    }
    drop(views);
    if view.write_back {
        write_back(&view, base as usize);
    }
    if view.owned {
        super::VirtualFree(base as *mut c_void, 0, super::MEM_RELEASE);
    }
    1
}

// Completion ports: a finished overlapped operation is a packet a worker asks for.

/// A packet a completion port holds: what finished, its key, and the title's
/// OVERLAPPED, which is how the title knows which request it was.
pub(crate) struct Packet {
    bytes: usize,
    key: usize,
    /// The title's OVERLAPPED, as an address: a packet crosses threads, and a
    /// pointer is not `Send`.
    overlapped: usize,
    /// 0 for a completed read or write, `STATUS_END_OF_FILE` for a read that
    /// reached the end, which `GetQueuedCompletionStatus` reports as a failure.
    status: usize,
}

/// A completion port: finished packets, and the workers waiting for one.
pub(crate) struct Port {
    state: Mutex<PortState>,
    ready: Condvar,
}

struct PortState {
    packets: VecDeque<Packet>,
}

const INFINITE: u32 = 0xFFFF_FFFF;
const ERROR_TIMEOUT: u32 = 1460;
/// `OVERLAPPED_ENTRY`: key, OVERLAPPED, Internal, byte count.
const OVERLAPPED_ENTRY: usize = 4 * std::mem::size_of::<usize>();

impl Port {
    fn new() -> Self {
        Self {
            state: Mutex::new(PortState {
                packets: VecDeque::new(),
            }),
            ready: Condvar::new(),
        }
    }

    fn post(&self, packet: Packet) {
        lock(&self.state).packets.push_back(packet);
        self.ready.notify_one();
        super::sync::signalled();
    }

    /// The next packet, waiting up to `ms` (`INFINITE` for as long as it takes).
    fn take(&self, ms: u32) -> Option<Packet> {
        let deadline = (ms != INFINITE).then(|| Instant::now() + Duration::from_millis(u64::from(ms)));
        let mut state = lock(&self.state);
        loop {
            if let Some(packet) = state.packets.pop_front() {
                return Some(packet);
            }
            match deadline {
                None => state = self.ready.wait(state).ok()?,
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    state = self.ready.wait_timeout(state, deadline - now).ok()?.0;
                }
            }
        }
    }
}

pub(crate) fn port_of(handle: *mut c_void) -> Option<Arc<Port>> {
    match lock(&HANDLES).get(&(handle as usize)) {
        Some(Object::Port(p)) => Some(p.clone()),
        _ => None,
    }
}

impl FileObject {
    /// Reports a finished overlapped operation to the port this handle reports to,
    /// which is what a title waiting on a port instead of on an event asks for.
    fn post_completion(&self, bytes: usize, overlapped: *mut u8, status: usize) {
        if overlapped.is_null() {
            return;
        }
        let association = lock(&self.port).clone();
        let Some((port, key)) = association else {
            return;
        };
        port.post(Packet {
            bytes,
            key,
            overlapped: overlapped as usize,
            status,
        });
    }
}

/// `CreateIoCompletionPort`: a new port, or this file associated with an existing
/// one, which is then returned as it was (Windows hands back the same handle).
pub extern "system" fn CreateIoCompletionPort(
    file: *mut c_void,
    port: *mut c_void,
    key: usize,
    _threads: u32,
) -> *mut c_void {
    if port.is_null() {
        if file as usize != INVALID_HANDLE_VALUE {
            set_error(ERROR_INVALID_PARAMETER);
            return std::ptr::null_mut();
        }
        return new_handle(Object::Port(Arc::new(Port::new())));
    }
    let Some(existing) = port_of(port) else {
        set_error(ERROR_INVALID_HANDLE);
        return std::ptr::null_mut();
    };
    if file as usize != INVALID_HANDLE_VALUE {
        let Some(object) = file_of(file) else {
            set_error(ERROR_INVALID_HANDLE);
            return std::ptr::null_mut();
        };
        *lock(&object.port) = Some((existing, key));
    }
    port
}

/// `PostQueuedCompletionStatus`: a packet of the caller's own making, which is how
/// a title tells a worker to stop.
pub extern "system" fn PostQueuedCompletionStatus(
    port: *mut c_void,
    bytes: u32,
    key: usize,
    overlapped: *mut u8,
) -> i32 {
    let Some(port) = port_of(port) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    port.post(Packet {
        bytes: bytes as usize,
        key,
        overlapped: overlapped as usize,
        status: 0,
    });
    1
}

/// `GetQueuedCompletionStatus`: the next finished packet, waiting up to `ms`.
pub extern "system" fn GetQueuedCompletionStatus(
    port: *mut c_void,
    bytes: *mut u32,
    key: *mut usize,
    overlapped: *mut *mut u8,
    ms: u32,
) -> i32 {
    let Some(port) = port_of(port) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let Some(packet) = port.take(ms) else {
        set_error(ERROR_TIMEOUT);
        return 0;
    };
    // SAFETY: the title's out-pointers, all of which it may leave null.
    unsafe {
        if !bytes.is_null() {
            bytes.write(packet.bytes as u32);
        }
        if !key.is_null() {
            key.write(packet.key);
        }
        if !overlapped.is_null() {
            overlapped.write(packet.overlapped as *mut u8);
        }
    }
    if packet.status != 0 {
        set_error(ERROR_HANDLE_EOF);
        return 0;
    }
    1
}

/// `GetQueuedCompletionStatusEx`: up to `count` packets at once, one wait.
pub extern "system" fn GetQueuedCompletionStatusEx(
    port: *mut c_void,
    entries: *mut u8,
    count: u32,
    removed: *mut u32,
    ms: u32,
    _alertable: i32,
) -> i32 {
    let Some(port) = port_of(port) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    if entries.is_null() || count == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Some(first) = port.take(ms) else {
        set_error(ERROR_TIMEOUT);
        return 0;
    };
    let mut taken = vec![first];
    // Whatever else is already finished comes too, without another wait.
    {
        let mut state = lock(&port.state);
        while (taken.len() as u32) < count {
            match state.packets.pop_front() {
                Some(packet) => taken.push(packet),
                None => break,
            }
        }
    }
    let mut failed = false;
    for (i, packet) in taken.iter().enumerate() {
        // SAFETY: the title's array of `count` OVERLAPPED_ENTRY.
        unsafe {
            let entry = entries.add(i * OVERLAPPED_ENTRY);
            entry.cast::<usize>().write(packet.key);
            entry
                .add(8)
                .cast::<usize>()
                .write(packet.overlapped as usize);
            entry.add(16).cast::<usize>().write(packet.status);
            entry.add(24).cast::<usize>().write(packet.bytes);
        }
        failed |= packet.status != 0;
    }
    // SAFETY: the title's out word.
    unsafe { removed.write(taken.len() as u32) };
    if failed {
        set_error(ERROR_HANDLE_EOF);
    }
    i32::from(!failed)
}

/// Names this module answers for.
pub(super) fn lookup(name: &str) -> Option<usize> {
    macro_rules! table {
        ($($f:ident),* $(,)?) => {
            match name {
                $(stringify!($f) => Some($f as *const () as usize),)*
                _ => None,
            }
        };
    }
    table!(
        CreateFileW,
        CreateFileA,
        GetStdHandle,
        ReadFile,
        WriteFile,
        GetOverlappedResult,
        SetFilePointer,
        SetFilePointerEx,
        GetFileSize,
        GetFileSizeEx,
        SetEndOfFile,
        FlushFileBuffers,
        GetFileAttributesW,
        GetFileAttributesA,
        GetFileAttributesExW,
        DeleteFileW,
        CreateDirectoryW,
        RemoveDirectoryW,
        FindFirstFileW,
        FindFirstFileExW,
        FindNextFileW,
        FindClose,
        CreateFileMappingW,
        OpenFileMappingW,
        MapViewOfFile,
        MapViewOfFileEx,
        UnmapViewOfFile,
        FlushViewOfFile,
        CreateIoCompletionPort,
        PostQueuedCompletionStatus,
        GetQueuedCompletionStatus,
        GetQueuedCompletionStatusEx,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// File calls report what Windows would: a missing folder apart from a missing file, and
    /// running out of descriptors apart from a refusal (`ERROR_ACCESS_DENIED` is the rest).
    #[test]
    fn host_errors_reach_file_calls_as_windows_reports_them() {
        let missing = io::Error::from_raw_os_error(2);
        let here = std::env::temp_dir().join("victory-no-such-file");
        assert_eq!(win32_error(&missing, &here), ERROR_FILE_NOT_FOUND);
        let nowhere = Path::new("/victory-no-such-folder/file");
        assert_eq!(win32_error(&missing, nowhere), ERROR_PATH_NOT_FOUND);
        let code = |errno| win32_error(&io::Error::from_raw_os_error(errno), &here);
        assert_eq!(code(24), 4, "EMFILE: ERROR_TOO_MANY_OPEN_FILES");
        assert_eq!(code(66), 145, "ENOTEMPTY: ERROR_DIR_NOT_EMPTY, as before");
        assert_eq!(code(17), 80, "EEXIST: ERROR_FILE_EXISTS, as before");
        assert_eq!(code(5), ERROR_ACCESS_DENIED, "EIO: the rest, as before");
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    #[test]
    fn a_drive_path_goes_under_its_folder_and_dotdot_stops_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("save.dat"), [7u8; 100]).unwrap();
        let letter = mount(dir.path()).unwrap();
        assert_eq!(mount(dir.path()), Some(letter));
        let root = std::path::absolute(dir.path()).unwrap();
        let path = |s: &str| translate(&s.encode_utf16().collect::<Vec<_>>()).unwrap();
        assert_eq!(
            path(&format!("{letter}:\\a\\b.txt")),
            root.join("a").join("b.txt")
        );
        assert_eq!(path(&format!("{letter}:/../../x")), root.join("x"));
        assert_eq!(path(&format!("\\\\?\\{letter}:\\")), root);
        assert_eq!(path("/tmp\\x"), PathBuf::from("/tmp/x"));

        let mut attributes = [0u8; 36];
        let p = wide(&format!("{letter}:\\save.dat"));
        assert_ne!(
            GetFileAttributesExW(p.as_ptr(), 0, attributes.as_mut_ptr()),
            0
        );
        let size = u32::from_le_bytes(attributes[32..36].try_into().unwrap());
        assert_eq!(size, 100);
    }

    #[test]
    fn find_first_only_reads_metadata_for_matching_names() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..999 {
            File::create(dir.path().join(format!("{i}.bin"))).unwrap();
        }
        std::fs::write(dir.path().join("slot.sav"), b"saved").unwrap();

        let pattern = wide(dir.path().join("*.sav").to_str().unwrap());
        let mut data = [0u8; 592];
        FIND_METADATA_LOOKUPS.set(0);
        let handle = FindFirstFileW(pattern.as_ptr(), data.as_mut_ptr());
        assert_ne!(handle, invalid_handle());
        assert_eq!(FIND_METADATA_LOOKUPS.get(), 1);
        let name: Vec<u16> = data[44..]
            .chunks_exact(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .take_while(|&unit| unit != 0)
            .collect();
        assert_eq!(String::from_utf16(&name).unwrap(), "slot.sav");
        assert_eq!(u32::from_le_bytes(data[28..32].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(data[32..36].try_into().unwrap()), 5);
        assert_eq!(FindNextFileW(handle, data.as_mut_ptr()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_NO_MORE_FILES);
        assert_eq!(FindClose(handle), 1);

        let none = wide(dir.path().join("*.zzz").to_str().unwrap());
        FIND_METADATA_LOOKUPS.set(0);
        assert_eq!(
            FindFirstFileW(none.as_ptr(), data.as_mut_ptr()),
            invalid_handle()
        );
        assert_eq!(super::super::GetLastError(), ERROR_FILE_NOT_FOUND);
        assert_eq!(FIND_METADATA_LOOKUPS.get(), 0);
    }

    #[test]
    fn wildcards_match_like_find_first_file() {
        let m = |p: &str, n: &str| {
            wildcard(
                &p.chars().collect::<Vec<_>>(),
                &n.chars().collect::<Vec<_>>(),
            )
        };
        assert!(m("*", "anything"));
        assert!(m("*.TXT", "a.txt"));
        assert!(m("a?c", "abc"));
        assert!(!m("a?c", "ac"));
        assert!(!m("*.bin", "a.txt"));
    }

    #[test]
    fn wildcards_follow_the_dos_rules_for_dots() {
        // What FindFirstFile on Windows gives for these long names (short 8.3 names, which
        // the host has none of, can add more matches there).
        let names = [
            ".", "..", "a", "aa", "a.a", "a..a", "a.a.a", ".a", "readme", "x.txt",
        ];
        let matches = |p: &str| -> Vec<&str> {
            let p: Vec<char> = p.chars().collect();
            names
                .iter()
                .copied()
                .filter(|n| wildcard(&p, &n.chars().collect::<Vec<_>>()))
                .collect()
        };
        // `*.*` is every name, with or without a dot.
        assert_eq!(matches("*.*"), names);
        // `name.*` also matches the name without an extension.
        assert_eq!(matches("aa.*"), ["aa"]);
        assert_eq!(matches("README.*"), ["readme"]);
        // A final `.` asks for names without an extension.
        assert_eq!(matches("*."), [".", "..", "a", "aa", "readme"]);
        assert_eq!(matches("a*."), ["a", "aa"]);
        // `?` matches one character, or none at a dot or the end.
        assert_eq!(matches("?"), [".", "..", "a"]);
        assert_eq!(matches("??."), [".", "..", "a", "aa"]);
        assert_eq!(matches("?.??."), [".", "..", "a", "a.a", ".a"]);
        // Literal dots and `*` elsewhere are unchanged.
        assert_eq!(matches("*.a"), ["a.a", "a..a", "a.a.a", ".a"]);
        assert_eq!(matches("*..*"), [".", "..", "a..a"]);
        assert_eq!(matches("*.TXT"), ["x.txt"]);
        // The DOS wildcards may be written directly.
        assert_eq!(matches("<.a"), ["a.a", "a..a", "a.a.a", ".a"]);
        assert_eq!(matches(r#"a"*"a"#), ["a..a", "a.a.a"]);
    }

    fn mapped_file(bytes: &[u8]) -> (tempfile::TempDir, PathBuf, *mut c_void) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pak.bin");
        std::fs::write(&path, bytes).unwrap();
        let name = wide(path.to_str().unwrap());
        let handle = CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        assert_ne!(handle, invalid_handle());
        (dir, path, handle)
    }

    /// A view holds the file's bytes at its offset, reads the whole rest of the
    /// file when asked for 0 bytes, and closing the section leaves the view alive.
    #[test]
    fn a_view_holds_the_file_at_its_offset() {
        let pattern: Vec<u8> = (0..(3 * 64 * 1024)).map(|i| (i % 251) as u8).collect();
        let (_dir, path, file) = mapped_file(&pattern);
        let section = CreateFileMappingW(
            file,
            std::ptr::null(),
            PAGE_READONLY,
            0,
            0,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        set_error(0);
        assert_eq!(super::super::CloseHandle(file), 1, "the section keeps the file");

        let view = MapViewOfFile(section, FILE_MAP_READ, 0, 0, 0);
        assert!(!view.is_null());
        // SAFETY: the view of the whole file this call made.
        let seen = unsafe { std::slice::from_raw_parts(view.cast::<u8>(), pattern.len()) };
        assert_eq!(seen, &pattern[..]);

        // A view at the second 64 KiB sees the file from there.
        let later = MapViewOfFile(section, FILE_MAP_READ, 0, 64 * 1024, 4096);
        assert!(!later.is_null());
        // SAFETY: 4096 bytes of the view this call made.
        let seen = unsafe { std::slice::from_raw_parts(later.cast::<u8>(), 4096) };
        assert_eq!(seen, &pattern[64 * 1024..64 * 1024 + 4096]);

        assert_eq!(UnmapViewOfFile(later), 1);
        assert_eq!(UnmapViewOfFile(view), 1);
        assert_eq!(super::super::CloseHandle(section), 1);
        assert_eq!(std::fs::read(&path).unwrap(), pattern);
    }

    /// A view that writes through puts its bytes back in the file when it is
    /// unmapped or flushed, and not past the file's end.
    #[test]
    fn a_write_view_writes_through_to_the_file() {
        let (_dir, path, file) = mapped_file(&[0u8; 128 * 1024]);
        let section = CreateFileMappingW(
            file,
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            0,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 0);
        assert!(!view.is_null());
        assert_eq!(super::super::GetLastError(), 0);
        // SAFETY: the view of the whole file.
        let bytes = unsafe { std::slice::from_raw_parts_mut(view.cast::<u8>(), 128 * 1024) };
        bytes[0] = 0xAA;
        bytes[70 * 1024] = 0xBB;
        assert_eq!(FlushViewOfFile(view, 0), 1);
        assert_eq!(std::fs::read(&path).unwrap()[70 * 1024], 0xBB, "flush");
        bytes[1] = 0xCC;
        assert_eq!(UnmapViewOfFile(view), 1);
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!((on_disk[0], on_disk[1]), (0xAA, 0xCC), "unmap");
        assert_eq!(on_disk.len(), 128 * 1024, "the file did not grow");
        assert_eq!(super::super::CloseHandle(section), 1);
    }

    /// The refusals: a view may not write a read-only section, an offset has to be
    /// a multiple of 64 KiB, and a range outside the section is no view.
    #[test]
    fn a_view_that_cannot_be_made_is_refused_with_reasons() {
        let (_dir, _path, file) = mapped_file(&[1u8; 4096]);
        let read_only = CreateFileMappingW(
            file,
            std::ptr::null(),
            PAGE_READONLY,
            0,
            0,
            std::ptr::null(),
        );
        assert!(MapViewOfFile(read_only, FILE_MAP_WRITE, 0, 0, 0).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_ACCESS_DENIED);
        assert!(MapViewOfFile(read_only, FILE_MAP_READ, 0, 4096, 0).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_MAPPED_ALIGNMENT);
        assert!(MapViewOfFile(read_only, FILE_MAP_READ, 0, 64 * 1024, 0).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        // Past the file's end is zero, as the last page of a view is on Windows.
        let past = MapViewOfFile(read_only, FILE_MAP_READ, 0, 0, 64 * 1024);
        assert!(!past.is_null());
        // SAFETY: the view of the whole 64 KiB.
        let seen = unsafe { std::slice::from_raw_parts(past.cast::<u8>(), 64 * 1024) };
        assert_eq!(&seen[..4096], &[1u8; 4096]);
        assert!(seen[4096..].iter().all(|&b| b == 0));
        assert_eq!(UnmapViewOfFile(past), 1);
        assert!(MapViewOfFile(std::ptr::null_mut(), FILE_MAP_READ, 0, 0, 0).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_HANDLE);
        assert_eq!(UnmapViewOfFile(std::ptr::null()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(super::super::CloseHandle(read_only), 1);
        assert_eq!(super::super::CloseHandle(file), 1);
    }

    /// The pagefile section: zeroed, sized by the caller, one memory however many
    /// views are mapped, and found again by its name.
    #[test]
    fn the_pagefile_section_is_shared_by_its_views_and_its_name() {
        let name = wide(r"Local\victory-pagefile-test");
        let size = 128 * 1024;
        let section = CreateFileMappingW(
            invalid_handle(),
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            size as u32,
            name.as_ptr(),
        );
        assert!(!section.is_null());
        assert_eq!(super::super::GetLastError(), 0);

        let first = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 0);
        // SAFETY: the view of the whole section.
        let bytes = unsafe { std::slice::from_raw_parts_mut(first.cast::<u8>(), size as usize) };
        assert!(bytes.iter().all(|&b| b == 0), "a fresh section is zeroed");
        bytes[1234] = 7;

        let again = CreateFileMappingW(
            invalid_handle(),
            std::ptr::null(),
            PAGE_READWRITE,
            0,
            size as u32,
            name.as_ptr(),
        );
        assert!(!again.is_null());
        assert_eq!(super::super::GetLastError(), ERROR_ALREADY_EXISTS);
        let opened = OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr());
        assert!(!opened.is_null());

        let second = MapViewOfFile(opened, FILE_MAP_READ, 0, 0, 0);
        // SAFETY: the view of the whole section.
        let seen = unsafe { std::slice::from_raw_parts(second.cast::<u8>(), size as usize) };
        assert_eq!(seen[1234], 7, "two views, one memory");

        assert_eq!(UnmapViewOfFile(second), 1);
        assert_eq!(UnmapViewOfFile(first), 1);
        for handle in [section, again, opened] {
            assert_eq!(super::super::CloseHandle(handle), 1);
        }
        assert!(OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_FILE_NOT_FOUND);
    }

    fn port() -> *mut c_void {
        let handle = CreateIoCompletionPort(invalid_handle(), std::ptr::null_mut(), 0, 0);
        assert!(!handle.is_null());
        handle
    }

    fn overlapped_for(offset: u64, event: *mut c_void) -> Vec<u8> {
        let mut ov = vec![0u8; 32];
        ov[16..24].copy_from_slice(&offset.to_le_bytes());
        ov[24..32].copy_from_slice(&(event as usize).to_le_bytes());
        ov
    }

    /// A packet the caller posted comes back with its key, byte count and
    /// OVERLAPPED, and a port with nothing finished times out instead.
    #[test]
    fn a_posted_packet_comes_back_and_an_empty_port_times_out() {
        let port = port();
        let mut request = [0u8; 32];
        assert_eq!(
            PostQueuedCompletionStatus(port, 512, 0x1234, request.as_mut_ptr()),
            1
        );
        let (mut bytes, mut key) = (0u32, 0usize);
        let mut got: *mut u8 = std::ptr::null_mut();
        assert_eq!(
            GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut got, 1000),
            1
        );
        assert_eq!((bytes, key, got), (512, 0x1234, request.as_mut_ptr()));

        set_error(0);
        assert_eq!(
            GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut got, 10),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_TIMEOUT);
        assert_eq!(super::super::CloseHandle(port), 1);
    }

    /// A file associated with a port reports its finished overlapped read there:
    /// the byte count, the key it was associated with, and the title's OVERLAPPED.
    #[test]
    fn an_overlapped_read_reports_to_the_port_the_file_is_associated_with() {
        let (_dir, _path, file) = mapped_file(&[9u8; 4096]);
        let port = port();
        assert_eq!(
            CreateIoCompletionPort(file, port, 7, 1),
            port,
            "the same port handle comes back"
        );

        let mut buffer = [0u8; 64];
        let mut ov = overlapped_for(128, std::ptr::null_mut());
        let mut read = 0u32;
        assert_eq!(
            ReadFile(file, buffer.as_mut_ptr(), 64, &mut read, ov.as_mut_ptr()),
            1
        );
        let (mut bytes, mut key) = (0u32, 0usize);
        let mut got: *mut u8 = std::ptr::null_mut();
        assert_eq!(
            GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut got, 1000),
            1
        );
        assert_eq!((bytes, key, got), (64, 7, ov.as_mut_ptr()));

        // Read to the end: the packet says so, and the call reports the failure.
        let mut ov = overlapped_for(4096, std::ptr::null_mut());
        assert_eq!(
            ReadFile(file, buffer.as_mut_ptr(), 64, &mut read, ov.as_mut_ptr()),
            0
        );
        assert_eq!(
            GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut got, 1000),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_HANDLE_EOF);
        assert_eq!((bytes, got), (0, ov.as_mut_ptr()));

        assert_eq!(super::super::CloseHandle(file), 1);
        assert_eq!(super::super::CloseHandle(port), 1);
    }

    /// A file with no port posts nothing, and the batch form hands over what is
    /// already finished in one call.
    #[test]
    fn a_file_without_a_port_posts_nothing_and_the_batch_form_drains() {
        let (_dir, _path, file) = mapped_file(&[3u8; 1024]);
        let mut buffer = [0u8; 16];
        let mut ov = overlapped_for(0, std::ptr::null_mut());
        let mut read = 0u32;
        assert_eq!(
            ReadFile(file, buffer.as_mut_ptr(), 16, &mut read, ov.as_mut_ptr()),
            1
        );
        assert_eq!(read, 16);

        let port = port();
        let (mut bytes, mut key) = (0u32, 0usize);
        let mut got: *mut u8 = std::ptr::null_mut();
        assert_eq!(
            GetQueuedCompletionStatus(port, &mut bytes, &mut key, &mut got, 20),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_TIMEOUT);

        for (count, key) in [(10u32, 1usize), (20, 2)] {
            assert_eq!(
                PostQueuedCompletionStatus(port, count, key, std::ptr::null_mut()),
                1
            );
        }
        let mut entries = [0u8; 4 * 32];
        let mut removed = 0u32;
        assert_eq!(
            GetQueuedCompletionStatusEx(port, entries.as_mut_ptr(), 2, &mut removed, 1000, 0),
            1
        );
        assert_eq!(removed, 2);
        // SAFETY: two OVERLAPPED_ENTRY of the title's array.
        let first = unsafe { entries.as_ptr().cast::<usize>().read_unaligned() };
        let second = unsafe { entries.as_ptr().add(32).cast::<usize>().read_unaligned() };
        assert_eq!((first, second), (1, 2), "the key field of each entry");
        assert_eq!(super::super::CloseHandle(port), 1);
        assert_eq!(super::super::CloseHandle(file), 1);
    }

    #[test]
    fn creating_a_read_only_handle_does_not_grant_write_access() {
        for disposition in [OPEN_ALWAYS, CREATE_NEW, CREATE_ALWAYS] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("save.dat");
            let name = wide(path.to_str().unwrap());
            let null = std::ptr::null_mut();
            let h = CreateFileW(name.as_ptr(), GENERIC_READ, 0, null, disposition, 0, null);
            assert_ne!(h, invalid_handle());
            assert_eq!(super::super::GetLastError(), 0);
            let mut count = 99;
            assert_eq!(WriteFile(h, b"x".as_ptr(), 1, &mut count, null.cast()), 0);
            assert_eq!(super::super::GetLastError(), ERROR_ACCESS_DENIED);
            assert_eq!(count, 0);
            assert_eq!(SetFilePointerEx(h, 3, std::ptr::null_mut(), 0), 1);
            set_error(0);
            assert_eq!(SetEndOfFile(h), 0);
            assert_eq!(super::super::GetLastError(), ERROR_ACCESS_DENIED);
            assert_eq!(super::super::CloseHandle(h), 1);
            assert!(std::fs::read(path).unwrap().is_empty());
        }
    }

    #[test]
    fn open_always_respects_access_when_reopening_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("save.dat");
        let name = wide(path.to_str().unwrap());
        let null = std::ptr::null_mut();
        let open = |access| CreateFileW(name.as_ptr(), access, 0, null, OPEN_ALWAYS, 0, null);

        let h = open(GENERIC_READ | GENERIC_WRITE);
        assert_ne!(h, invalid_handle());
        let mut count = 0;
        assert_eq!(WriteFile(h, b"save".as_ptr(), 4, &mut count, null.cast()), 1);
        assert_eq!(count, 4);
        assert_eq!(super::super::CloseHandle(h), 1);

        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();
        let h = open(GENERIC_READ);
        assert_ne!(h, invalid_handle());
        assert_eq!(super::super::GetLastError(), ERROR_ALREADY_EXISTS);
        let mut data = [0; 4];
        assert_eq!(ReadFile(h, data.as_mut_ptr(), 4, &mut count, null.cast()), 1);
        assert_eq!(count, 4);
        assert_eq!(&data, b"save");
        assert_eq!(WriteFile(h, b"!".as_ptr(), 1, &mut count, null.cast()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_ACCESS_DENIED);
        assert_eq!(count, 0);
        assert_eq!(super::super::CloseHandle(h), 1);
        assert_eq!(std::fs::read(path).unwrap(), b"save");
    }

    #[test]
    fn truncate_existing_requires_generic_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("save.dat");
        std::fs::write(&path, b"save").unwrap();
        let name = wide(path.to_str().unwrap());
        let truncate = |access| {
            let null = std::ptr::null_mut();
            CreateFileW(name.as_ptr(), access, 0, null, TRUNCATE_EXISTING, 0, null)
        };

        for access in [GENERIC_READ, 0, FILE_WRITE_DATA, FILE_APPEND_DATA] {
            let handle = truncate(access);
            assert_eq!(std::fs::read(&path).unwrap(), b"save");
            assert_eq!(handle, invalid_handle());
            assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        }

        for access in [GENERIC_WRITE, GENERIC_ALL] {
            std::fs::write(&path, b"save").unwrap();
            let handle = truncate(access);
            assert_ne!(handle, invalid_handle(), "access {access:#x}");
            assert_eq!(super::super::CloseHandle(handle), 1);
            assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        }
    }

    /// `FILE_APPEND_DATA` alone writes at the end of the file, whatever the file pointer says.
    #[test]
    fn append_only_access_writes_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("log.txt"), b"abc").unwrap();
        let letter = mount(dir.path()).unwrap();
        let path = wide(&format!("{letter}:\\log.txt"));
        let open = |access| {
            let null = std::ptr::null_mut();
            CreateFileW(path.as_ptr(), access, 0, null, OPEN_EXISTING, 0, null)
        };
        let h = open(FILE_APPEND_DATA);
        assert_ne!(h, invalid_handle());
        let mut count = 0u32;
        let null = std::ptr::null_mut();
        assert_eq!(SetEndOfFile(h), 0);
        assert_eq!(super::super::GetLastError(), ERROR_ACCESS_DENIED);
        assert_eq!(WriteFile(h, b"de".as_ptr(), 2, &mut count, null), 1);
        assert_eq!(WriteFile(h, b"f".as_ptr(), 1, &mut count, null), 1);
        assert_eq!(super::super::CloseHandle(h), 1);
        assert_eq!(std::fs::read(dir.path().join("log.txt")).unwrap(), b"abcdef");
        // Write access as well: at the file pointer, as before.
        let h = open(FILE_WRITE_DATA | FILE_APPEND_DATA);
        assert_eq!(WriteFile(h, b"X".as_ptr(), 1, &mut count, null), 1);
        assert_eq!(super::super::CloseHandle(h), 1);
        assert_eq!(std::fs::read(dir.path().join("log.txt")).unwrap(), b"Xbcdef");
    }

    #[test]
    fn concurrent_synchronous_writes_keep_both_records() {
        const RECORD_LEN: usize = 4096;
        const ROUNDS: usize = 64;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("records.bin");
        let handle = create(Some(path.clone()), GENERIC_WRITE, CREATE_ALWAYS, 0);
        assert_ne!(handle, invalid_handle());
        let handle_id = handle as usize;
        let start = std::sync::Barrier::new(2);
        let first = [b'A'; RECORD_LEN];
        let second = [b'B'; RECORD_LEN];

        let write_records = |record: &[u8; RECORD_LEN]| {
            let mut all_written = true;
            for _ in 0..ROUNDS {
                start.wait();
                let mut count = 0;
                let result = WriteFile(
                    handle_id as *mut c_void,
                    record.as_ptr(),
                    RECORD_LEN as u32,
                    &mut count,
                    std::ptr::null_mut(),
                );
                all_written &= result == 1 && count == RECORD_LEN as u32;
            }
            all_written
        };
        std::thread::scope(|scope| {
            let writer_a = scope.spawn(|| write_records(&first));
            let writer_b = scope.spawn(|| write_records(&second));
            assert!(writer_a.join().unwrap());
            assert!(writer_b.join().unwrap());
        });

        assert_eq!(super::super::CloseHandle(handle), 1);
        let data = std::fs::read(path).unwrap();
        assert_eq!(data.len(), ROUNDS * 2 * RECORD_LEN);
        for (round, pair) in data.chunks_exact(2 * RECORD_LEN).enumerate() {
            let (left, right) = pair.split_at(RECORD_LEN);
            assert!(
                (left == first && right == second) || (left == second && right == first),
                "round {round} lost or corrupted a record"
            );
        }
    }

    #[test]
    fn overlapped_reads_report_eof_only_when_a_nonempty_request_reads_nothing() {
        use super::super::{
            CloseHandle, CreateEventW, GetLastError, ResetEvent, WAIT_OBJECT_0, WaitForSingleObject,
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read.bin");
        std::fs::write(&path, b"1234tail").unwrap();
        let handle = create(Some(path), GENERIC_READ, OPEN_EXISTING, 0);
        assert_ne!(handle, invalid_handle());
        let event = CreateEventW(std::ptr::null(), 1, 0, std::ptr::null());
        assert!(!event.is_null());
        // OVERLAPPED: Internal, InternalHigh, Offset/OffsetHigh, hEvent.
        let mut ov = [0, 0, 4, event as usize];
        let ov_ptr = (&raw mut ov).cast();
        let mut buffer = [0; 16];
        let mut count = 99;
        assert_eq!(
            ReadFile(handle, buffer.as_mut_ptr(), 16, &mut count, ov_ptr),
            1
        );
        assert_eq!(count, 4);
        assert_eq!(&buffer[..4], b"tail");
        count = 99;
        assert_eq!(GetOverlappedResult(handle, ov_ptr, &mut count, 0), 1);
        assert_eq!(count, 4);

        for offset in [8, 9] {
            assert_eq!(ResetEvent(event), 1);
            ov[2] = offset;
            count = 99;
            assert_eq!(
                ReadFile(handle, buffer.as_mut_ptr(), 16, &mut count, ov_ptr),
                0,
                "offset {offset}"
            );
            assert_eq!(GetLastError(), 38);
            assert_eq!(count, 0);
            assert_eq!(&ov[..2], &[0xC000_0011, 0]);
            assert_eq!(WaitForSingleObject(event, 0), WAIT_OBJECT_0);

            set_error(0);
            count = 99;
            assert_eq!(GetOverlappedResult(handle, ov_ptr, &mut count, 0), 0);
            assert_eq!(GetLastError(), 38);
            assert_eq!(count, 0);
        }

        ov[2] = 8;
        count = 99;
        assert_eq!(
            ReadFile(handle, std::ptr::null_mut(), 0, &mut count, ov_ptr),
            1
        );
        assert_eq!(count, 0);
        assert_eq!(&ov[..2], &[0, 0]);
        assert_eq!(CloseHandle(event), 1);
        assert_eq!(CloseHandle(handle), 1);
    }

    #[test]
    fn synchronous_read_at_eof_succeeds_with_zero_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("read.bin");
        std::fs::write(&path, b"data").unwrap();
        let handle = create(Some(path), GENERIC_READ, OPEN_EXISTING, 0);
        assert_ne!(handle, invalid_handle());
        assert_eq!(SetFilePointer(handle, 0, std::ptr::null_mut(), 2), 4);
        let mut buffer = [0; 4];
        let mut count = 99;
        assert_eq!(
            ReadFile(
                handle,
                buffer.as_mut_ptr(),
                4,
                &mut count,
                std::ptr::null_mut()
            ),
            1
        );
        assert_eq!(count, 0);
        assert_eq!(super::super::CloseHandle(handle), 1);
    }

    #[test]
    fn zero_length_null_io_and_find_close_handle_checks() {
        let dir = tempfile::tempdir().unwrap();
        let letter = mount(dir.path()).unwrap();
        let path = wide(&format!("{letter}:\\io.bin"));
        let h = CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            std::ptr::null(),
            CREATE_ALWAYS,
            0,
            std::ptr::null_mut(),
        );
        assert_ne!(h, invalid_handle());

        // Zero-length reads and writes with a null buffer succeed without UB.
        let mut count = 99u32;
        assert_eq!(WriteFile(h, std::ptr::null(), 0, &mut count, std::ptr::null_mut()), 1);
        assert_eq!(count, 0);
        count = 99;
        assert_eq!(ReadFile(h, std::ptr::null_mut(), 0, &mut count, std::ptr::null_mut()), 1);
        assert_eq!(count, 0);

        // Nonzero-length reads and writes with a null buffer fail with ERROR_INVALID_PARAMETER.
        assert_eq!(WriteFile(h, std::ptr::null(), 4, &mut count, std::ptr::null_mut()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(ReadFile(h, std::ptr::null_mut(), 4, &mut count, std::ptr::null_mut()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);

        // FindClose rejects non-Find handles without closing them.
        assert_eq!(FindClose(h), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_HANDLE);
        let payload = b"ok";
        assert_eq!(WriteFile(h, payload.as_ptr(), 2, &mut count, std::ptr::null_mut()), 1);
        assert_eq!(count, 2);
        assert_eq!(super::super::CloseHandle(h), 1);

        // FindClose on an invalid handle sets ERROR_INVALID_HANDLE.
        assert_eq!(FindClose(invalid_handle()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_HANDLE);
    }
}

/// `VICTORY_TRACE_FILES=1`: reads per file (first and every power of two).
fn trace_read(file: &File, len: u32) {
    use std::os::fd::AsRawFd;
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    static SEEN: Mutex<std::collections::BTreeMap<String, (u64, u64)>> =
        Mutex::new(std::collections::BTreeMap::new());
    let mut buf = [0u8; 1024];
    // SAFETY: F_GETPATH (50) fills a PATH_MAX buffer.
    if unsafe { fcntl(file.as_raw_fd(), 50, buf.as_mut_ptr()) } < 0 {
        return;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(0);
    let path = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let e = seen.entry(path.clone()).or_default();
    e.0 += 1;
    e.1 += u64::from(len);
    if e.0.is_power_of_two() {
        tracing::info!(path, reads = e.0, bytes = e.1, "ReadFile of a file so far");
    }
}

#[cfg(test)]
mod std_handle_tests {
    use super::*;

    /// The three streams are handles a write can take, and asking twice gives
    /// the same one back, as on Windows.
    #[test]
    fn the_standard_streams_are_file_handles() {
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        assert!(!out.is_null());
        assert_ne!(out as usize, INVALID_HANDLE_VALUE);
        assert_eq!(GetStdHandle(STD_OUTPUT_HANDLE), out);
        assert!(file_of(out).is_some());

        let err = GetStdHandle(STD_ERROR_HANDLE);
        let inp = GetStdHandle(STD_INPUT_HANDLE);
        assert!(!err.is_null() && !inp.is_null());
        assert_ne!(err, out);
        assert_ne!(inp, out);

        // A write of nothing succeeds and reports nothing written: the handle
        // is a stream, and the call reaches it rather than failing.
        let mut written = u32::MAX;
        assert_eq!(
            WriteFile(out, std::ptr::null(), 0, &mut written, std::ptr::null_mut()),
            1
        );
        assert_eq!(written, 0);
    }

    #[test]
    fn a_stream_that_is_not_one_is_refused() {
        assert!(GetStdHandle(0).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }
}
