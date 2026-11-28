//! The rest of what title code (and the DLLs it ships) calls in kernel32 and
//! its API sets, advapi32, bcrypt, crypt32 and the window and message
//! calls of user32, on POSIX: time and time zones, strings and locales,
//! system information, processes and threads, mutexes, waitable timers,
//! the thread pool, FLS, interlocked lists, and stubs where the console
//! has nothing to answer (registry, ETW, COM, named pipes, window messages).
//!
//! Stubs say so in their failure: the registry has no keys, a COM class is
//! not registered, no window ever receives a message. Real work is only
//! where a title cannot go on without it.
//!
//! The C runtime (`api-ms-win-crt-*`) is not here.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

use victory_gpu::clock;
use victory_loader::native::{self, seh, teb};
use victory_platform::mem::{self, Protect};
use victory_platform::sync::{Event, Semaphore};
use victory_platform::thread;

use super::{Object, WAIT_OBJECT_0, WAIT_TIMEOUT, Waitable, files, lock, new_handle, set_error};

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_NOT_SUPPORTED: u32 = 50;
const ERROR_INVALID_PARAMETER: u32 = 87;
const ERROR_CALL_NOT_IMPLEMENTED: u32 = 120;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;
const ERROR_NOT_OWNER: u32 = 288;
const ERROR_IO_PENDING: u32 = 997;
const ERROR_INVALID_FLAGS: u32 = 1004;
const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
const ERROR_MOD_NOT_FOUND: u32 = 126;
const ERROR_ENVVAR_NOT_FOUND: u32 = 203;

/// 100 ns ticks from 1601 to 1970.
const EPOCH_DIFFERENCE: u64 = 116_444_736_000_000_000;
const TICKS_PER_SECOND: u64 = 10_000_000;
const TICKS_PER_DAY: u64 = 86_400 * TICKS_PER_SECOND;
/// Days from 1601-01-01 to 1970-01-01.
const DAYS_1601_TO_1970: i64 = 134_774;

unsafe extern "C" {
    fn arc4random_buf(buf: *mut c_void, len: usize);
    fn localtime_r(time: *const i64, out: *mut Tm) -> *mut Tm;
}

/// libc's `struct tm` on macOS.
#[repr(C)]
struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
    gmtoff: i64,
    zone: *const u8,
}

// SAFETY (every function below): the title passes the pointers its Windows
// call takes, valid for the structures named in each signature.

// Strings.

/// `n` units at `p`, or up to the NUL if `n` is negative.
unsafe fn units<'a, T: Copy + Default + PartialEq>(p: *const T, n: i32) -> &'a [T] {
    // SAFETY: as above.
    unsafe {
        let len = if n < 0 {
            (0..).take_while(|&i| *p.add(i) != T::default()).count()
        } else {
            n as usize
        };
        std::slice::from_raw_parts(p, len)
    }
}

/// Copies `text` out as `WideCharToMultiByte` and friends do: the count
/// needed if `size` is 0, else the count written, 0 with
/// `ERROR_INSUFFICIENT_BUFFER` if it does not fit.
fn copy_out<T: Copy>(text: &[T], out: *mut T, size: i32) -> i32 {
    if size < 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if size == 0 {
        return text.len() as i32;
    }
    if out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if text.len() > size as usize {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    // SAFETY: the title's buffer of `size` elements.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len()) };
    text.len() as i32
}

// shortcut: every code page is read and written as UTF-8 (or Latin-1 for
// 28591 and 1252); the title's own text is ASCII or UTF-8.
pub extern "system" fn MultiByteToWideChar(
    code_page: u32,
    _flags: u32,
    src: *const u8,
    src_len: i32,
    out: *mut u16,
    size: i32,
) -> i32 {
    if src.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: see above.
    let bytes = unsafe { units(src, if src_len < 0 { -1 } else { src_len }) };
    let mut text: Vec<u16> = if matches!(code_page, 1252 | 28591) {
        bytes.iter().map(|&b| u16::from(b)).collect()
    } else {
        String::from_utf8_lossy(bytes).encode_utf16().collect()
    };
    if src_len < 0 {
        text.push(0);
    }
    copy_out(&text, out, size)
}

pub extern "system" fn WideCharToMultiByte(
    code_page: u32,
    _flags: u32,
    src: *const u16,
    src_len: i32,
    out: *mut u8,
    size: i32,
    default: *const u8,
    used_default: *mut i32,
) -> i32 {
    if src.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if !used_default.is_null() {
        // SAFETY: see above.
        unsafe { used_default.write(0) };
    }
    // SAFETY: see above.
    let text =
        String::from_utf16_lossy(unsafe { units(src, if src_len < 0 { -1 } else { src_len }) });
    let mut replaced = false;
    let fallback = if default.is_null() {
        b'?'
    } else {
        // SAFETY: the title passes a pointer to a single default byte.
        unsafe { default.read() }
    };
    let mut bytes: Vec<u8> = if matches!(code_page, 1252 | 28591) {
        text.chars()
            .map(|c| match u8::try_from(u32::from(c)) {
                Ok(b) => b,
                Err(_) => {
                    replaced = true;
                    fallback
                }
            })
            .collect()
    } else {
        text.into_bytes()
    };
    if replaced && !used_default.is_null() {
        // SAFETY: see above.
        unsafe { used_default.write(1) };
    }
    if src_len < 0 {
        bytes.push(0);
    }
    copy_out(&bytes, out, size)
}

pub extern "system" fn lstrcmpW(a: *const u16, b: *const u16) -> i32 {
    // SAFETY: see above.
    let (a, b) = unsafe { (units(a, -1), units(b, -1)) };
    a.cmp(b) as i32
}

const NORM_IGNORECASE: u32 = 1;
const LINGUISTIC_IGNORECASE: u32 = 0x10;

fn fold(unit: u16) -> u16 {
    char::from_u32(u32::from(unit))
        .and_then(|c| c.to_lowercase().next())
        .map_or(unit, |c| c as u32 as u16)
}

// shortcut: code unit order, with case folded when asked; no linguistic
// collation.
pub extern "system" fn CompareStringEx(
    _locale: *const u16,
    flags: u32,
    a: *const u16,
    a_len: i32,
    b: *const u16,
    b_len: i32,
    _version: *const c_void,
    _reserved: *const c_void,
    _param: isize,
) -> i32 {
    // SAFETY: see above.
    let (a, b) = unsafe { (units(a, a_len), units(b, b_len)) };
    let ignore = flags & (NORM_IGNORECASE | LINGUISTIC_IGNORECASE) != 0;
    let key = |s: &[u16]| -> Vec<u16> {
        s.iter()
            .map(|&u| if ignore { fold(u) } else { u })
            .collect()
    };
    match key(a).cmp(&key(b)) {
        std::cmp::Ordering::Less => 1,
        std::cmp::Ordering::Equal => 2,
        std::cmp::Ordering::Greater => 3,
    }
}

const CT_CTYPE1: u32 = 1;

/// The `CT_CTYPE1` categories of one character (`winnls.h`).
fn ctype1(character: u16) -> u16 {
    const UPPER: u16 = 0x0001;
    const LOWER: u16 = 0x0002;
    const DIGIT: u16 = 0x0004;
    const SPACE: u16 = 0x0008;
    const PUNCT: u16 = 0x0010;
    const CNTRL: u16 = 0x0020;
    const BLANK: u16 = 0x0040;
    const XDIGIT: u16 = 0x0080;
    const ALPHA: u16 = 0x0100;
    const DEFINED: u16 = 0x0200;
    let Some(c) = char::from_u32(u32::from(character)) else {
        // A lone surrogate: not a character, so nothing is true of it.
        return 0;
    };
    // A digit is a digit in any script: `C1_DIGIT` is not ASCII-only here, and a
    // numeric character is never punctuation.
    let (alpha, digit) = (c.is_alphabetic(), c.is_numeric());
    let (space, control) = (c.is_whitespace(), c.is_control());
    let mut flags = DEFINED;
    if alpha {
        flags |= ALPHA;
    }
    if c.is_uppercase() {
        flags |= UPPER;
    }
    if c.is_lowercase() {
        flags |= LOWER;
    }
    if digit {
        flags |= DIGIT;
    }
    if space {
        flags |= SPACE;
    }
    if control {
        flags |= CNTRL;
    }
    if c == ' ' || c == '\t' {
        flags |= BLANK;
    }
    if c.is_ascii_hexdigit() {
        flags |= XDIGIT;
    }

    // `ispunct` is a graphical character that is neither a letter, a digit nor a
    // space - which is what makes an apostrophe one and a non-breaking space
    // none.
    if !alpha && !digit && !space && !control {
        flags |= PUNCT;
    }
    flags
}

fn get_string_type(kind: u32, src: *const u16, len: i32, out: *mut u16) -> i32 {
    if kind != CT_CTYPE1 || src.is_null() || out.is_null() {
        if kind != CT_CTYPE1 {
            // The bidirectional and width-and-kana tables are not answered here;
            // saying so beats answering zero for every character.
            tracing::warn!("GetStringType: CT_CTYPE{kind} is not answered");
        }
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: see above.
    for (i, &unit) in unsafe { units(src, len) }.iter().enumerate() {
        // SAFETY: the title's output array has one word per unit.
        unsafe { out.add(i).write(ctype1(unit)) }
    }
    1
}

pub extern "system" fn GetStringTypeW(kind: u32, src: *const u16, len: i32, out: *mut u16) -> i32 {
    get_string_type(kind, src, len, out)
}

/// The same question, asked with a locale: this host's is the one its
/// classification already uses.
pub extern "system" fn GetStringTypeExW(
    _locale: u32,
    kind: u32,
    src: *const u16,
    len: i32,
    out: *mut u16,
) -> i32 {
    get_string_type(kind, src, len, out)
}

pub extern "system" fn GetCPInfo(code_page: u32, info: *mut u8) -> i32 {
    if info.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // CPINFO: MaxCharSize u32, DefaultChar[2], LeadByte[12].
    // SAFETY: a 18-byte structure.
    unsafe {
        std::ptr::write_bytes(info, 0, 18);
        info.cast::<u32>()
            .write_unaligned(if code_page == 65001 { 4 } else { 1 });
        info.add(4).write(b'?');
    }
    1
}

const LCMAP_LOWERCASE: u32 = 0x100;
const LCMAP_UPPERCASE: u32 = 0x200;

pub extern "system" fn LCMapStringEx(
    _locale: *const u16,
    flags: u32,
    src: *const u16,
    len: i32,
    out: *mut u16,
    size: i32,
    _version: *const c_void,
    _reserved: *const c_void,
    _param: isize,
) -> i32 {
    if src.is_null() || len == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: see above.
    let mut text = unsafe { units(src, len) }.to_vec();
    for unit in &mut text {
        let Some(c) = char::from_u32(u32::from(*unit)) else {
            continue;
        };
        let mapped = if flags & LCMAP_LOWERCASE != 0 {
            c.to_lowercase().next()
        } else if flags & LCMAP_UPPERCASE != 0 {
            c.to_uppercase().next()
        } else {
            None
        };
        if let Some(m) = mapped.filter(|m| (*m as u32) < 0x1_0000) {
            *unit = m as u32 as u16;
        }
    }
    if len < 0 {
        text.push(0);
    }
    copy_out(&text, out, size)
}

fn write_wide(text: &str, out: *mut u16, size: i32) -> i32 {
    let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
    copy_out(&units, out, size)
}

pub extern "system" fn GetUserDefaultLocaleName(out: *mut u16, size: i32) -> i32 {
    write_wide("en-US", out, size)
}

pub extern "system" fn GetUserDefaultGeoName(out: *mut u16, size: i32) -> i32 {
    write_wide("US", out, size)
}

pub extern "system" fn LocaleNameToLCID(_name: *const u16, _flags: u32) -> u32 {
    0x0409
}

const LOCALE_RETURN_NUMBER: u32 = 0x2000_0000;

// shortcut: en-US; only the separators and names titles format with.
pub extern "system" fn GetLocaleInfoEx(
    _name: *const u16,
    kind: u32,
    out: *mut u16,
    size: i32,
) -> i32 {
    let text = match kind & !LOCALE_RETURN_NUMBER {
        0x0E => ".",
        0x0F => ",",
        0x10 => "3;0",
        0x5C | 0x59 => "en-US",
        0x02 | 0x04 => "English (United States)",
        _ => {
            set_error(ERROR_INVALID_FLAGS);
            return 0;
        }
    };
    write_wide(text, out, size)
}

/// `FormatMessage` with no message table: the id as text.
pub extern "system" fn FormatMessageA(
    flags: u32,
    _source: *const c_void,
    id: u32,
    _language: u32,
    out: *mut u8,
    size: u32,
    _args: *const c_void,
) -> u32 {
    const ALLOCATE_BUFFER: u32 = 0x100;
    let text = format!("Error {id}\0");
    if flags & ALLOCATE_BUFFER != 0 {
        // The title frees it with `LocalFree`, which keeps nothing to free.
        let leaked: &'static mut [u8] = Box::leak(text.into_bytes().into_boxed_slice());
        // SAFETY: with ALLOCATE_BUFFER `out` is a `char **`.
        unsafe { out.cast::<*mut u8>().write(leaked.as_mut_ptr()) };
        return (leaked.len() - 1) as u32;
    }
    if text.len() > size as usize {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    // SAFETY: the title's buffer of `size` bytes.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len()) };
    (text.len() - 1) as u32
}

/// `FormatMessageW`, the part a library uses (Microsoft Learn): `FROM_SYSTEM`, `FROM_STRING`
/// with `%1`..`%99` inserts, `IGNORE_INSERTS`, `ARGUMENT_ARRAY`, `ALLOCATE_BUFFER` and
/// `MAX_WIDTH_MASK` (other widths do not wrap: guess, `docs/native-host.md`). Other sources
/// (`FROM_HMODULE`) or flags fail with `ERROR_INVALID_PARAMETER`.
pub extern "system" fn FormatMessageW(
    flags: u32,
    source: *const c_void,
    id: u32,
    _language: u32,
    out: *mut u16,
    size: u32,
    args: *const c_void,
) -> u32 {
    const ALLOCATE_BUFFER: u32 = 0x100;
    const IGNORE_INSERTS: u32 = 0x200;
    const FROM_STRING: u32 = 0x400;
    const FROM_SYSTEM: u32 = 0x1000;
    const ARGUMENT_ARRAY: u32 = 0x2000;
    const MAX_WIDTH_MASK: u32 = 0xFF;
    let known = ALLOCATE_BUFFER | IGNORE_INSERTS | FROM_STRING | FROM_SYSTEM | ARGUMENT_ARRAY;
    let text = match flags & (FROM_STRING | FROM_SYSTEM) {
        _ if flags & !(known | MAX_WIDTH_MASK) != 0 || out.is_null() => None,
        FROM_SYSTEM => {
            let text = system_message(id);
            Some(if flags & MAX_WIDTH_MASK == MAX_WIDTH_MASK {
                text.trim_end_matches("\r\n").to_owned()
            } else {
                text
            })
        }
        FROM_STRING if !source.is_null() => {
            let format = files::wide_str(source.cast());
            let args = match () {
                () if flags & IGNORE_INSERTS != 0 => None,
                () if flags & ARGUMENT_ARRAY != 0 => Some(args.cast::<u64>()),
                // SAFETY: without `ARGUMENT_ARRAY`, `args` is a `va_list *`.
                () if !args.is_null() => Some(unsafe { args.cast::<*const u64>().read() }),
                () => Some(std::ptr::null()),
            };
            expand(&format, args)
        }
        _ => None,
    };
    let Some(text) = text else {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
    let len = (units.len() - 1) as u32;
    if flags & ALLOCATE_BUFFER != 0 {
        // shortcut: leaked, as `FormatMessageA`'s, since `LocalFree` keeps nothing to free; give
        // them a heap if a title formats many messages.
        let leaked: &'static mut [u16] = Box::leak(units.into_boxed_slice());
        // SAFETY: with ALLOCATE_BUFFER `out` is a `wchar_t **`.
        unsafe { out.cast::<*mut u16>().write(leaked.as_mut_ptr()) };
        return len;
    }
    if units.len() > size as usize {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    // SAFETY: the title's buffer of `size` UTF-16 units.
    unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), out, units.len()) };
    len
}

/// `FROM_SYSTEM`'s text for `id`, a Win32 error or the `HRESULT` of one, ending in "\r\n" as
/// Windows' do: its English text for common errors, else "Error 0x%08X". Guess: Windows fails
/// with `ERROR_MR_MID_NOT_FOUND` for an id it has no text for; a title that logs it is better
/// served by the number.
fn system_message(id: u32) -> String {
    let code = if id & 0xFFFF_0000 == 0x8007_0000 {
        id & 0xFFFF
    } else {
        id
    };
    let text = match code {
        0 => "The operation completed successfully.",
        1 => "Incorrect function.",
        2 => "The system cannot find the file specified.",
        3 => "The system cannot find the path specified.",
        4 => "The system cannot open the file.",
        5 => "Access is denied.",
        6 => "The handle is invalid.",
        8 => "Not enough memory resources are available to process this command.",
        14 => "Not enough memory resources are available to complete this operation.",
        50 => "The request is not supported.",
        87 => "The parameter is incorrect.",
        120 => "This function is not supported on this system.",
        122 => "The data area passed to a system call is too small.",
        0x8000_4001 => "Not implemented",
        0x8000_4002 => "No such interface supported",
        0x8000_4003 => "Invalid pointer",
        0x8000_4005 => "Unspecified error",
        _ => return format!("Error 0x{id:08X}\r\n"),
    };
    format!("{text}\r\n")
}

/// Expands a `FROM_STRING` message: `%1`..`%99` with an optional `!printf!` of `s`, `ls`, `ws`,
/// `d`, `i`, `u`, `x` or `X` (`!s!` when absent), and the escapes `%0` (ends the text), `%n`,
/// `%t`, `%%`, `%.`, `%!` and `% `. Arguments are 64-bit slots from `args`; `None` passes the
/// inserts through (`IGNORE_INSERTS`). `None` back for another `printf` or no arguments.
fn expand(format: &[u16], args: Option<*const u64>) -> Option<String> {
    let chars: Vec<char> = String::from_utf16_lossy(format).chars().collect();
    let mut text = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c != '%' {
            text.push(c);
            continue;
        }
        let Some(&next) = chars.get(i) else {
            text.push('%');
            break;
        };
        i += 1;
        match next {
            '0' => return Some(text),
            'n' => text.push_str("\r\n"),
            't' => text.push('\t'),
            '1'..='9' => {
                let start = i - 2;
                let mut n = next as usize - '0' as usize;
                if let Some(d) = chars.get(i).and_then(|c| c.to_digit(10)) {
                    n = n * 10 + d as usize;
                    i += 1;
                }
                let mut spec = String::from("s");
                if chars.get(i) == Some(&'!') {
                    let end = chars[i + 1..].iter().position(|&c| c == '!')? + i + 1;
                    spec = chars[i + 1..end].iter().collect();
                    i = end + 1;
                }
                let Some(args) = args else {
                    text.extend(&chars[start..i]);
                    continue;
                };
                if args.is_null() {
                    return None;
                }
                // SAFETY: the title passes an argument for each insert it uses.
                let arg = unsafe { args.add(n - 1).read_unaligned() };
                match spec.as_str() {
                    "s" | "ls" | "ws" if arg == 0 => return None,
                    "s" | "ls" | "ws" => {
                        text.push_str(&String::from_utf16_lossy(&files::wide_str(arg as _)));
                    }
                    "d" | "i" => text.push_str(&(arg as u32 as i32).to_string()),
                    "u" => text.push_str(&(arg as u32).to_string()),
                    "x" => text.push_str(&format!("{:x}", arg as u32)),
                    "X" => text.push_str(&format!("{:X}", arg as u32)),
                    _ => return None,
                }
            }
            other => text.push(other),
        }
    }
    Some(text)
}

pub extern "system" fn LocalFree(memory: *mut c_void) -> *mut c_void {
    let _ = memory;
    std::ptr::null_mut()
}

pub extern "system" fn GlobalFree(memory: *mut c_void) -> *mut c_void {
    LocalFree(memory)
}

/// `QueueUserWorkItem`: the callback runs on a thread of its own, as the legacy
/// work queue does. The flags choose which pool would run it, and this host has
/// the one, so they are not consulted.
pub extern "system" fn QueueUserWorkItem(callback: usize, context: *mut c_void, _flags: u32) -> i32 {
    if callback == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // A title pointer is not `Send`, and it is the title's own to pass across
    // threads: it handed this one to the queue for that.
    let context = context as usize;
    std::thread::spawn(move || {
        // SAFETY: the title's `WORKERCALLBACK` (`PVOID`, `BOOL TimerOrWaitFired`),
        // which this is not a timer for.
        unsafe { native::call_win64(callback, &[context as u64, 0]) };
    });
    1
}

/// `InitializeProcThreadAttributeList`: the attributes describe a process this
/// host does not start, so the call says so instead of leaving a list that
/// `UpdateProcThreadAttribute` would fill with nothing. It still answers the
/// size a list would take, which is what a caller asks with a null list.
pub extern "system" fn InitializeProcThreadAttributeList(
    list: *mut c_void,
    count: u32,
    _flags: u32,
    size: *mut usize,
) -> i32 {
    if size.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    if list.is_null() {
        // The size query. An attribute list is a header and one entry per
        // attribute; the exact layout is not this host's to promise.
        // SAFETY: the title's size out-pointer.
        unsafe { size.write(usize::from(size_of::<u64>()) * (1 + count as usize)) };
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    tracing::warn!("InitializeProcThreadAttributeList: a process is not started here");
    set_error(ERROR_NOT_SUPPORTED);
    0
}

/// `UpdateProcThreadAttribute`: nothing to put an attribute in, so it is refused
/// with the reason rather than accepted and forgotten.
pub extern "system" fn UpdateProcThreadAttribute(
    _list: *mut c_void,
    _flags: u32,
    attribute: usize,
    _value: *const c_void,
    _size: usize,
    _previous: *mut c_void,
    _return_size: *mut usize,
) -> i32 {
    tracing::warn!("UpdateProcThreadAttribute: attribute {attribute:#x} is not applied here");
    set_error(ERROR_NOT_SUPPORTED);
    0
}

/// `DeleteProcThreadAttributeList`: there is no list, and nothing to free.
pub extern "system" fn DeleteProcThreadAttributeList(_list: *mut c_void) {}

// Resources, read out of the mapped image's own directory: a module's resource
// tree is type, then name, then language, so a lookup walks three levels of it.

const ERROR_RESOURCE_TYPE_NOT_FOUND: u32 = 1813;
const ERROR_RESOURCE_NAME_NOT_FOUND: u32 = 1814;
const ERROR_RESOURCE_LANG_NOT_FOUND: u32 = 1815;

/// Where a module's resource directory starts, from the headers of the image
/// mapped at `base`, or `None` when it has none.
fn resource_directory(base: usize) -> Option<*const u8> {
    if base == 0 {
        return None;
    }
    // SAFETY: `base` is a mapped image and these are its own headers, which the
    // loader wrote and which stay for the life of the process.
    unsafe {
        let image = base as *const u8;
        let pe_at = image.add(0x3C).cast::<u32>().read_unaligned() as usize;
        let pe = image.add(pe_at);
        if pe.cast::<u32>().read_unaligned() != 0x0000_4550 {
            return None;
        }
        // The file header is 20 bytes after the signature, then the optional
        // header, whose data directory follows its fixed part.
        let optional = pe.add(24);
        let directory = match optional.cast::<u16>().read_unaligned() {
            0x20B => optional.add(112), // PE32+
            0x10B => optional.add(96),  // PE32
            _ => return None,
        };
        // Entry 2 is the resource directory: its address, then its size.
        let rva = directory.add(2 * 8).cast::<u32>().read_unaligned() as usize;
        (rva != 0).then(|| image.add(rva))
    }
}

/// One level of the tree: the entry for `name` (compared as Windows compares it,
/// without case) or for the integer `id` when `name` is `None`.
///
/// Returns the entry's own offset word, whose top bit says whether it points at
/// the next level or at the data.
///
/// # Safety
///
/// `level` and `base` are inside a mapped image's resource directory, and
/// `level` is a directory rather than a leaf.
unsafe fn resource_level(
    level: *const u8,
    base: *const u8,
    name: Option<&str>,
    id: u32,
) -> Option<u32> {
    // SAFETY: the caller's promise: both pointers are inside a mapped image.
    unsafe {
        let named = level.add(12).cast::<u16>().read_unaligned() as usize;
        let ids = level.add(14).cast::<u16>().read_unaligned() as usize;
        for i in 0..named + ids {
            let entry = level.add(16 + i * 8);
            let key = entry.cast::<u32>().read_unaligned();
            let offset = entry.add(4).cast::<u32>().read_unaligned();
            match name {
                // A named entry holds the offset of its string, which is a length
                // and then that many UTF-16 characters.
                Some(wanted) if key & 0x8000_0000 != 0 => {
                    let text = base.add((key & 0x7FFF_FFFF) as usize);
                    let len = text.cast::<u16>().read_unaligned() as usize;
                    let units = std::slice::from_raw_parts(text.add(2).cast::<u16>(), len);
                    if String::from_utf16_lossy(units).eq_ignore_ascii_case(wanted) {
                        return Some(offset);
                    }
                }
                Some(_) => {}
                None if key & 0x8000_0000 == 0 && key == id => return Some(offset),
                None => {}
            }
        }
    }
    None
}

/// A type or name argument: an integer id below 0x10000, or the address of a
/// string, which is how `MAKEINTRESOURCE` and a plain string are told apart.
fn resource_id(value: usize) -> Option<u32> {
    (value < 0x1_0000).then_some(value as u32)
}

/// The name a string argument holds.
///
/// # Safety
///
/// `value` is the address of a NUL-terminated wide string.
unsafe fn resource_name(value: usize) -> String {
    // SAFETY: the caller's promise: a NUL-terminated string.
    unsafe {
        let text = value as *const u16;
        let mut n = 0usize;
        while *text.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(text, n))
    }
}

/// The module a call names: its own base, or the title image's when it passes
/// null, which is what Windows means by null there.
fn resource_base(module: *mut c_void) -> usize {
    if module.is_null() {
        crate::runtime::main_image().unwrap_or(0)
    } else {
        module as usize
    }
}

/// `FindResourceExW`: the image's own resource entry, or the reason there is
/// none.
pub extern "system" fn FindResourceExW(
    module: *mut c_void,
    kind: usize,
    name: usize,
    language: u16,
) -> *mut c_void {
    let base = resource_base(module);
    let Some(directory) = resource_directory(base) else {
        tracing::warn!("FindResourceExW: the module at {base:#x} has no resources");
        set_error(ERROR_RESOURCE_TYPE_NOT_FOUND);
        return std::ptr::null_mut();
    };
    // SAFETY: every part of the directory below is inside the mapped image.
    unsafe {
        let kind_name = resource_id(kind).map_or_else(|| Some(resource_name(kind)), |_| None);
        let Some(kind_offset) = resource_level(directory, directory, kind_name.as_deref(), kind as u32)
        else {
            set_error(ERROR_RESOURCE_TYPE_NOT_FOUND);
            return std::ptr::null_mut();
        };
        if kind_offset & 0x8000_0000 == 0 {
            // A leaf where a level belongs: not a resource tree.
            set_error(ERROR_RESOURCE_TYPE_NOT_FOUND);
            return std::ptr::null_mut();
        }
        let by_name = resource_id(name).map_or_else(|| Some(resource_name(name)), |_| None);
        let names = directory.add((kind_offset & 0x7FFF_FFFF) as usize);
        let Some(name_offset) = resource_level(names, directory, by_name.as_deref(), name as u32)
        else {
            set_error(ERROR_RESOURCE_NAME_NOT_FOUND);
            return std::ptr::null_mut();
        };
        if name_offset & 0x8000_0000 == 0 {
            set_error(ERROR_RESOURCE_NAME_NOT_FOUND);
            return std::ptr::null_mut();
        }
        // The languages: the one asked for, or the first when zero means any.
        let languages = directory.add((name_offset & 0x7FFF_FFFF) as usize);
        let found = if language == 0 {
            let ids = languages.add(14).cast::<u16>().read_unaligned();
            (ids > 0).then(|| languages.add(20).cast::<u32>().read_unaligned())
        } else {
            resource_level(languages, directory, None, u32::from(language))
        };
        let Some(leaf) = found else {
            set_error(ERROR_RESOURCE_LANG_NOT_FOUND);
            return std::ptr::null_mut();
        };
        if leaf & 0x8000_0000 != 0 {
            set_error(ERROR_RESOURCE_LANG_NOT_FOUND);
            return std::ptr::null_mut();
        }
        // `HRSRC` is the image's own data entry, which is what Windows hands out
        // for a mapped module.
        directory.add(leaf as usize) as *mut c_void
    }
}

/// `LoadResource`: the resource's bytes, already in the mapped image, so the data
/// entry's address is all that is needed.
pub extern "system" fn LoadResource(module: *mut c_void, resource: *mut c_void) -> *mut c_void {
    if resource.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let base = resource_base(module);
    if base == 0 {
        set_error(ERROR_INVALID_HANDLE);
        return std::ptr::null_mut();
    }
    // SAFETY: `resource` is a data entry of the image at `base`.
    unsafe {
        let entry = resource as *const u8;
        let rva = entry.cast::<u32>().read_unaligned() as usize;
        (base + rva) as *mut c_void
    }
}

/// `GetConsoleWindow`: this host writes to the standard streams, so a console
/// is here and its window is a handle of its own (as `CreateWindowExA` above
/// answers for a library's message window). Nothing draws on it.
static CONSOLE_WINDOW: AtomicU64 = AtomicU64::new(0x10);

pub extern "system" fn GetConsoleWindow() -> *mut c_void {
    CONSOLE_WINDOW.load(Ordering::Relaxed) as *mut c_void
}

/// `GetConsoleScreenBufferInfo`: the shape a title sizes its output to. The
/// numbers are the ones a Windows console starts with.
pub extern "system" fn GetConsoleScreenBufferInfo(
    handle: *mut c_void,
    info: *mut u8,
) -> i32 {
    if handle.is_null() || info.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: the title's CONSOLE_SCREEN_BUFFER_INFO, 22 bytes.
    unsafe {
        let put16 = |at: usize, v: i16| info.add(at).cast::<i16>().write_unaligned(v);
        put16(0, 80); // dwSize.X
        put16(2, 25); // dwSize.Y
        put16(4, 0); // dwCursorPosition.X
        put16(6, 0); // dwCursorPosition.Y
        put16(8, 7); // wAttributes: grey on black
        put16(10, 0); // srWindow.Left
        put16(12, 0); // srWindow.Top
        put16(14, 79); // srWindow.Right
        put16(16, 24); // srWindow.Bottom
        put16(18, 80); // dwMaximumWindowSize.X
        put16(20, 25); // dwMaximumWindowSize.Y
    }
    1
}

/// `WriteConsoleW`: the text goes to the stream it was written to, as UTF-8,
/// rather than nowhere. What a console would draw with attributes is not drawn.
pub extern "system" fn WriteConsoleW(
    handle: *mut c_void,
    buffer: *const u16,
    len: u32,
    written: *mut u32,
    _reserved: *const c_void,
) -> i32 {
    const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;
    if !written.is_null() {
        // SAFETY: the title's count out-pointer.
        unsafe { written.write(0) };
    }
    if handle.is_null() || (len > 0 && buffer.is_null()) {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let text = if len == 0 {
        std::borrow::Cow::Borrowed("")
    } else {
        // SAFETY: the title's buffer of `len` characters.
        let units = unsafe { std::slice::from_raw_parts(buffer, len as usize) };
        String::from_utf16_lossy(units).into()
    };
    let fd = if handle == files::GetStdHandle(STD_ERROR_HANDLE) {
        2
    } else {
        1
    };
    let bytes = text.as_bytes();
    // SAFETY: a plain write of the title's text to the host's stream.
    let n = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if n < 0 {
        return 0;
    }
    if !written.is_null() {
        // SAFETY: the title's count out-pointer.
        unsafe { written.write(len) };
    }
    1
}

/// `LocalAlloc`: the process heap's block, as `HeapAlloc` gives one. A movable
/// block is refused rather than answered with a pointer, since a pointer is not
/// a movable handle.
///
/// Note that `LocalFree` keeps nothing to free (see its note), so what this
/// allocates is never returned: a title that allocates in a loop would grow.
pub extern "system" fn LocalAlloc(flags: u32, bytes: usize) -> *mut c_void {
    const LMEM_ZEROINIT: u32 = 0x0040;
    const LMEM_MOVEABLE: u32 = 0x0002;
    if flags & LMEM_MOVEABLE != 0 {
        tracing::warn!("LocalAlloc: a movable block is not answered");
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    if bytes == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    // SAFETY: a non-zero size; the block is the title's.
    let block = unsafe { libc::malloc(bytes) };
    if block.is_null() {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    }
    if flags & LMEM_ZEROINIT != 0 {
        // SAFETY: `bytes` of the block this call just allocated.
        unsafe { std::ptr::write_bytes(block.cast::<u8>(), 0, bytes) };
    }
    block
}

/// `lstrlenW`: the length in characters, not counting the terminator.
pub extern "system" fn lstrlenW(text: *const u16) -> i32 {
    if text.is_null() {
        return 0;
    }
    let mut n = 0usize;
    // SAFETY: the title's NUL-terminated string.
    unsafe {
        while *text.add(n) != 0 {
            n += 1;
        }
    }
    n as i32
}

/// `lstrcmpiW`: case-insensitive comparison, as Windows makes it, by folding
/// case rather than by the host's locale.
pub extern "system" fn lstrcmpiW(left: *const u16, right: *const u16) -> i32 {
    if left.is_null() || right.is_null() {
        // Windows compares a null string as empty.
        return i32::from(left.is_null() as i32 as u8) - i32::from(right.is_null() as i32 as u8);
    }
    let mut at = 0usize;
    loop {
        // SAFETY: the title's NUL-terminated strings, read to their end.
        let (a, b) = unsafe { (*left.add(at), *right.add(at)) };
        if a == 0 || b == 0 {
            return i32::from(a != 0) - i32::from(b != 0);
        }
        let fold = |c: u16| -> u32 {
            char::from_u32(u32::from(c))
                .map(|ch| {
                    ch.to_lowercase()
                        .next()
                        .map_or(u32::from(c), |lower| lower as u32)
                })
                .unwrap_or(u32::from(c))
        };
        let (fa, fb) = (fold(a), fold(b));
        if fa != fb {
            return if fa < fb { -1 } else { 1 };
        }
        at += 1;
    }
}

/// `GetUserDefaultUILanguage`: the language of the interface, which for this
/// host is the one its locale answers with.
pub extern "system" fn GetUserDefaultUILanguage() -> u16 {
    0x0409
}

/// `GetSystemTimeAdjustment`: the clock is not adjusted here, and the increment
/// is the timer's period on Windows (15.625 ms in 100 ns units).
pub extern "system" fn GetSystemTimeAdjustment(
    adjustment: *mut u32,
    increment: *mut u32,
    disabled: *mut i32,
) -> i32 {
    // SAFETY: the title's out parameters, each optional.
    unsafe {
        if !adjustment.is_null() {
            adjustment.write(0);
        }
        if !increment.is_null() {
            increment.write(156_250);
        }
        if !disabled.is_null() {
            disabled.write(1);
        }
    }
    1
}

/// `FlushProcessWriteBuffers`: on Windows this makes every thread's earlier
/// writes visible to every other. The fence is what that means here; the host's
/// memory is already coherent.
pub extern "system" fn FlushProcessWriteBuffers() {
    std::sync::atomic::fence(Ordering::SeqCst);
}

pub extern "system" fn EncodePointer(pointer: *mut c_void) -> *mut c_void {
    pointer
}

pub extern "system" fn DecodePointer(pointer: *mut c_void) -> *mut c_void {
    pointer
}

// Time.

pub(super) fn now_ticks() -> u64 {
    EPOCH_DIFFERENCE + (clock::unix_time().as_nanos() / 100) as u64
}

/// Seconds east of UTC at the current time.
fn local_offset() -> i64 {
    let now = (now_ticks() - EPOCH_DIFFERENCE) / TICKS_PER_SECOND;
    let mut tm = std::mem::MaybeUninit::<Tm>::zeroed();
    // SAFETY: `localtime_r` fills the structure it is given.
    unsafe {
        localtime_r(&raw const now as *const i64, tm.as_mut_ptr());
        tm.assume_init().gmtoff
    }
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `SYSTEMTIME`: year, month, day of week, day, hour, minute, second, ms.
type SystemTimeFields = [u16; 8];

fn to_system_time(ticks: u64) -> SystemTimeFields {
    let days = (ticks / TICKS_PER_DAY) as i64;
    let rest = ticks % TICKS_PER_DAY;
    let (y, m, d) = civil_from_days(days - DAYS_1601_TO_1970);
    let seconds = rest / TICKS_PER_SECOND;
    [
        y as u16,
        m as u16,
        ((days + 1) % 7) as u16,
        d as u16,
        (seconds / 3600) as u16,
        (seconds / 60 % 60) as u16,
        (seconds % 60) as u16,
        (rest % TICKS_PER_SECOND / 10_000) as u16,
    ]
}

fn days_in_month(year: u16, month: u16) -> u16 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap =
                (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
            if leap { 29 } else { 28 }
        }
        _ => 0,
    }
}

fn from_system_time(t: &SystemTimeFields) -> Option<u64> {
    let [y, m, _, d, h, mi, s, ms] = *t;
    if !(1601..=30827).contains(&y)
        || !(1..=12).contains(&m)
        || d == 0
        || d > days_in_month(y, m)
        || h > 23
        || mi > 59
        || s > 59
        || ms > 999
    {
        return None;
    }
    let days = days_from_civil(i64::from(y), u32::from(m), u32::from(d)) + DAYS_1601_TO_1970;
    let days = u64::try_from(days).ok()?;
    let seconds = u64::from(h) * 3600 + u64::from(mi) * 60 + u64::from(s);
    Some(days * TICKS_PER_DAY + seconds * TICKS_PER_SECOND + u64::from(ms) * 10_000)
}

pub extern "system" fn GetSystemTimeAsFileTime(out: *mut u64) {
    // SAFETY: a FILETIME (two words, 4-byte aligned).
    unsafe { out.write_unaligned(now_ticks()) };
}

pub extern "system" fn GetSystemTimePreciseAsFileTime(out: *mut u64) {
    GetSystemTimeAsFileTime(out);
}

pub extern "system" fn GetSystemTime(out: *mut SystemTimeFields) {
    // SAFETY: a SYSTEMTIME.
    unsafe { out.write_unaligned(to_system_time(now_ticks())) };
}

pub extern "system" fn GetLocalTime(out: *mut SystemTimeFields) {
    let ticks = now_ticks().wrapping_add_signed(local_offset() * TICKS_PER_SECOND as i64);
    // SAFETY: a SYSTEMTIME.
    unsafe { out.write_unaligned(to_system_time(ticks)) };
}

pub extern "system" fn FileTimeToSystemTime(ft: *const u64, out: *mut SystemTimeFields) -> i32 {
    if ft.is_null() || out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: a FILETIME and a SYSTEMTIME.
    unsafe { out.write_unaligned(to_system_time(ft.read_unaligned())) };
    1
}

pub extern "system" fn SystemTimeToFileTime(st: *const SystemTimeFields, out: *mut u64) -> i32 {
    if st.is_null() || out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: a SYSTEMTIME and a FILETIME.
    let fields = unsafe { st.read_unaligned() };
    match from_system_time(&fields) {
        Some(ticks) => {
            // SAFETY: as above.
            unsafe { out.write_unaligned(ticks) };
            1
        }
        None => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

pub extern "system" fn FileTimeToLocalFileTime(ft: *const u64, out: *mut u64) -> i32 {
    if ft.is_null() || out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: two FILETIMEs.
    unsafe {
        out.write_unaligned(
            ft.read_unaligned()
                .wrapping_add_signed(local_offset() * TICKS_PER_SECOND as i64),
        )
    };
    1
}

pub extern "system" fn LocalFileTimeToFileTime(ft: *const u64, out: *mut u64) -> i32 {
    if ft.is_null() || out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: two FILETIMEs.
    unsafe {
        out.write_unaligned(
            ft.read_unaligned()
                .wrapping_add_signed(-local_offset() * TICKS_PER_SECOND as i64),
        )
    };
    1
}

/// `GetDateFormatEx` and `GetTimeFormatEx`: ISO 8601, whatever the format
/// asked for.
fn format_time(time: *const SystemTimeFields, date: bool, out: *mut u16, size: i32) -> i32 {
    let t = if time.is_null() {
        to_system_time(now_ticks().wrapping_add_signed(local_offset() * TICKS_PER_SECOND as i64))
    } else {
        // SAFETY: a SYSTEMTIME.
        unsafe { time.read_unaligned() }
    };
    let text = if date {
        format!("{:04}-{:02}-{:02}", t[0], t[1], t[3])
    } else {
        format!("{:02}:{:02}:{:02}", t[4], t[5], t[6])
    };
    write_wide(&text, out, size)
}

pub extern "system" fn GetDateFormatEx(
    _locale: *const u16,
    _flags: u32,
    time: *const SystemTimeFields,
    _format: *const u16,
    out: *mut u16,
    size: i32,
    _calendar: *const u16,
) -> i32 {
    format_time(time, true, out, size)
}

pub extern "system" fn GetTimeFormatEx(
    _locale: *const u16,
    _flags: u32,
    time: *const SystemTimeFields,
    _format: *const u16,
    out: *mut u16,
    size: i32,
) -> i32 {
    format_time(time, false, out, size)
}

// System information.

fn cpus() -> u32 {
    std::thread::available_parallelism().map_or(1, |n| n.get() as u32)
}

pub extern "system" fn GetNativeSystemInfo(out: *mut u8) {
    // SYSTEM_INFO, 48 bytes.
    let n = cpus();
    let mask = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    // SAFETY: the title's SYSTEM_INFO.
    unsafe {
        std::ptr::write_bytes(out, 0, 48);
        out.cast::<u16>().write_unaligned(9); // PROCESSOR_ARCHITECTURE_AMD64
        out.add(4).cast::<u32>().write_unaligned(4096);
        out.add(8).cast::<u64>().write_unaligned(0x1_0000);
        out.add(16).cast::<u64>().write_unaligned(0x7FFF_FFFE_FFFF);
        out.add(24).cast::<u64>().write_unaligned(mask);
        out.add(32).cast::<u32>().write_unaligned(n.min(64));
        out.add(36).cast::<u32>().write_unaligned(8664);
        out.add(40).cast::<u32>().write_unaligned(0x1_0000);
        out.add(44).cast::<u16>().write_unaligned(0x17);
    }
}

pub extern "system" fn GetSystemInfo(out: *mut u8) {
    GetNativeSystemInfo(out);
}

pub extern "system" fn GlobalMemoryStatusEx(out: *mut u8) -> i32 {
    let (fast, standard) = victory_gpu::console::current().title_memory();
    let total = (fast + standard) as u64;
    // MEMORYSTATUSEX, 64 bytes.
    // SAFETY: the title's structure.
    unsafe {
        out.add(4).cast::<u32>().write_unaligned(25);
        for (at, value) in [
            (8, total),
            (16, total * 3 / 4),
            (24, total),
            (32, total * 3 / 4),
            (40, 0x7FFF_FFFE_0000),
            (48, 0x7FFF_0000_0000),
            (56, 0),
        ] {
            out.add(at).cast::<u64>().write_unaligned(value);
        }
    }
    1
}

pub extern "system" fn GetLogicalProcessorInformation(out: *mut u8, length: *mut u32) -> i32 {
    if length.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SYSTEM_LOGICAL_PROCESSOR_INFORMATION is 32 bytes: one entry per
    // core, and one for the package.
    let n = cpus().min(64);
    let needed = (n + 1) * 32;
    // SAFETY: the title's length word and buffer.
    unsafe {
        if out.is_null() || length.read_unaligned() < needed {
            length.write_unaligned(needed);
            set_error(ERROR_INSUFFICIENT_BUFFER);
            return 0;
        }
        std::ptr::write_bytes(out, 0, needed as usize);
        for i in 0..n {
            out.add(i as usize * 32)
                .cast::<u64>()
                .write_unaligned(1 << i);
        }
        let package = out.add(n as usize * 32);
        package
            .cast::<u64>()
            .write_unaligned(if n == 64 { u64::MAX } else { (1 << n) - 1 });
        package.add(8).cast::<u32>().write_unaligned(3);
        length.write_unaligned(needed);
    }
    1
}

/// How many cores are the fast kind (`hw.perflevel0`): the ones a scheduler
/// should fill first. `None` when the host does not say.
fn performance_cores() -> Option<u32> {
    // SAFETY: a plain sysctl read of a u32.
    unsafe {
        let mut value = 0u32;
        let mut size = size_of::<u32>();
        let read = libc::sysctlbyname(
            c"hw.perflevel0.physicalcpu".as_ptr(),
            (&raw mut value).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        );
        (read == 0 && size == size_of::<u32>()).then_some(value)
    }
}

// The extended logical-processor information: one entry per answer, each with
// its own relationship and size, as Windows lays them out.

const RELATION_PROCESSOR_CORE: u32 = 0;
const RELATION_CACHE: u32 = 2;
const RELATION_PROCESSOR_PACKAGE: u32 = 3;
const RELATION_GROUP: u32 = 4;
const RELATION_ALL: u32 = 0xFFFF;

/// Appends one `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX`: the relationship, the
/// entry's size, then the relationship's own structure. Entries are eight-byte
/// aligned and the size covers the whole entry.
fn push_relation(out: &mut Vec<u8>, relationship: u32, payload: &[u8]) {
    let size = (8 + payload.len() + 7) & !7;
    out.extend_from_slice(&relationship.to_le_bytes());
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out.resize(out.len() + (size - 8 - payload.len()), 0);
}

/// `PROCESSOR_RELATIONSHIP`: the cores in one group, with `class` telling a
/// scheduler how efficient they are (zero is the most efficient kind here).
fn processor_relationship(mask: u64, class: u8) -> Vec<u8> {
    let mut payload = [0u8; 40];
    payload[0] = 0; // Flags: LTP_PC_SMT is not set, these cores are whole.
    payload[1] = class;
    payload[22..24].copy_from_slice(&1u16.to_le_bytes()); // GroupCount
    payload[24..32].copy_from_slice(&mask.to_le_bytes()); // GroupMask[0].Mask
    // Group 0 and the reserved words stay zero.
    payload.to_vec()
}

/// `GROUP_RELATIONSHIP`: the one processor group, with every processor in it.
fn group_relationship(count: u32, mask: u64) -> Vec<u8> {
    let mut payload = [0u8; 72];
    payload[0..2].copy_from_slice(&1u16.to_le_bytes()); // MaximumGroupCount
    payload[2..4].copy_from_slice(&1u16.to_le_bytes()); // ActiveGroupCount
    payload[24] = count.min(64) as u8; // GroupInfo[0].MaximumProcessorCount
    payload[25] = count.min(64) as u8; // GroupInfo[0].ActiveProcessorCount
    payload[64..72].copy_from_slice(&mask.to_le_bytes()); // ActiveProcessorMask
    payload.to_vec()
}

/// The entries an answer holds, or `None` for a relationship this host does not
/// answer: caches and NUMA nodes are not described here, and a caller asking for
/// one is told so rather than handed an entry that would parse as something it
/// is not.
fn logical_processor_entries(relationship: u32) -> Option<Vec<u8>> {
    let n = cpus().min(64);
    let every = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
    let wanted = |kind: u32| relationship == kind || relationship == RELATION_ALL;
    let mut out = Vec::new();
    if wanted(RELATION_PROCESSOR_CORE) {
        // One logical processor is one core here, which is what `GetSystemInfo`
        // advertises too.
        let fast = performance_cores().unwrap_or(n).min(n);
        for i in 0..n {
            let class = u8::from(i >= fast);
            push_relation(&mut out, RELATION_PROCESSOR_CORE, &processor_relationship(1 << i, class));
        }
    }
    if wanted(RELATION_PROCESSOR_PACKAGE) {
        push_relation(&mut out, RELATION_PROCESSOR_PACKAGE, &processor_relationship(every, 0));
    }
    if wanted(RELATION_GROUP) {
        push_relation(&mut out, RELATION_GROUP, &group_relationship(n, every));
    }
    (!out.is_empty()).then_some(out)
}

pub extern "system" fn GetLogicalProcessorInformationEx(
    relationship: u32,
    buffer: *mut u8,
    length: *mut u32,
) -> i32 {
    if length.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Some(bytes) = logical_processor_entries(relationship) else {
        tracing::warn!("GetLogicalProcessorInformationEx: {relationship:#x} is not answered");
        // A relationship this host has nothing to say about, named.
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    };
    let needed = bytes.len() as u32;
    // SAFETY: the title's length word and buffer.
    unsafe {
        let room = length.read_unaligned();
        length.write_unaligned(needed);
        if buffer.is_null() || room < needed {
            set_error(ERROR_INSUFFICIENT_BUFFER);
            return 0;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len());
    }
    1
}

pub extern "system" fn IsProcessorFeaturePresent(feature: u32) -> i32 {
    // SSE and later, RDTSC, NX, cmpxchg16b, XSAVE, AVX2, RDRAND: the console's CPU has them.
    // Not `__fastfail` (23): a failure then goes through `abort`, which we log.
    i32::from(matches!(
        feature,
        6 | 8 | 10 | 12 | 13 | 14 | 17 | 28 | 36 | 37 | 38
    ))
}

pub extern "system" fn IsDebuggerPresent() -> i32 {
    0
}

pub extern "system" fn UnhandledExceptionFilter(_info: *const c_void) -> i32 {
    0 // EXCEPTION_CONTINUE_SEARCH
}

pub extern "system" fn RtlCaptureStackBackTrace(
    _skip: u32,
    _count: u32,
    _frames: *mut *mut c_void,
    hash: *mut u32,
) -> u16 {
    if !hash.is_null() {
        // SAFETY: the title's hash word.
        unsafe { hash.write(0) };
    }
    0
}

/// The old form of `RtlUnwindEx`, with no context to start from.
pub extern "system" fn RtlUnwind(
    target_frame: u64,
    target_ip: usize,
    record: *mut seh::context::ExceptionRecord,
    value: u64,
) {
    // SAFETY: the title's own call; the unwinder checks what it walks.
    unsafe {
        seh::api::unwind_ex(
            target_frame,
            target_ip,
            record,
            value,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    }
}

pub extern "system" fn WerRegisterFile(_file: *const u16, _kind: u32, _flags: u32) -> i32 {
    0
}

// Processes and threads.

pub extern "system" fn GetCurrentProcess() -> isize {
    -1
}

pub extern "system" fn GetCurrentThread() -> isize {
    -2
}

// The thread calls a crash reporter asks for: a thread's name, which thread
// belongs to whom, and how a pool should size itself. The name is kept for the
// thread and given back as Windows gives it out.

#[derive(Clone, Default)]
struct ThreadSettings {
    /// The thread's description, as a crash reporter prints it.
    name: Option<String>,
}

/// The calling thread has no handle of its own, so its settings live here.
thread_local! {
    static CURRENT_THREAD_SETTINGS: std::cell::RefCell<ThreadSettings> =
        const { std::cell::RefCell::new(ThreadSettings { name: None }) };
}

/// Every other thread, by the handle the title holds.
static THREAD_SETTINGS: LazyLock<Mutex<HashMap<usize, ThreadSettings>>> =
    LazyLock::new(Default::default);

/// The settings of `thread`, which the call has already checked is this process's
/// thread.
fn thread_settings(thread: *mut c_void, change: impl FnOnce(&mut ThreadSettings)) {
    if thread as isize == GetCurrentThread() {
        CURRENT_THREAD_SETTINGS.with(|settings| change(&mut settings.borrow_mut()));
        return;
    }
    let mut all = super::lock(&THREAD_SETTINGS);
    change(all.entry(thread as usize).or_default());
}

fn settings_of(thread: *mut c_void) -> ThreadSettings {
    if thread as isize == GetCurrentThread() {
        return CURRENT_THREAD_SETTINGS.with(|settings| settings.borrow().clone());
    }
    super::lock(&THREAD_SETTINGS)
        .get(&(thread as usize))
        .cloned()
        .unwrap_or_default()
}

/// Whether `thread` is a thread of this process: the pseudo handle of the calling
/// thread, or a thread in the handle table.
fn is_thread(thread: *mut c_void) -> bool {
    if thread as isize == GetCurrentThread() {
        return true;
    }
    matches!(
        super::lock(&super::HANDLES).get(&(thread as usize)),
        Some(Object::Thread(_))
    )
}

pub extern "system" fn GetMaximumProcessorCount(group: u16) -> u32 {
    GetActiveProcessorCount(group)
}

/// One group holds every processor.
pub extern "system" fn GetActiveProcessorGroupCount() -> u16 {
    1
}

pub extern "system" fn GetProcessIdOfThread(thread: *mut c_void) -> u32 {
    if !is_thread(thread) {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    // Every thread in this host is this process's.
    super::GetCurrentProcessId()
}

/// The name is kept for the thread, which is what a crash reporter prints; it is
/// not given to the host's own tooling, which has no way to name a thread.
pub extern "system" fn SetThreadDescription(thread: *mut c_void, description: usize) -> i32 {
    if !is_thread(thread) {
        return 0x8007_0006u32 as i32; // HRESULT_FROM_WIN32(ERROR_INVALID_HANDLE)
    }
    let name = if description == 0 {
        None
    } else {
        // SAFETY: the caller's `HSTRING`.
        Some(unsafe {
            let mut len = 0u32;
            let data = super::winrt::WindowsGetStringRawBuffer(description as *mut u8, &mut len);
            String::from_utf16_lossy(std::slice::from_raw_parts(data, len as usize))
        })
    };
    thread_settings(thread, |settings| settings.name = name);
    0
}

/// The name comes back as a `LocalAlloc`ed string, as Windows hands it out, and a
/// thread with no name answers success with no string rather than an error.
pub extern "system" fn GetThreadDescription(thread: *mut c_void, out: *mut usize) -> i32 {
    if out.is_null() {
        return 0x8000_4003u32 as i32; // E_POINTER
    }
    if !is_thread(thread) {
        return 0x8007_0006u32 as i32; // HRESULT_FROM_WIN32(ERROR_INVALID_HANDLE)
    }
    let name = settings_of(thread).name;
    let copy = match name {
        Some(name) => {
            let units: Vec<u16> = name.encode_utf16().chain([0]).collect();
            let block = LocalAlloc(0, units.len() * size_of::<u16>()).cast::<u16>();
            if block.is_null() {
                return 0x8007_000Eu32 as i32; // E_OUTOFMEMORY
            }
            // SAFETY: the block `LocalAlloc` just gave us, of `units.len()`
            // characters.
            unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), block, units.len()) };
            block as usize
        }
        None => 0,
    };
    // SAFETY: the caller's out string.
    unsafe { out.write_unaligned(copy) };
    0
}

pub extern "system" fn GetProcessId(_process: isize) -> u32 {
    std::process::id()
}

pub extern "system" fn GetThreadPriority(_thread: *mut c_void) -> i32 {
    0
}

pub extern "system" fn SetThreadPriority(_thread: *mut c_void, _priority: i32) -> i32 {
    1
}

pub extern "system" fn SetProcessPriorityBoost(_process: isize, _disable: i32) -> i32 {
    1
}

/// `ALL_PROCESSOR_GROUPS`: the group number that asks for every processor.
const ALL_PROCESSOR_GROUPS: u16 = 0xFFFF;
/// Windows puts at most 64 processors in a group.
const GROUP_SIZE: u32 = 64;

/// `GetActiveProcessorCount`: how many processors a title picks up here. A title
/// that sizes its worker pool by this answers `cpus()`, where the stub answered 0.
pub extern "system" fn GetActiveProcessorCount(group: u16) -> u32 {
    let n = cpus();
    let groups = n.div_ceil(GROUP_SIZE);
    if group == ALL_PROCESSOR_GROUPS {
        return n;
    }
    let group = u32::from(group);
    if group >= groups {
        return 0;
    }
    n.saturating_sub(group * GROUP_SIZE).min(GROUP_SIZE)
}

/// `GetCurrentProcessorNumber`: a hint on Windows, and a hint here too, from the
/// calling thread's id (the host gives no cheaper way to ask which CPU is running).
pub extern "system" fn GetCurrentProcessorNumber() -> u32 {
    thread::current_id() % cpus()
}

// shortcut: the boost setting is remembered, not applied; this host has no per-thread
// priority boost to turn off, and a title only reads back what it set.
static NO_BOOST: AtomicU32 = AtomicU32::new(0);

pub extern "system" fn GetThreadPriorityBoost(_thread: *mut c_void, disable: *mut i32) -> i32 {
    // SAFETY: the title's out word.
    unsafe { disable.write(NO_BOOST.load(Ordering::Relaxed) as i32) };
    1
}

pub extern "system" fn SetThreadPriorityBoost(_thread: *mut c_void, disable: i32) -> i32 {
    NO_BOOST.store(u32::from(disable != 0), Ordering::Relaxed);
    1
}

/// `SetThreadStackGuarantee`: the title's stack is a fixed reservation here, so the
/// guarantee is accepted and left as the caller set it.
pub extern "system" fn SetThreadStackGuarantee(guarantee: *mut u32) -> i32 {
    if guarantee.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    1
}

// shortcut: one error mode for the process. Windows keeps a thread mode beside the
// process one; a title that sets one and reads the other sees one value here.
static ERROR_MODE: AtomicU32 = AtomicU32::new(0);

pub extern "system" fn GetErrorMode() -> u32 {
    ERROR_MODE.load(Ordering::Relaxed)
}

pub extern "system" fn SetErrorMode(mode: u32) -> u32 {
    ERROR_MODE.swap(mode, Ordering::Relaxed)
}

pub extern "system" fn SetThreadErrorMode(mode: u32, old: *mut u32) -> i32 {
    let previous = ERROR_MODE.swap(mode, Ordering::Relaxed);
    if !old.is_null() {
        // SAFETY: the title's out word.
        unsafe { old.write(previous) };
    }
    1
}

// shortcut: the priority class is remembered, not applied. Raising a process's own
/// scheduling class on macOS is a privileged operation, and a title only reads back
/// what it set (some print it in their crash logs).
static PRIORITY_CLASS: AtomicU32 = AtomicU32::new(NORMAL_PRIORITY_CLASS);
const IDLE_PRIORITY_CLASS: u32 = 0x40;
const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x4000;
const NORMAL_PRIORITY_CLASS: u32 = 0x20;
const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x8000;
const HIGH_PRIORITY_CLASS: u32 = 0x80;
const REALTIME_PRIORITY_CLASS: u32 = 0x100;

const PRIORITY_CLASSES: [u32; 6] = [
    IDLE_PRIORITY_CLASS,
    BELOW_NORMAL_PRIORITY_CLASS,
    NORMAL_PRIORITY_CLASS,
    ABOVE_NORMAL_PRIORITY_CLASS,
    HIGH_PRIORITY_CLASS,
    REALTIME_PRIORITY_CLASS,
];

/// `IMAGE_FILE_MACHINE_AMD64`: the machine whose code this host runs.
const IMAGE_FILE_MACHINE_AMD64: u16 = 0x8664;

/// Whether `process` names this process: the pseudo handle is the only one a title
/// can hold here, since no process handle is ever created.
fn is_this_process(process: *mut c_void) -> bool {
    process as isize == GetCurrentProcess()
}

pub extern "system" fn SetPriorityClass(process: *mut c_void, class: u32) -> i32 {
    if !is_this_process(process) {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if !PRIORITY_CLASSES.contains(&class) {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    PRIORITY_CLASS.store(class, Ordering::Relaxed);
    1
}

pub extern "system" fn GetPriorityClass(process: *mut c_void) -> u32 {
    if !is_this_process(process) {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    PRIORITY_CLASS.load(Ordering::Relaxed)
}

/// `IsWow64Process2`: this process is the machine's own code, so it is not a 32-bit
/// process under emulation. The native machine is reported as AMD64 because that is
/// the machine whose code runs here: a title told otherwise takes paths (arm64
/// binaries, different threading) that this host does not serve.
pub extern "system" fn IsWow64Process2(
    process: *mut c_void,
    process_machine: *mut u16,
    native_machine: *mut u16,
) -> i32 {
    if !is_this_process(process) {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    // SAFETY: the title's out words, either of which it may leave null.
    unsafe {
        if !process_machine.is_null() {
            process_machine.write(0);
        }
        if !native_machine.is_null() {
            native_machine.write(IMAGE_FILE_MACHINE_AMD64);
        }
    }
    1
}

/// `QueryFullProcessImageNameW`: the title image's path, in the caller's buffer with
/// its NUL, and the length without it back in `size`. A buffer that is too small is
/// `ERROR_INSUFFICIENT_BUFFER`, as Windows reports it, and is not filled.
pub extern "system" fn QueryFullProcessImageNameW(
    process: *mut c_void,
    _flags: u32,
    out: *mut u16,
    size: *mut u32,
) -> i32 {
    if !is_this_process(process) {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if out.is_null() || size.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let mut name = vec![0u16; 4096];
    let written =
        crate::modules::kernel::GetModuleFileNameW(std::ptr::null_mut(), name.as_mut_ptr(), 4096)
            as usize;
    let path: Vec<u16> = if written > 0 {
        name.truncate(written);
        name
    } else {
        std::env::current_exe()
            .map(|p| p.to_string_lossy().encode_utf16().collect())
            .unwrap_or_default()
    };
    // SAFETY: the title's in/out size.
    let capacity = unsafe { size.read() } as usize;
    if path.is_empty() {
        set_error(ERROR_FILE_NOT_FOUND);
        return 0;
    }
    if path.len() + 1 > capacity {
        set_error(ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    // SAFETY: a buffer of `capacity` units, which holds the path and its NUL.
    unsafe {
        std::ptr::copy_nonoverlapping(path.as_ptr(), out, path.len());
        out.add(path.len()).write(0);
        size.write(path.len() as u32);
    }
    1
}

pub extern "system" fn SetThreadIdealProcessorEx(
    _thread: *mut c_void,
    _ideal: *const u8,
    previous: *mut u8,
) -> i32 {
    if !previous.is_null() {
        // SAFETY: a PROCESSOR_NUMBER (group u16, number u8, reserved u8).
        unsafe { std::ptr::write_bytes(previous, 0, 4) };
    }
    1
}

pub extern "system" fn GetThreadGroupAffinity(_thread: *mut c_void, out: *mut u8) -> i32 {
    let n = cpus().min(64);
    // GROUP_AFFINITY: Mask u64, Group u16, 6 reserved bytes.
    // SAFETY: the title's structure.
    unsafe {
        std::ptr::write_bytes(out, 0, 16);
        out.cast::<u64>()
            .write_unaligned(if n == 64 { u64::MAX } else { (1 << n) - 1 });
    }
    1
}

pub extern "system" fn SetThreadGroupAffinity(
    _thread: *mut c_void,
    _affinity: *const u8,
    previous: *mut u8,
) -> i32 {
    if !previous.is_null() {
        GetThreadGroupAffinity(_thread, previous);
    }
    1
}

pub extern "system" fn SetThreadAffinityMask(_thread: *mut c_void, _mask: usize) -> usize {
    usize::MAX >> (64 - cpus().min(64))
}

pub extern "system" fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize) {
    // SAFETY: the calling thread's own TEB, and the title's two words.
    unsafe {
        let teb = teb::current();
        high.write(teb.add(teb::OFF_STACK_BASE).cast::<usize>().read());
        low.write(teb.add(teb::OFF_STACK_LIMIT).cast::<usize>().read());
    }
}

pub extern "system" fn GetStartupInfoW(out: *mut u8) {
    // STARTUPINFOW is 104 bytes, `cb` first.
    // SAFETY: the title's structure.
    unsafe {
        std::ptr::write_bytes(out, 0, 104);
        out.cast::<u32>().write_unaligned(104);
    }
}

fn command_line() -> &'static [u16] {
    static LINE: LazyLock<Vec<u16>> = LazyLock::new(|| {
        let name = crate::runtime::main_image()
            .and_then(crate::runtime::image_path)
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "title.exe".into());
        format!("\"{name}\"").encode_utf16().chain([0]).collect()
    });
    &LINE
}

pub extern "system" fn GetCommandLineW() -> *const u16 {
    command_line().as_ptr()
}

pub extern "system" fn GetCommandLineA() -> *const u8 {
    static LINE: LazyLock<Vec<u8>> = LazyLock::new(|| {
        let text = String::from_utf16_lossy(&command_line()[..command_line().len() - 1]);
        text.into_bytes().into_iter().chain([0]).collect()
    });
    LINE.as_ptr()
}

/// The variable `name` of the process environment, which the C runtime's `getenv` reads too:
/// its own name and its value, matched ignoring case as Windows does.
fn env_entry(name: &str) -> Option<(String, String)> {
    std::env::var(name).ok().map(|v| (name.to_string(), v)).or_else(|| {
        std::env::vars_os()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned()))
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
    })
}

/// `GetEnvironmentVariable`'s result for `value` and the title's buffer of `size` units: the
/// units written without the NUL, or the room the value and its NUL need.
fn put_env_value<T: Copy + Default>(value: Option<Vec<T>>, buf: *mut T, size: u32) -> u32 {
    let Some(v) = value else {
        set_error(ERROR_ENVVAR_NOT_FOUND);
        return 0;
    };
    if buf.is_null() || (size as usize) <= v.len() {
        return v.len() as u32 + 1;
    }
    // SAFETY: the title's buffer of `size` units, which holds the value and its NUL.
    unsafe {
        std::ptr::copy_nonoverlapping(v.as_ptr(), buf, v.len());
        buf.add(v.len()).write(T::default());
    }
    v.len() as u32
}

/// Sets `name` (an existing variable of any case) to `value`, or removes it for `None`.
fn set_env(name: &str, value: Option<&str>) -> i32 {
    // The host's environment takes no `=` in a name (Windows' hidden `=C:` ones included).
    if name.is_empty() || name.contains('=') {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // One title writer at a time, so the name matched ignoring case is still the one set (two
    // threads setting `Foo` and `FOO` would otherwise make two variables).
    static WRITING: Mutex<()> = Mutex::new(());
    let _writing = lock(&WRITING);
    let key = env_entry(name).map_or_else(|| name.to_string(), |(k, _)| k);
    // shortcut: std::env locks around each of its calls, so this cannot race the C runtime's
    // getenv (std::env too) or GetEnvironmentVariable; host C code that calls libc's getenv
    // while a title thread sets a variable still can. Move to a title-private environment if a
    // crash ever points there.
    // SAFETY: the process environment, changed as the title asks, as `_putenv` does.
    unsafe {
        match value {
            Some(v) => std::env::set_var(&key, v),
            None => std::env::remove_var(&key),
        }
    }
    1
}

pub extern "system" fn GetEnvironmentVariableA(name: *const u8, buf: *mut u8, size: u32) -> u32 {
    if name.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: a NUL-terminated name.
    let name = String::from_utf8_lossy(unsafe { units(name, -1) });
    put_env_value(env_entry(&name).map(|(_, v)| v.into_bytes()), buf, size)
}

pub extern "system" fn GetEnvironmentVariableW(name: *const u16, buf: *mut u16, size: u32) -> u32 {
    if name.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: a NUL-terminated name.
    let name = String::from_utf16_lossy(unsafe { units(name, -1) });
    put_env_value(env_entry(&name).map(|(_, v)| v.encode_utf16().collect()), buf, size)
}

pub extern "system" fn SetEnvironmentVariableA(name: *const u8, value: *const u8) -> i32 {
    if name.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: NUL-terminated strings; a null value removes the variable.
    let (name, value) = unsafe {
        (
            String::from_utf8_lossy(units(name, -1)).into_owned(),
            (!value.is_null()).then(|| String::from_utf8_lossy(units(value, -1)).into_owned()),
        )
    };
    set_env(&name, value.as_deref())
}

pub extern "system" fn SetEnvironmentVariableW(name: *const u16, value: *const u16) -> i32 {
    if name.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: NUL-terminated strings; a null value removes the variable.
    let (name, value) = unsafe {
        (
            String::from_utf16_lossy(units(name, -1)),
            (!value.is_null()).then(|| String::from_utf16_lossy(units(value, -1))),
        )
    };
    set_env(&name, value.as_deref())
}

pub extern "system" fn GetModuleHandleExW(
    flags: u32,
    name: *const u16,
    out: *mut *mut c_void,
) -> i32 {
    const FROM_ADDRESS: u32 = 4;
    let module = if flags & FROM_ADDRESS != 0 {
        seh::unwind::image_base_of(name as usize).unwrap_or(0) as *mut c_void
    } else {
        crate::modules::kernel::GetModuleHandleW(name)
    };
    if module.is_null() {
        set_error(ERROR_MOD_NOT_FOUND);
        return 0;
    }
    if !out.is_null() {
        // SAFETY: the title's output word.
        unsafe { out.write(module) };
    }
    1
}

/// `MEMORY_BASIC_INFORMATION` (48 bytes) of the page at `address`.
pub extern "system" fn VirtualQuery(address: *const c_void, out: *mut u8, length: usize) -> usize {
    const MEM_COMMIT: u32 = 0x1000;
    const MEM_RESERVE: u32 = 0x2000;
    const MEM_FREE: u32 = 0x10000;
    const MEM_PRIVATE: u32 = 0x20000;
    if length < 48 || out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let page = mem::page_size();
    let at = address as usize & !(page - 1);
    let (base, size, state, protect) = match mem::next_region(at) {
        Some(r) if r.base <= at => {
            let flags = match r.protect {
                Protect::NO_ACCESS => 0x01,
                Protect::READ => 0x02,
                Protect::READ_WRITE => 0x04,
                Protect::READ_EXECUTE => 0x20,
                _ => 0x40,
            };
            let state = if r.protect == Protect::NO_ACCESS {
                MEM_RESERVE
            } else {
                MEM_COMMIT
            };
            // The allocation: the image or the region of ours around it; the Mach region, a
            // run of one protection, only for memory that is neither.
            let base = seh::unwind::image_base_of(at)
                .or_else(|| super::region_at(at).map(|(base, _)| base))
                .unwrap_or(r.base);
            (base, r.base + r.len - at, state, flags)
        }
        next => (
            at,
            next.map_or(1 << 47, |r| r.base).saturating_sub(at),
            MEM_FREE,
            0x01,
        ),
    };
    // SAFETY: the title's 48-byte structure.
    unsafe {
        std::ptr::write_bytes(out, 0, 48);
        out.cast::<usize>().write_unaligned(at);
        out.add(8)
            .cast::<usize>()
            .write_unaligned(if state == MEM_FREE { 0 } else { base });
        out.add(16)
            .cast::<u32>()
            .write_unaligned(if state == MEM_FREE { 0 } else { protect });
        out.add(24).cast::<usize>().write_unaligned(size);
        out.add(32).cast::<u32>().write_unaligned(state);
        out.add(36)
            .cast::<u32>()
            .write_unaligned(if state == MEM_FREE { 1 } else { protect });
        out.add(40)
            .cast::<u32>()
            .write_unaligned(if state == MEM_FREE { 0 } else { MEM_PRIVATE });
    }
    48
}

pub extern "system" fn VirtualQueryEx(
    _process: isize,
    address: *const c_void,
    out: *mut u8,
    length: usize,
) -> usize {
    VirtualQuery(address, out, length)
}

// Synchronisation objects.

/// A mutex object: owned by a thread, recursive, released by that thread.
pub(crate) struct MutexObject {
    free: Semaphore,
    owner: AtomicU32,
    count: AtomicU32,
}

impl MutexObject {
    pub(super) fn new() -> MutexObject {
        MutexObject {
            free: Semaphore::new(1, 1).expect("semaphore"),
            owner: AtomicU32::new(0),
            count: AtomicU32::new(0),
        }
    }

    /// Takes it for the calling thread (again, if it has it).
    pub(crate) fn acquire(&self, timeout: Option<Duration>) -> bool {
        let me = thread::current_id();
        if self.owner.load(Ordering::Acquire) == me {
            self.count.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        if !self.free.wait(timeout) {
            return false;
        }
        self.owner.store(me, Ordering::Release);
        self.count.store(1, Ordering::Relaxed);
        true
    }

    /// One release by the owner: `Some(true)` if the mutex is now free for
    /// another thread, `Some(false)` if still recursively owned, or `None` if
    /// the caller does not own it.
    pub(crate) fn release(&self) -> Option<bool> {
        if self.owner.load(Ordering::Acquire) != thread::current_id() {
            return None;
        }
        if self.count.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.owner.store(0, Ordering::Release);
            self.free.release(1);
            Some(true)
        } else {
            Some(false)
        }
    }
}

pub extern "system" fn CreateMutexW(
    _attributes: *const c_void,
    initial_owner: i32,
    _name: *const u16,
) -> *mut c_void {
    let mutex = Arc::new(MutexObject::new());
    if initial_owner != 0 {
        mutex.acquire(Some(Duration::ZERO));
    }
    new_handle(Object::Mutex(mutex))
}

pub extern "system" fn CreateMutexA(
    attributes: *const c_void,
    initial_owner: i32,
    _name: *const u8,
) -> *mut c_void {
    CreateMutexW(attributes, initial_owner, std::ptr::null())
}

pub extern "system" fn CreateMutexExW(
    attributes: *const c_void,
    name: *const u16,
    flags: u32,
    _access: u32,
) -> *mut c_void {
    CreateMutexW(attributes, i32::from(flags & 1 != 0), name)
}

pub extern "system" fn ReleaseMutex(handle: *mut c_void) -> i32 {
    let mutex = match lock(&super::HANDLES).get(&(handle as usize)) {
        Some(Object::Mutex(m)) => Some(m.clone()),
        _ => None,
    };
    let Some(mutex) = mutex else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let Some(freed) = mutex.release() else {
        set_error(ERROR_NOT_OWNER);
        return 0;
    };
    if freed {
        super::sync::signalled();
    }
    1
}

pub extern "system" fn CreateEventA(
    attributes: *const c_void,
    manual_reset: i32,
    initial: i32,
    _name: *const u8,
) -> *mut c_void {
    super::CreateEventW(attributes, manual_reset, initial, std::ptr::null())
}

pub extern "system" fn CreateEventExW(
    attributes: *const c_void,
    _name: *const u16,
    flags: u32,
    _access: u32,
) -> *mut c_void {
    // CREATE_EVENT_MANUAL_RESET 1, CREATE_EVENT_INITIAL_SET 2.
    super::CreateEventW(
        attributes,
        i32::from(flags & 1 != 0),
        i32::from(flags & 2 != 0),
        std::ptr::null(),
    )
}

pub extern "system" fn CreateSemaphoreExW(
    attributes: *const c_void,
    initial: i32,
    max: i32,
    _name: *const u16,
    _flags: u32,
    _access: u32,
) -> *mut c_void {
    super::CreateSemaphoreW(attributes, initial, max, std::ptr::null())
}

/// A waitable timer: an event driven by at most one worker thread per timer,
/// with `reset`, `set`, re-arm and cancellation serialised under `state`.
pub(crate) struct TimerObject {
    pub(crate) event: Arc<Event>,
    inner: Arc<TimerInner>,
}

struct TimerInner {
    event: Arc<Event>,
    state: Mutex<TimerState>,
    changed: Condvar,
}

#[derive(Default)]
struct TimerState {
    generation: u64,
    arm: Option<(Instant, Duration)>,
    worker_running: bool,
    closed: bool,
}

impl Drop for TimerObject {
    fn drop(&mut self) {
        let mut st = lock(&self.inner.state);
        st.closed = true;
        st.arm = None;
        self.inner.changed.notify_all();
    }
}

fn timer_worker(inner: Arc<TimerInner>) {
    let mut st = lock(&inner.state);
    loop {
        if st.closed {
            st.worker_running = false;
            return;
        }
        let Some((deadline, period)) = st.arm else {
            st.worker_running = false;
            return;
        };
        let generation = st.generation;
        let now = Instant::now();
        if now >= deadline {
            inner.event.set();
            st.arm = (!period.is_zero()).then(|| (now + period, period));
            drop(st);
            super::sync::signalled();
            st = lock(&inner.state);
            continue;
        }
        st = inner
            .changed
            .wait_timeout_while(st, deadline - now, |s| !s.closed && s.generation == generation)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

pub extern "system" fn CreateWaitableTimerW(
    _attributes: *const c_void,
    manual_reset: i32,
    _name: *const u16,
) -> *mut c_void {
    let Ok(event) = Event::new(manual_reset != 0, false) else {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    };
    let event = Arc::new(event);
    let inner = Arc::new(TimerInner {
        event: event.clone(),
        state: Mutex::default(),
        changed: Condvar::new(),
    });
    new_handle(Object::Timer(Arc::new(TimerObject { event, inner })))
}

pub extern "system" fn CreateWaitableTimerExW(
    attributes: *const c_void,
    name: *const u16,
    flags: u32,
    _access: u32,
) -> *mut c_void {
    CreateWaitableTimerW(attributes, i32::from(flags & 1 != 0), name)
}

/// How long until `due` (negative: relative, 100 ns; positive: an absolute FILETIME).
fn until(due: i64) -> Duration {
    let ticks = if due < 0 {
        due.unsigned_abs()
    } else {
        // shortcut: waits use host time at debug rates, scale wait durations if virtual deadlines must match them.
        (due as u64).saturating_sub(now_ticks())
    };
    Duration::from_nanos(ticks.saturating_mul(100))
}

fn timer_of(handle: *mut c_void) -> Option<Arc<TimerObject>> {
    match lock(&super::HANDLES).get(&(handle as usize)) {
        Some(Object::Timer(t)) => Some(t.clone()),
        _ => None,
    }
}

// shortcut: the completion routine of `SetWaitableTimer` never runs (it
// needs an alertable wait); the event is what titles wait on.
pub extern "system" fn SetWaitableTimer(
    handle: *mut c_void,
    due: *const i64,
    period_ms: i32,
    _routine: *const c_void,
    _arg: *const c_void,
    _resume: i32,
) -> i32 {
    if due.is_null() || period_ms < 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    let Some(timer) = timer_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    // SAFETY: checked non-null; the title's due time.
    let first = until(unsafe { due.read_unaligned() });
    let period = Duration::from_millis(period_ms as u64);
    let now = Instant::now();
    let mut st = lock(&timer.inner.state);
    st.generation = st.generation.wrapping_add(1);
    timer.event.reset();
    let fired_now = first.is_zero();
    if fired_now {
        timer.event.set();
        st.arm = (!period.is_zero()).then(|| (now + period, period));
    } else {
        st.arm = Some((now + first, period));
    }
    if st.arm.is_some() && !st.worker_running {
        st.worker_running = true;
        let inner = timer.inner.clone();
        std::thread::spawn(move || timer_worker(inner));
    } else {
        timer.inner.changed.notify_all();
    }
    drop(st);
    if fired_now {
        super::sync::signalled();
    }
    1
}

pub extern "system" fn SetWaitableTimerEx(
    handle: *mut c_void,
    due: *const i64,
    period_ms: i32,
    routine: *const c_void,
    arg: *const c_void,
    _context: *const c_void,
    _tolerable: u32,
) -> i32 {
    SetWaitableTimer(handle, due, period_ms, routine, arg, 0)
}

pub extern "system" fn CancelWaitableTimer(handle: *mut c_void) -> i32 {
    let Some(timer) = timer_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    let mut st = lock(&timer.inner.state);
    st.generation = st.generation.wrapping_add(1);
    st.arm = None;
    timer.inner.changed.notify_all();
    1
}

// Interlocked singly linked lists: the header is ours (head pointer, depth),
// every access goes through one lock.

static SLISTS: Mutex<()> = Mutex::new(());

pub extern "system" fn InitializeSListHead(head: *mut usize) {
    // SAFETY: a 16-byte SLIST_HEADER.
    unsafe {
        head.write(0);
        head.add(1).write(0);
    }
}

pub extern "system" fn InterlockedPushEntrySList(
    head: *mut usize,
    entry: *mut usize,
) -> *mut usize {
    let _held = lock(&SLISTS);
    // SAFETY: the header and the entry (whose first word links to the next).
    unsafe {
        let first = head.read();
        entry.write(first);
        head.write(entry as usize);
        head.add(1).write(head.add(1).read() + 1);
        first as *mut usize
    }
}

pub extern "system" fn InterlockedPopEntrySList(head: *mut usize) -> *mut usize {
    let _held = lock(&SLISTS);
    // SAFETY: as above.
    unsafe {
        let first = head.read() as *mut usize;
        if !first.is_null() {
            head.write(first.read());
            head.add(1).write(head.add(1).read() - 1);
        }
        first
    }
}

pub extern "system" fn InterlockedFlushSList(head: *mut usize) -> *mut usize {
    let _held = lock(&SLISTS);
    // SAFETY: as above.
    unsafe {
        let first = head.read();
        head.write(0);
        head.add(1).write(0);
        first as *mut usize
    }
}

pub extern "system" fn QueryDepthSList(head: *const usize) -> u16 {
    let _held = lock(&SLISTS);
    // SAFETY: as above.
    unsafe { head.add(1).read() as u16 }
}

/// `InitOnceExecuteOnce`: the word holds 0 (new), 1 (running) or the
/// callback's context with 2 set (done).
pub extern "system" fn InitOnceExecuteOnce(
    once: *mut AtomicU64,
    init: usize,
    parameter: *mut c_void,
    context: *mut *mut c_void,
) -> i32 {
    // SAFETY: the title's INIT_ONCE, a pointer-sized word.
    let word = unsafe { &*once };
    loop {
        match word.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(state) if state & 3 == 2 => {
                if !context.is_null() {
                    // SAFETY: the title's output word.
                    unsafe { context.write((state & !3) as *mut c_void) };
                }
                return 1;
            }
            Err(_) => std::thread::sleep(Duration::from_micros(200)),
        }
    }
    let mut result: *mut c_void = std::ptr::null_mut();
    // SAFETY: the title's own init routine, `BOOL (INIT_ONCE *, PVOID, PVOID *)`.
    let ok = unsafe {
        native::call_win64(
            init,
            &[once as u64, parameter as u64, &raw mut result as u64],
        )
    } as u32
        != 0;
    if ok {
        word.store(result as u64 | 2, Ordering::Release);
        if !context.is_null() {
            // SAFETY: the title's output word.
            unsafe { context.write(result) };
        }
        1
    } else {
        word.store(0, Ordering::Release);
        0
    }
}

/// The flags `InitOnceBeginInitialize` and `InitOnceComplete` take.
const INIT_ONCE_CHECK_ONLY: u32 = 0x1;
const INIT_ONCE_ASYNC: u32 = 0x2;
const INIT_ONCE_INIT_FAILED: u32 = 0x4;

/// Writes a finished one-time initialization's context to the caller's out word.
fn write_once_context(state: u64, context: *mut *mut c_void) {
    if !context.is_null() {
        // SAFETY: the title's output word.
        unsafe { context.write((state & !3) as *mut c_void) };
    }
}

/// `InitOnceBeginInitialize`: claims the run `InitOnceExecuteOnce` would have run, or reports
/// the one that already finished, on the same word: 0 new, 1 running, the context with bit 1
/// set done. With `INIT_ONCE_CHECK_ONLY` nothing is claimed and a run still under way is
/// reported as pending instead of waited for.
///
/// An asynchronous begin hands back the slot the winner's context goes in, which this
/// emulation has nowhere to keep: it says so rather than hand back a slot nobody fills.
pub extern "system" fn InitOnceBeginInitialize(
    once: *mut AtomicU64,
    flags: u32,
    pending: *mut i32,
    context: *mut *mut c_void,
) -> i32 {
    if flags & INIT_ONCE_ASYNC != 0 {
        set_error(ERROR_CALL_NOT_IMPLEMENTED);
        return 0;
    }
    // SAFETY: the title's INIT_ONCE, a pointer-sized word.
    let word = unsafe { &*once };
    if flags & INIT_ONCE_CHECK_ONLY != 0 {
        let state = word.load(Ordering::Acquire);
        if state & 3 != 2 {
            if !pending.is_null() {
                // SAFETY: the title's output word.
                unsafe { pending.write(1) };
            }
            set_error(ERROR_IO_PENDING);
            return 0;
        }
        write_once_context(state, context);
        if !pending.is_null() {
            // SAFETY: as above.
            unsafe { pending.write(0) };
        }
        return 1;
    }
    loop {
        match word.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => {
                if !pending.is_null() {
                    // SAFETY: the title's output word.
                    unsafe { pending.write(1) };
                }
                return 1;
            }
            Err(state) if state & 3 == 2 => {
                write_once_context(state, context);
                if !pending.is_null() {
                    // SAFETY: as above.
                    unsafe { pending.write(0) };
                }
                return 1;
            }
            Err(_) => std::thread::sleep(Duration::from_micros(200)),
        }
    }
}

/// `InitOnceComplete`: stores the context of the run a begin claimed the word for. Only the
/// claim may complete it; a failed attempt hands the word back for the next caller to try.
pub extern "system" fn InitOnceComplete(
    once: *mut AtomicU64,
    flags: u32,
    context: *mut c_void,
) -> i32 {
    if flags & INIT_ONCE_ASYNC != 0 {
        set_error(ERROR_CALL_NOT_IMPLEMENTED);
        return 0;
    }
    // SAFETY: the title's INIT_ONCE, a pointer-sized word.
    let word = unsafe { &*once };
    let done = if flags & INIT_ONCE_INIT_FAILED != 0 {
        0
    } else {
        context as u64 | 2
    };
    if word
        .compare_exchange(1, done, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Nothing was claimed, or the run already finished: this caller never began it.
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    1
}

// Fiber local storage, per thread (a fiber is a thread here).

const FLS_OUT_OF_INDEXES: u32 = 0xFFFF_FFFF;
const FLS_SLOTS: usize = 4096;

static FLS_USED: Mutex<Vec<bool>> = Mutex::new(Vec::new());

/// Bumped each time a slot is handed out: a thread's value from an earlier owner of the slot
/// reads 0, as Windows clears a freed slot in every thread.
static FLS_GENERATION: [AtomicU32; FLS_SLOTS] = [const { AtomicU32::new(0) }; FLS_SLOTS];

thread_local! {
    /// Each slot's value and the generation it was set in.
    static FLS_VALUES: std::cell::RefCell<Vec<(u32, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

// shortcut: the destructor callback is not called (at thread end or `FlsFree`).
pub extern "system" fn FlsAlloc(_callback: usize) -> u32 {
    let mut used = lock(&FLS_USED);
    let free = used.iter().position(|u| !u).unwrap_or(used.len());
    if free >= FLS_SLOTS {
        set_error(ERROR_INVALID_PARAMETER);
        return FLS_OUT_OF_INDEXES;
    }
    if free == used.len() {
        used.push(true);
    }
    used[free] = true;
    FLS_GENERATION[free].fetch_add(1, Ordering::Relaxed);
    free as u32
}

pub extern "system" fn FlsFree(index: u32) -> i32 {
    match lock(&FLS_USED).get_mut(index as usize) {
        Some(used) if *used => {
            *used = false;
            1
        }
        _ => {
            set_error(ERROR_INVALID_PARAMETER);
            0
        }
    }
}

pub extern "system" fn FlsGetValue(index: u32) -> usize {
    set_error(0);
    let Some(generation) = FLS_GENERATION.get(index as usize) else {
        return 0;
    };
    let generation = generation.load(Ordering::Relaxed);
    FLS_VALUES.with(|v| match v.borrow().get(index as usize) {
        Some(&(set_in, value)) if set_in == generation => value,
        _ => 0,
    })
}

pub extern "system" fn FlsSetValue(index: u32, value: usize) -> i32 {
    if !lock(&FLS_USED).get(index as usize).is_some_and(|u| *u) {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    FLS_VALUES.with(|v| {
        let mut v = v.borrow_mut();
        if v.len() <= index as usize {
            v.resize(index as usize + 1, (0, 0));
        }
        v[index as usize] = (FLS_GENERATION[index as usize].load(Ordering::Relaxed), value);
    });
    1
}

// One worker per submission or timer arm. Callbacks use the Windows ABI
// (instance, context, object).
pub(crate) struct PoolObject {
    callback: usize,
    context: usize,
    state: Mutex<PoolState>,
    changed: Condvar,
}

#[derive(Default)]
struct PoolState {
    /// Replacing the deadline retires workers still waiting for it.
    timer_generation: u64,
    /// Canceling queued callbacks invalidates their tickets, including after a rearm.
    cancel_generation: u64,
    queued: u32,
    running: u32,
    /// Workers still blocked on a wait object, waiting for it or for the arming
    /// to be retired. Counted so `WaitForThreadpoolWaitCallbacks` can answer
    /// "nothing of mine is still armed".
    armed: u32,
}

impl PoolState {
    fn queue_callback(&mut self) -> u64 {
        self.queued += 1;
        self.cancel_generation
    }

    fn take_queued(&mut self, ticket: u64) -> bool {
        if ticket != self.cancel_generation {
            return false;
        }
        self.queued -= 1;
        true
    }
}

/// What a callback gets as its instance: nothing it can use, but not null.
static INSTANCE: u64 = 0;

fn new_pool_object(callback: usize, context: usize) -> *mut PoolObject {
    Box::into_raw(Box::new(PoolObject {
        callback,
        context,
        state: Mutex::default(),
        changed: Condvar::new(),
    }))
}

impl PoolObject {
    fn submit(&'static self, state: &mut PoolState, delay: Duration, period: Duration) {
        let generation = state.timer_generation;
        let armed = Instant::now();
        // An immediately due callback is queued before the submitting call returns.
        let mut ticket = delay.is_zero().then(|| state.queue_callback());
        std::thread::spawn(move || {
            let mut state = lock(&self.state);
            let mut delay = delay.saturating_sub(armed.elapsed());
            loop {
                let run = if let Some(epoch) = ticket.take() {
                    state.take_queued(epoch)
                } else {
                    (state, _) = self
                        .changed
                        .wait_timeout_while(state, delay, |s| s.timer_generation == generation)
                        .unwrap_or_else(|e| e.into_inner());
                    if state.timer_generation != generation {
                        return;
                    }
                    // The waiting worker starts the callback directly at expiry.
                    true
                };
                if run {
                    state.running += 1;
                    drop(state);
                    // SAFETY: the title's TP_WORK_CALLBACK or TP_TIMER_CALLBACK.
                    unsafe {
                        native::call_win64(
                            self.callback,
                            &[
                                &raw const INSTANCE as u64,
                                self.context as u64,
                                self as *const Self as u64,
                            ],
                        )
                    };
                    state = lock(&self.state);
                    state.running -= 1;
                    self.changed.notify_all();
                }
                if period.is_zero() || state.timer_generation != generation {
                    return;
                }
                delay = period;
            }
        });
    }

    /// Arms one wait on `object`: the callback runs once when the object is
    /// signalled, or when `timeout` passes, whichever comes first.
    ///
    /// A worker of its own performs the wait, because the title arms a wait and
    /// goes on: the object it waits on may be signalled long after the call
    /// returns. One callback per arming, as Windows does it - a wait on an
    /// object that stays signalled would otherwise call back in a loop nobody
    /// asked for. Arming again, cancelling, or closing retires the wait.
    fn wait_once(
        &'static self,
        state: &mut PoolState,
        object: Waitable,
        timeout: Option<Duration>,
    ) {
        let generation = state.timer_generation;
        state.armed += 1;
        let armed_at = Instant::now();
        std::thread::spawn(move || {
            // In slices, so a disarm need not wait for the object to change.
            const SLICE: Duration = Duration::from_millis(20);
            let deadline = timeout.map(|t| armed_at + t);
            let result = loop {
                if lock(&self.state).timer_generation != generation {
                    break None;
                }
                let slice = match deadline {
                    None => SLICE,
                    Some(deadline) => {
                        let left = deadline.saturating_duration_since(Instant::now());
                        if left.is_zero() {
                            // The title asked for the callback on the timeout too.
                            break Some(WAIT_TIMEOUT);
                        }
                        left.min(SLICE)
                    }
                };
                if object.wait(Some(slice)) {
                    break Some(WAIT_OBJECT_0);
                }
            };
            let mut state = lock(&self.state);
            state.armed -= 1;
            let result = match result {
                Some(result) if state.timer_generation == generation => result,
                _ => {
                    self.changed.notify_all();
                    return;
                }
            };
            state.running += 1;
            self.changed.notify_all();
            drop(state);
            // SAFETY: the title's TP_WAIT_CALLBACK, called as Windows calls it.
            unsafe {
                native::call_win64(
                    self.callback,
                    &[
                        &raw const INSTANCE as u64,
                        self.context as u64,
                        self as *const Self as u64,
                        result as u64,
                    ],
                )
            };
            let mut state = lock(&self.state);
            state.running -= 1;
            self.changed.notify_all();
        });
    }

    fn wait_idle(&self, cancel: bool) {
        let mut state = lock(&self.state);
        if cancel && state.queued != 0 {
            state.cancel_generation = state.cancel_generation.wrapping_add(1);
            state.queued = 0;
            self.changed.notify_all();
        }
        drop(
            self.changed
                .wait_while(state, |s| s.queued != 0 || s.running != 0 || s.armed != 0)
                .unwrap_or_else(|e| e.into_inner()),
        );
    }
}

pub extern "system" fn CreateThreadpoolWork(
    callback: usize,
    context: usize,
    _environment: *const c_void,
) -> *mut PoolObject {
    new_pool_object(callback, context)
}

pub extern "system" fn SubmitThreadpoolWork(work: *mut PoolObject) {
    // SAFETY: pool objects live for the life of the process.
    let o = unsafe { &*work };
    o.submit(&mut lock(&o.state), Duration::ZERO, Duration::ZERO);
}

pub extern "system" fn WaitForThreadpoolWorkCallbacks(work: *mut PoolObject, cancel: i32) {
    // SAFETY: as above.
    unsafe { &*work }.wait_idle(cancel != 0);
}

pub extern "system" fn CloseThreadpoolWork(work: *mut PoolObject) {
    // SAFETY: as above.
    unsafe { &*work }.wait_idle(false);
}

pub extern "system" fn CreateThreadpoolTimer(
    callback: usize,
    context: usize,
    _environment: *const c_void,
) -> *mut PoolObject {
    new_pool_object(callback, context)
}

pub extern "system" fn SetThreadpoolTimer(
    timer: *mut PoolObject,
    due: *const i64,
    period_ms: u32,
    _window_ms: u32,
) {
    // SAFETY: as above.
    let o = unsafe { &*timer };
    let mut state = lock(&o.state);
    state.timer_generation = state.timer_generation.wrapping_add(1);
    o.changed.notify_all();
    if !due.is_null() {
        // SAFETY: the title's due time.
        let delay = until(unsafe { due.read_unaligned() });
        o.submit(
            &mut state,
            delay,
            Duration::from_millis(u64::from(period_ms)),
        );
    }
}

pub extern "system" fn WaitForThreadpoolTimerCallbacks(timer: *mut PoolObject, cancel: i32) {
    // SAFETY: as above.
    unsafe { &*timer }.wait_idle(cancel != 0);
}

pub extern "system" fn CloseThreadpoolTimer(timer: *mut PoolObject) {
    SetThreadpoolTimer(timer, std::ptr::null(), 0, 0);
    WaitForThreadpoolTimerCallbacks(timer, 1);
}

pub extern "system" fn CreateThreadpoolWait(
    callback: usize,
    context: usize,
    _environment: *const c_void,
) -> *mut PoolObject {
    new_pool_object(callback, context)
}

pub extern "system" fn SetThreadpoolWait(
    wait: *mut PoolObject,
    handle: *mut c_void,
    timeout: *const i64,
) {
    // SAFETY: pool objects live for the life of the process.
    let o: &'static PoolObject = unsafe { &*wait };
    let mut state = lock(&o.state);
    // Arming again retires the wait it replaces, as Windows does.
    state.timer_generation = state.timer_generation.wrapping_add(1);
    o.changed.notify_all();
    if handle.is_null() {
        // A null handle disarms the wait and is not an error.
        return;
    }
    let Some(object) = Waitable::of(handle) else {
        // Nothing is armed for a handle that cannot be waited on, and the
        // callback would describe a wait that never happened.
        tracing::warn!("SetThreadpoolWait: {handle:p} is not a waitable object");
        return;
    };
    // SAFETY: the title's optional due time, a relative FILETIME.
    let timeout = (!timeout.is_null()).then(|| until(unsafe { timeout.read_unaligned() }));
    o.wait_once(&mut state, object, timeout);
}

pub extern "system" fn WaitForThreadpoolWaitCallbacks(wait: *mut PoolObject, cancel: i32) {
    // SAFETY: as above.
    let o = unsafe { &*wait };
    if cancel != 0 {
        // Cancelling a wait means the callback will not run: retiring the
        // arming is how the worker blocked on the object hears it.
        let mut state = lock(&o.state);
        state.timer_generation = state.timer_generation.wrapping_add(1);
        o.changed.notify_all();
        drop(state);
    }
    o.wait_idle(false);
}

pub extern "system" fn CloseThreadpoolWait(wait: *mut PoolObject) {
    SetThreadpoolWait(wait, std::ptr::null_mut(), std::ptr::null());
    WaitForThreadpoolWaitCallbacks(wait, 1);
}

/// `ExitProcess`: the process ends with this code. The images' detach runs
/// first, through the exit table registered when the first one attached, which
/// is `ExitProcess`'s order.
pub extern "system" fn ExitProcess(code: u32) -> ! {
    tracing::info!("ExitProcess({code})");
    std::process::exit(code as i32);
}

pub extern "system" fn CallbackMayRunLong(_instance: *const c_void) -> i32 {
    0
}

pub extern "system" fn DisassociateCurrentThreadFromCallback(_instance: *const c_void) {}

pub extern "system" fn FreeLibraryWhenCallbackReturns(
    _instance: *const c_void,
    _module: *mut c_void,
) {
}

// Files the file module has no answer for.

pub extern "system" fn GetFileInformationByHandle(handle: *mut c_void, out: *mut u8) -> i32 {
    let Some(m) = files::metadata_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    use std::os::unix::fs::MetadataExt;
    // BY_HANDLE_FILE_INFORMATION, 52 bytes.
    // SAFETY: the title's structure.
    unsafe {
        let put = |at: usize, v: u32| out.add(at).cast::<u32>().write_unaligned(v);
        let put64 = |at: usize, v: u64| out.add(at).cast::<u64>().write_unaligned(v);
        put(0, files::attributes_of(&m));
        put64(4, files::filetime(m.created()));
        put64(12, files::filetime(m.accessed()));
        put64(20, files::filetime(m.modified()));
        put(28, 1);
        put(32, (m.len() >> 32) as u32);
        put(36, m.len() as u32);
        put(40, m.nlink() as u32);
        put(44, (m.ino() >> 32) as u32);
        put(48, m.ino() as u32);
    }
    1
}

pub extern "system" fn GetFileTime(
    handle: *mut c_void,
    created: *mut u64,
    accessed: *mut u64,
    written: *mut u64,
) -> i32 {
    let Some(m) = files::metadata_of(handle) else {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    };
    for (out, time) in [
        (created, m.created()),
        (accessed, m.accessed()),
        (written, m.modified()),
    ] {
        if !out.is_null() {
            // SAFETY: the title's FILETIME or null.
            unsafe { out.write_unaligned(files::filetime(time)) };
        }
    }
    1
}

// shortcut: times and attributes are not changed (the host's own stay).
pub extern "system" fn SetFileTime(
    _handle: *mut c_void,
    _created: *const u64,
    _accessed: *const u64,
    _written: *const u64,
) -> i32 {
    1
}

pub extern "system" fn SetFileAttributesW(_path: *const u16, _attributes: u32) -> i32 {
    1
}

pub extern "system" fn GetDiskFreeSpaceExW(
    _path: *const u16,
    available: *mut u64,
    total: *mut u64,
    free: *mut u64,
) -> i32 {
    const TB: u64 = 1 << 40;
    for (out, value) in [(available, TB / 2), (total, TB), (free, TB / 2)] {
        if !out.is_null() {
            // SAFETY: the title's output word or null.
            unsafe { out.write_unaligned(value) };
        }
    }
    1
}

pub extern "system" fn GetTempPathW(size: u32, out: *mut u16) -> u32 {
    // The console's temporary files drive.
    let text: Vec<u16> = r"T:\".encode_utf16().chain([0]).collect();
    if (size as usize) < text.len() {
        return text.len() as u32;
    }
    if out.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: the title's buffer of `size` units.
    unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), out, text.len()) };
    (text.len() - 1) as u32
}

pub extern "system" fn MoveFileExW(from: *const u16, to: *const u16, _flags: u32) -> i32 {
    let (Some(from), Some(to)) = (files::wide_path(from), files::wide_path(to)) else {
        set_error(ERROR_FILE_NOT_FOUND);
        return 0;
    };
    match std::fs::rename(&from, &to) {
        Ok(()) => 1,
        Err(e) => {
            set_error(files::win32_error(&e, &from));
            0
        }
    }
}

pub extern "system" fn CreateFile2(
    path: *const u16,
    access: u32,
    share: u32,
    disposition: u32,
    _parameters: *const c_void,
) -> *mut c_void {
    files::CreateFileW(
        path,
        access,
        share,
        std::ptr::null(),
        disposition,
        0,
        std::ptr::null_mut(),
    )
}

pub extern "system" fn CopyFile2(
    from: *const u16,
    to: *const u16,
    _parameters: *const c_void,
) -> i32 {
    let (Some(from), Some(to)) = (files::wide_path(from), files::wide_path(to)) else {
        return 0x8007_0002_u32 as i32; // HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)
    };
    match std::fs::copy(&from, &to) {
        Ok(_) => 0,
        Err(e) => 0x8007_0000_u32 as i32 | files::win32_error(&e, &from) as i32,
    }
}

pub extern "system" fn CreateHardLinkW(
    _link: *const u16,
    _target: *const u16,
    _security: *const c_void,
) -> i32 {
    set_error(ERROR_NOT_SUPPORTED);
    0
}

pub extern "system" fn CreateSymbolicLinkW(
    _link: *const u16,
    _target: *const u16,
    _flags: u32,
) -> u8 {
    set_error(ERROR_NOT_SUPPORTED);
    0
}

// Named pipes: the console has none to connect to.

pub extern "system" fn SetNamedPipeHandleState(
    _pipe: *mut c_void,
    _mode: *const u32,
    _max_collection: *const u32,
    _timeout: *const u32,
) -> i32 {
    set_error(ERROR_INVALID_HANDLE);
    0
}

pub extern "system" fn WaitNamedPipeW(_name: *const u16, _timeout: u32) -> i32 {
    set_error(ERROR_FILE_NOT_FOUND);
    0
}

// Process state notifications: the title is told of nothing.

pub extern "system" fn RegisterAppStateChangeNotification(
    _routine: usize,
    _context: *const c_void,
    registration: *mut *mut c_void,
) -> u32 {
    if !registration.is_null() {
        // SAFETY: the title's output word.
        unsafe { registration.write(0x10 as *mut c_void) };
    }
    0
}

pub extern "system" fn UnregisterAppStateChangeNotification(_registration: *mut c_void) {}

pub extern "system" fn RegisterAppConstrainedChangeNotification(
    routine: usize,
    context: *const c_void,
    registration: *mut *mut c_void,
) -> u32 {
    RegisterAppStateChangeNotification(routine, context, registration)
}

pub extern "system" fn UnregisterAppConstrainedChangeNotification(_registration: *mut c_void) {}

// COM: nothing is registered.

pub extern "system" fn CoInitializeEx(_reserved: *const c_void, _model: u32) -> i32 {
    0
}

pub extern "system" fn CoUninitialize() {}

pub extern "system" fn CoCreateInstance(
    class: *const c_void,
    _outer: *const c_void,
    _context: u32,
    interface: *const c_void,
    out: *mut *mut c_void,
) -> i32 {
    if let Some(hr) = super::wasapi::create(class, interface, out) {
        return hr;
    }
    static SEEN: Mutex<Vec<([u8; 16], [u8; 16])>> = Mutex::new(Vec::new());
    let guid = |p: *const c_void| {
        let mut g = [0u8; 16];
        if !p.is_null() {
            // SAFETY: a 16-byte GUID.
            g.copy_from_slice(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), 16) });
        }
        g
    };
    let key = (guid(class), guid(interface));
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&key) {
        seen.push(key);
        tracing::info!(class = ?key.0, interface = ?key.1, "CoCreateInstance: class not registered");
    }
    drop(seen);
    if !out.is_null() {
        // SAFETY: the title's output pointer.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    0x8004_0154_u32 as i32 // REGDB_E_CLASSNOTREG
}

/// `CoTaskMemAlloc`: a block from the process heap, as `LocalAlloc` gives one.
///
/// `CoTaskMemFree` keeps nothing to free (see its note), so what this allocates
/// is not returned.
pub extern "system" fn CoTaskMemAlloc(bytes: usize) -> *mut c_void {
    if bytes == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    // SAFETY: a non-zero size; the block is the title's.
    unsafe { libc::malloc(bytes) }
}

/// `CoGetApartmentType`: every thread of this host is in the multithreaded
/// apartment, which is the model `CoInitializeEx` is answered with.
pub extern "system" fn CoGetApartmentType(kind: *mut u32, qualifier: *mut u32) -> i32 {
    const APTTYPE_MTA: u32 = 1;
    const APTTYPEQUALIFIER_IMPLICIT_MTA: u32 = 1;
    // SAFETY: the title's out words, each optional.
    unsafe {
        if !kind.is_null() {
            kind.write(APTTYPE_MTA);
        }
        if !qualifier.is_null() {
            qualifier.write(APTTYPEQUALIFIER_IMPLICIT_MTA);
        }
    }
    0
}

/// The count `CoAddRefServerProcess` keeps: nothing here shuts down with it, so
/// it is a count of callers and no more.
static SERVER_REFS: AtomicU32 = AtomicU32::new(0);

pub extern "system" fn CoAddRefServerProcess() -> u32 {
    SERVER_REFS.fetch_add(1, Ordering::AcqRel) + 1
}

pub extern "system" fn CoReleaseServerProcess() -> u32 {
    SERVER_REFS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            Some(n.saturating_sub(1))
        })
        .map_or(0, |before| before.saturating_sub(1))
}

/// The object-context calls are answered as a machine with no COM+ context
/// answers them: there is no context to describe, and saying so is the answer a
/// caller checks for.
pub extern "system" fn CoGetObjectContext(_interface: *const c_void, out: *mut *mut c_void) -> i32 {
    if !out.is_null() {
        // SAFETY: the title's out pointer.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    0x8000_4002_u32 as i32 // E_NOINTERFACE
}

pub extern "system" fn CoGetContextToken(out: *mut usize) -> i32 {
    if !out.is_null() {
        // SAFETY: the title's out word.
        unsafe { out.write(0) };
    }
    0x8000_4002_u32 as i32 // E_NOINTERFACE
}

/// The free-threaded marshaler and the stream marshaling are refused with
/// `E_NOTIMPL`, which is a legal answer for a class that does not aggregate one:
/// a refused hand-off is visible, while a marshaler that marshals nothing would
/// hand the other apartment a pointer to it that does not work.
pub extern "system" fn CoCreateFreeThreadedMarshaler(
    _outer: *mut c_void,
    out: *mut *mut c_void,
) -> i32 {
    tracing::warn!("CoCreateFreeThreadedMarshaler: no marshaler is created here");
    if !out.is_null() {
        // SAFETY: the title's out pointer.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    0x8000_4001_u32 as i32 // E_NOTIMPL
}

pub extern "system" fn CoMarshalInterThreadInterfaceInStream(
    _interface: *const c_void,
    _unknown: *mut c_void,
    out: *mut *mut c_void,
) -> i32 {
    tracing::warn!("CoMarshalInterThreadInterfaceInStream: interfaces are not marshaled here");
    if !out.is_null() {
        // SAFETY: the title's out pointer.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    0x8000_4001_u32 as i32 // E_NOTIMPL
}

pub extern "system" fn CoGetInterfaceAndReleaseStream(
    _stream: *mut c_void,
    _interface: *const c_void,
    out: *mut *mut c_void,
) -> i32 {
    tracing::warn!("CoGetInterfaceAndReleaseStream: no stream was made here");
    if !out.is_null() {
        // SAFETY: the title's out pointer.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    0x8000_4001_u32 as i32 // E_NOTIMPL
}

// WinRT: initialization, the apartment, and the errors a call's failure carries.

/// The model this thread was initialised with, and how many times: `RoInitialize`
/// answers `S_FALSE` for the same model again and `RPC_E_CHANGED_MODE` for a
/// different one, as COM does.
thread_local! {
    static RO_APARTMENT: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

pub extern "system" fn RoInitialize(model: u32) -> i32 {
    const RO_INIT_MULTITHREADED: u32 = 1;
    const S_FALSE: i32 = 1;
    const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;
    // This host has one apartment, so a request for it is honoured and a request
    // for another model is told the model changed.
    if model != RO_INIT_MULTITHREADED {
        return RPC_E_CHANGED_MODE;
    }
    RO_APARTMENT.with(|apartment| match apartment.get() {
        Some(_) => S_FALSE,
        None => {
            apartment.set(Some(model));
            0
        }
    })
}

pub extern "system" fn RoUninitialize() {
    RO_APARTMENT.with(|apartment| apartment.set(None));
}

/// `RoGetApartmentIdentifier`: the one apartment, which every thread is in.
pub extern "system" fn RoGetApartmentIdentifier(out: *mut u64) -> i32 {
    if out.is_null() {
        return 0x8000_4003_u32 as i32; // E_POINTER
    }
    // SAFETY: the title's out word.
    unsafe { out.write(1) };
    0
}

/// `RoRegisterForApartmentShutdown` and its inverse: nothing shuts an apartment
/// down here, so the registration is answered and never called back.
pub extern "system" fn RoRegisterForApartmentShutdown(
    _callback: *const c_void,
    _context: *mut c_void,
    registration: *mut u64,
) -> i32 {
    if registration.is_null() {
        return 0x8000_4003_u32 as i32; // E_POINTER
    }
    static NEXT: AtomicU64 = AtomicU64::new(1);
    // SAFETY: the title's out registration id.
    unsafe { registration.write(NEXT.fetch_add(1, Ordering::Relaxed)) };
    0
}

pub extern "system" fn RoUnregisterForApartmentShutdown(_registration: u64) {}

/// The activation factories: no runtime class is implemented by this host, and
/// the name is logged so a title asking for one says which.
pub extern "system" fn RoGetActivationFactory(
    class: usize,
    _interface: *const c_void,
    out: *mut *mut c_void,
) -> i32 {
    if !out.is_null() {
        // SAFETY: the title's out pointer.
        unsafe { out.write(std::ptr::null_mut()) };
    }
    // SAFETY: the caller's `HSTRING`.
    let name = unsafe {
        let mut len = 0u32;
        let data = super::winrt::WindowsGetStringRawBuffer(class as *mut u8, &mut len);
        String::from_utf16_lossy(std::slice::from_raw_parts(data, len as usize))
    };
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&name) {
        seen.push(name.clone());
        tracing::info!(class = %name, "RoGetActivationFactory: no such runtime class");
    }
    drop(seen);
    0x8004_0154_u32 as i32 // REGDB_E_CLASSNOTREG
}

pub extern "system" fn RoRegisterActivationFactories(
    _classes: *const usize,
    _factories: *const usize,
    count: u32,
    cookie: *mut u32,
) -> i32 {
    if cookie.is_null() {
        return 0x8000_4003_u32 as i32; // E_POINTER
    }
    static NEXT: AtomicU32 = AtomicU32::new(1);
    tracing::info!(count, "RoRegisterActivationFactories: activation is not served here");
    // SAFETY: the title's out cookie.
    unsafe { cookie.write(NEXT.fetch_add(1, Ordering::Relaxed)) };
    0
}

pub extern "system" fn RoRevokeActivationFactories(_cookie: u32) {}

/// The error a failed WinRT call carries: the message is kept per thread and
/// logged at once, so a failure inside the title is readable in the log even
/// though no `IRestrictedErrorInfo` object is handed out.
thread_local! {
    static RO_ERROR: std::cell::RefCell<Option<(i32, String)>> = const { std::cell::RefCell::new(None) };
}

fn record_error(code: i32, message: usize) -> i32 {
    // SAFETY: the caller's optional `HSTRING`.
    let text = unsafe {
        let mut len = 0u32;
        let data = super::winrt::WindowsGetStringRawBuffer(message as *mut u8, &mut len);
        String::from_utf16_lossy(std::slice::from_raw_parts(data, len as usize))
    };
    tracing::warn!("WinRT error {code:#010x}: {text}");
    RO_ERROR.with(|slot| *slot.borrow_mut() = Some((code, text)));
    1
}

pub extern "system" fn RoOriginateError(code: i32, message: usize) -> i32 {
    record_error(code, message)
}

pub extern "system" fn RoTransformError(old: i32, new: i32, message: usize) -> i32 {
    let _ = old;
    record_error(new, message)
}

/// No `IRestrictedErrorInfo` object exists here, so setting one is refused and
/// getting one answers nothing: `S_FALSE` is what Windows answers when there is
/// no error to report.
pub extern "system" fn SetRestrictedErrorInfo(_info: *mut c_void) -> i32 {
    0x8000_4001_u32 as i32 // E_NOTIMPL
}

pub extern "system" fn GetRestrictedErrorInfo(out: *mut *mut c_void) -> i32 {
    if out.is_null() {
        return 0x8000_4003_u32 as i32; // E_POINTER
    }
    // SAFETY: the title's out pointer.
    unsafe { out.write(std::ptr::null_mut()) };
    1 // S_FALSE: there is none
}

pub extern "system" fn CoTaskMemFree(_memory: *mut c_void) {}

pub extern "system" fn CoCreateGuid(out: *mut u8) -> i32 {
    // SAFETY: a 16-byte GUID.
    unsafe {
        arc4random_buf(out.cast(), 16);
        // Version 4, variant 1.
        *out.add(7) = *out.add(7) & 0x0F | 0x40;
        *out.add(8) = *out.add(8) & 0x3F | 0x80;
    }
    0
}

pub extern "system" fn StringFromGUID2(guid: *const u8, out: *mut u16, size: i32) -> i32 {
    if guid.is_null() || out.is_null() || size <= 0 {
        return 0;
    }
    // SAFETY: a 16-byte GUID.
    let g = unsafe { std::slice::from_raw_parts(guid, 16) };
    let text = format!(
        "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
        u32::from_le_bytes([g[0], g[1], g[2], g[3]]),
        u16::from_le_bytes([g[4], g[5]]),
        u16::from_le_bytes([g[6], g[7]]),
        g[8],
        g[9],
        g[10],
        g[11],
        g[12],
        g[13],
        g[14],
        g[15]
    );
    let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
    if (size as usize) < units.len() {
        return 0;
    }
    // SAFETY: the title's buffer of `size` units.
    unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), out, units.len()) };
    units.len() as i32
}

pub extern "system" fn PropVariantClear(variant: *mut u8) -> i32 {
    // SAFETY: a 24-byte PROPVARIANT; clearing leaves VT_EMPTY.
    unsafe { std::ptr::write_bytes(variant, 0, 24) };
    0
}

// Registry: no keys, but the one the title reads its device type from (see
// `modules::xsystem`).

const HKEY_LOCAL_MACHINE: isize = 0x8000_0002_u32 as i32 as isize;
const OEM_KEY: &str = r"Software\Microsoft\Windows NT\CurrentVersion\OEM";
/// Stands for the opened OEM key.
static OEM: u8 = 0;

fn wide_is(text: *const u16, expected: &str) -> bool {
    if text.is_null() {
        return false;
    }
    // SAFETY: the title passes a NUL-terminated string.
    let text = unsafe {
        let len = (0..).take_while(|&i| *text.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
    };
    text.eq_ignore_ascii_case(expected)
}

fn open_key(key: isize, sub: *const u16, result: *mut *mut c_void) -> i32 {
    if key == HKEY_LOCAL_MACHINE && wide_is(sub, OEM_KEY) && !result.is_null() {
        // SAFETY: the title passes room for a key.
        unsafe { result.write((&raw const OEM).cast_mut().cast()) };
        return 0;
    }
    ERROR_FILE_NOT_FOUND as i32
}

pub extern "system" fn RegOpenKeyExW(
    key: isize,
    sub_key: *const u16,
    _options: u32,
    _access: u32,
    result: *mut *mut c_void,
) -> i32 {
    open_key(key, sub_key, result)
}

pub extern "system" fn RegOpenKeyW(
    key: isize,
    sub_key: *const u16,
    result: *mut *mut c_void,
) -> i32 {
    open_key(key, sub_key, result)
}

pub extern "system" fn RegQueryValueExW(
    key: isize,
    name: *const u16,
    _reserved: *const u32,
    kind: *mut u32,
    data: *mut u8,
    size: *mut u32,
) -> i32 {
    if key != (&raw const OEM) as isize || !wide_is(name, "ConsoleMode") {
        return ERROR_FILE_NOT_FOUND as i32;
    }
    const ERROR_MORE_DATA: i32 = 234;
    // SAFETY: the title passes valid or null pointers, sized by `size`.
    unsafe {
        if !kind.is_null() {
            kind.write(4); // REG_DWORD
        }
        if size.is_null() {
            return 0;
        }
        let room = size.read();
        size.write(4);
        if data.is_null() {
            return 0;
        }
        if room < 4 {
            return ERROR_MORE_DATA;
        }
        data.cast::<u32>().write_unaligned(crate::modules::xsystem::console_mode(
            victory_gpu::console::current(),
        ));
    }
    0
}

pub extern "system" fn RegCloseKey(_key: isize) -> i32 {
    0
}

// ETW: no listeners, so every event is dropped.

pub extern "system" fn EventRegister(
    _provider: *const c_void,
    _callback: usize,
    _context: *const c_void,
    handle: *mut u64,
) -> u32 {
    if !handle.is_null() {
        // SAFETY: the title's REGHANDLE.
        unsafe { handle.write(0) };
    }
    0
}

pub extern "system" fn EventUnregister(_handle: u64) -> u32 {
    0
}

pub extern "system" fn EventSetInformation(
    _handle: u64,
    _class: u32,
    _info: *const c_void,
    _len: u32,
) -> u32 {
    0
}

pub extern "system" fn EventWriteTransfer(
    _handle: u64,
    _descriptor: *const c_void,
    _activity: *const c_void,
    _related: *const c_void,
    _count: u32,
    _data: *const c_void,
) -> u32 {
    0
}

// Classic (WPP) tracing. A provider registers, and from then on every gate
// reports what a machine with no trace session started reports: no logger, no
// enable level, no flags. None of it is a stub, so a runtime that traces and
// then reads those gates finds them answered rather than zeroed by the
// fallback.

/// Stands for a registration; never a host handle value.
static TRACE_REGISTRATION: u8 = 0;

pub extern "system" fn RegisterTraceGuidsW(
    _request: *const c_void,
    _context: *const c_void,
    control: *const c_void,
    count: u32,
    _trace: *const c_void,
    _mof_path: *const u16,
    _mof_resource: *const u16,
    handle: *mut u64,
) -> u32 {
    if control.is_null() || count == 0 || handle.is_null() {
        return ERROR_INVALID_PARAMETER;
    }
    // SAFETY: the title's out pointer; a registration it hands back later.
    unsafe { handle.write((&raw const TRACE_REGISTRATION) as u64) };
    0
}

pub extern "system" fn UnregisterTraceGuids(_registration: u64) -> u32 {
    0
}

/// `NULL`: no logger, so the caller traces nothing.
pub extern "system" fn GetTraceLoggerHandle(_buffer: *const c_void) -> u64 {
    0
}

pub extern "system" fn GetTraceEnableLevel(_logger: u64) -> u8 {
    0
}

pub extern "system" fn GetTraceEnableFlags(_logger: u64) -> u32 {
    0
}

/// Variadic: the messages after `number` are not read, since there is no
/// logger to hand them to.
pub extern "system" fn TraceMessage(
    _logger: u64,
    _flags: u32,
    _guid: *const c_void,
    _number: u16,
) -> u32 {
    0
}

// bcrypt.

pub extern "system" fn BCryptOpenAlgorithmProvider(
    handle: *mut *mut c_void,
    _algorithm: *const u16,
    _implementation: *const u16,
    _flags: u32,
) -> i32 {
    // SAFETY: the title's output word. Only the RNG is real: the handle
    // is an opaque non-null value.
    unsafe { handle.write(0x20 as *mut c_void) };
    0
}

pub extern "system" fn BCryptCloseAlgorithmProvider(_handle: *mut c_void, _flags: u32) -> i32 {
    0
}

pub extern "system" fn BCryptGenRandom(
    _algorithm: *mut c_void,
    buffer: *mut u8,
    len: u32,
    _flags: u32,
) -> i32 {
    // SAFETY: the title's buffer of `len` bytes.
    unsafe { arc4random_buf(buffer.cast(), len as usize) };
    0
}

// crypt32: no certificates.

pub extern "system" fn CertOpenSystemStoreW(_provider: usize, _store: *const u16) -> *mut c_void {
    set_error(ERROR_FILE_NOT_FOUND);
    std::ptr::null_mut()
}

pub extern "system" fn CertCloseStore(_store: *mut c_void, _flags: u32) -> i32 {
    1
}

pub extern "system" fn CertFindChainInStore(
    _store: *mut c_void,
    _encoding: u32,
    _flags: u32,
    _kind: u32,
    _parameter: *const c_void,
    _previous: *const c_void,
) -> *const c_void {
    set_error(ERROR_FILE_NOT_FOUND);
    std::ptr::null()
}

pub extern "system" fn CertFreeCertificateChain(_chain: *const c_void) {}

// The same answer for the store a library opens by provider rather than by
// name, and for what it looks up in it: this host has no certificate store, so
// there is nothing to open and nothing to find. Saying so is what a machine
// without the store does; a handle nobody can read would have the caller
// believe it has certificates.

pub extern "system" fn CertOpenStore(
    _provider: *const u8,
    _encoding: u32,
    _crypt: usize,
    _flags: u32,
    _para: *const c_void,
) -> *mut c_void {
    set_error(ERROR_FILE_NOT_FOUND);
    std::ptr::null_mut()
}

pub extern "system" fn CertFindCertificateInStore(
    _store: *mut c_void,
    _encoding: u32,
    _flags: u32,
    _kind: u32,
    _to_find: *const c_void,
    _previous: *const c_void,
) -> *const c_void {
    set_error(ERROR_FILE_NOT_FOUND);
    std::ptr::null()
}

pub extern "system" fn CertFreeCertificateContext(_context: *const c_void) -> i32 {
    1
}

// The crypt32 string codecs. `XCurl` decodes the user and password out of the
// proxy URL before it builds the auth header, so this one has to be right
// rather than plausible: a wrong decode corrupts the header silently.

const CRYPT_STRING_BASE64: u32 = 0x1;
const CRYPT_STRING_BINARY: u32 = 0x2;
const CRYPT_STRING_BASE64HEADER: u32 = 0x4;
const CRYPT_STRING_BASE64REQUEST: u32 = 0x8;
const CRYPT_STRING_BASE64_ANY: u32 = 0x6;
const ERROR_INVALID_DATA: u32 = 13;
const ERROR_MORE_DATA: u32 = 234;

/// What the text encodes: the bytes, which encoding it turned out to be, and how
/// many characters of preamble were skipped to reach it.
type Decoded = (Vec<u8>, u32, u32);

fn decode_crypt_string(text: &[u8], flags: u32) -> Result<Decoded, u32> {
    // The format is a value, not a bit: `CRYPT_STRING_BASE64_ANY` is six, so a
    // bit test would read it as binary.
    match flags & 0xF {
        CRYPT_STRING_BINARY => return Ok((text.to_vec(), CRYPT_STRING_BINARY, 0)),
        CRYPT_STRING_BASE64
        | CRYPT_STRING_BASE64HEADER
        | CRYPT_STRING_BASE64REQUEST
        | CRYPT_STRING_BASE64_ANY => {}
        // Hex and the other encodings are not what this title asks for, and a
        // decoder guessed at is worse than a refusal that shows up in a log.
        other => {
            tracing::warn!("CryptStringToBinary: flags {other:#x} are not answered");
            return Err(ERROR_INVALID_PARAMETER);
        }
    }
    let armored = flags & (CRYPT_STRING_BASE64HEADER | CRYPT_STRING_BASE64REQUEST | CRYPT_STRING_BASE64_ANY) != 0;
    let body_of = |text: &[u8]| -> (Vec<u8>, u32) {
        if !armored {
            return (text.to_vec(), 0);
        }
        // Line by line: the dashes of the armor and the header lines before the
        // body are not part of the encoded data, and a body line holds nothing
        // but the alphabet.
        let (mut body, mut skipped, mut at) = (Vec::new(), 0u32, 0u32);
        for line in text.split(|&b| b == b'\n') {
            let holds_body = line.iter().all(|&b| {
                b.is_ascii_alphanumeric()
                    || matches!(b, b'+' | b'/' | b'=' | b'\r' | b'\t' | b' ')
            }) && line.iter().any(|&b| !matches!(b, b'\r' | b'\t' | b' '));
            if holds_body {
                if body.is_empty() {
                    let lead = line
                        .iter()
                        .position(|&b| !matches!(b, b'\r' | b'\t' | b' '))
                        .unwrap_or(0);
                    skipped = at + lead as u32;
                }
                body.extend_from_slice(line);
            }
            at += line.len() as u32 + 1;
        }
        (body, skipped)
    };
    let (body, skipped) = body_of(text);
    let bytes = base64_body(&body)?;
    // Which flavour the text turned out to be: the armor is what the dashes say.
    let opened_with_dashes = text
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| *b == b'-');
    let used = if opened_with_dashes {
        CRYPT_STRING_BASE64HEADER
    } else {
        CRYPT_STRING_BASE64
    };
    Ok((bytes, used, skipped))
}

/// RFC 4648 base64: four characters to three bytes, `=` once at the end, and
/// whitespace anywhere as the encoder writes it at line ends.
fn base64_body(body: &[u8]) -> Result<Vec<u8>, u32> {
    let mut out = Vec::with_capacity(body.len() / 4 * 3 + 3);
    let (mut bits, mut held) = (0u32, 0u32);
    let mut padded = false;
    for &b in body {
        let value = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padded = true;
                continue;
            }
            b' ' | b'\t' | b'\r' | b'\n' => continue,
            // Not an alphabet character and not padding: this is not base64.
            _ => return Err(ERROR_INVALID_DATA),
        };
        if padded {
            // Data after the padding is not a base64 string.
            return Err(ERROR_INVALID_DATA);
        }
        bits = (bits << 6) | u32::from(value);
        held += 6;
        if held >= 8 {
            held -= 8;
            out.push((bits >> held) as u8);
        }
    }
    // Six bits left over is one character of a group of four, not a byte.
    if held >= 6 {
        return Err(ERROR_INVALID_DATA);
    }
    Ok(out)
}

pub extern "system" fn CryptStringToBinaryW(
    string: *const u16,
    length: u32,
    flags: u32,
    binary: *mut u8,
    size: *mut u32,
    skip: *mut u32,
    used: *mut u32,
) -> i32 {
    if string.is_null() || size.is_null() {
        set_error(ERROR_INVALID_PARAMETER);
        return 0;
    }
    // SAFETY: the title's string, `length` characters or NUL-terminated.
    let wide = unsafe {
        let mut n = 0usize;
        if length == u32::MAX {
            while *string.add(n) != 0 {
                n += 1;
            }
        } else {
            n = length as usize;
        }
        std::slice::from_raw_parts(string, n)
    };
    // Every encoding answered here is ASCII, so a character above that is not
    // one of them whatever the flags say.
    let mut text = Vec::with_capacity(wide.len());
    for &c in wide {
        match u8::try_from(c) {
            Ok(b) => text.push(b),
            Err(_) => {
                set_error(ERROR_INVALID_DATA);
                return 0;
            }
        }
    }
    let (bytes, reported, skipped) = match decode_crypt_string(&text, flags) {
        Ok(decoded) => decoded,
        Err(error) => {
            set_error(error);
            return 0;
        }
    };
    let needed = bytes.len() as u32;
    // SAFETY: the title's out parameters, each optional.
    unsafe {
        if !skip.is_null() {
            skip.write(skipped);
        }
        if !used.is_null() {
            used.write(reported);
        }
        let room = *size;
        size.write(needed);
        if binary.is_null() {
            // The size query: how much a buffer would need.
            return 1;
        }
        if room < needed {
            set_error(ERROR_MORE_DATA);
            return 0;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), binary, bytes.len());
    }
    1
}

/// `RaiseFailFastException`: the process ends here, which is what fail-fast
/// means — the record's code becomes the exit status, and nothing after the
/// call runs (no unwinding, no exit handlers, as on Windows).
pub extern "system" fn RaiseFailFastException(
    record: *const c_void,
    _context: *const c_void,
    _flags: u32,
) {
    /// `STATUS_FAIL_FAST_EXCEPTION`, when the caller hands no record.
    const STATUS_FAIL_FAST_EXCEPTION: u32 = 0xC000_0409;
    let code = if record.is_null() {
        STATUS_FAIL_FAST_EXCEPTION
    } else {
        // SAFETY: the title's `EXCEPTION_RECORD`, whose first field is its code.
        unsafe { record.cast::<u32>().read() }
    };
    tracing::error!("the title raised a fail-fast exception, code {code:#010x}");
    std::process::exit(code as i32);
}

// user32: the console has one window of the title's own; these are for
// libraries that make a message window and pump it.

static WINDOWS: AtomicU64 = AtomicU64::new(1);

pub extern "system" fn RegisterClassExA(_class: *const c_void) -> u16 {
    0xC001
}

pub extern "system" fn CreateWindowExA(
    _ex_style: u32,
    _class: *const u8,
    _name: *const u8,
    _style: u32,
    _x: i32,
    _y: i32,
    _width: i32,
    _height: i32,
    _parent: *mut c_void,
    _menu: *mut c_void,
    _instance: *mut c_void,
    _param: *mut c_void,
) -> *mut c_void {
    ((WINDOWS.fetch_add(1, Ordering::Relaxed) as usize) << 4) as *mut c_void
}

pub extern "system" fn DefWindowProcA(
    _window: *mut c_void,
    _message: u32,
    _w: usize,
    _l: isize,
) -> isize {
    0
}

pub extern "system" fn ShowWindow(_window: *mut c_void, _command: i32) -> i32 {
    0
}

pub extern "system" fn PeekMessageA(
    _message: *mut c_void,
    _window: *mut c_void,
    _min: u32,
    _max: u32,
    _remove: u32,
) -> i32 {
    0
}

pub extern "system" fn TranslateMessage(_message: *const c_void) -> i32 {
    0
}

pub extern "system" fn DispatchMessageA(_message: *const c_void) -> isize {
    0
}

/// The function table of the DLLs this module answers for.
pub(super) fn lookup(dll: &str, name: &str) -> Option<usize> {
    macro_rules! table {
        ($($f:ident),* $(,)?) => {
            match name {
                $(stringify!($f) => Some($f as *const () as usize),)*
                _ => None,
            }
        };
    }
    // The registry lives in `advapi32.dll` below, but a DLL built against the
    // API sets imports it from `api-ms-win-core-registry-*`, which the generic
    // set table does not carry: stubbed, `RegOpenKeyExW` returns 0, which the
    // caller reads as success with a null key in hand.
    if dll.starts_with("api-ms-win-core-registry-") {
        return table!(RegOpenKeyExW, RegOpenKeyW, RegQueryValueExW, RegCloseKey);
    }
    // Classic (WPP) tracing is where a DLL built against the API sets announces
    // itself; the generic set table below carries none of it. Stubbed,
    // `RegisterTraceGuidsW` returns 0 — "registered", with a null registration
    // — and every gate after it reads a logger that never existed.
    if dll == "api-ms-win-eventing-classicprovider-l1-1-0.dll" {
        return table!(
            RegisterTraceGuidsW,
            UnregisterTraceGuids,
            GetTraceLoggerHandle,
            GetTraceEnableLevel,
            GetTraceEnableFlags,
            TraceMessage,
        );
    }
    let kernel = dll == "kernel32.dll"
        || dll == "kernelbase.dll"
        || dll.starts_with("api-ms-win-core-")
        || dll.starts_with("api-ms-win-eventing-");
    if kernel {
        return table!(
            MultiByteToWideChar,
            WideCharToMultiByte,
            lstrcmpW,
            CompareStringEx,
            GetStringTypeW,
            GetStringTypeExW,
            FindResourceExW,
            LoadResource,
            GetCPInfo,
            LCMapStringEx,
            GetUserDefaultLocaleName,
            GetUserDefaultGeoName,
            GetMaximumProcessorCount,
            GetActiveProcessorGroupCount,
            GetProcessIdOfThread,
            SetThreadDescription,
            GetThreadDescription,
            GetUserDefaultUILanguage,
            GetSystemTimeAdjustment,
            GetLogicalProcessorInformationEx,
            GetConsoleWindow,
            GetConsoleScreenBufferInfo,
            WriteConsoleW,
            QueueUserWorkItem,
            InitializeProcThreadAttributeList,
            UpdateProcThreadAttribute,
            DeleteProcThreadAttributeList,
            FlushProcessWriteBuffers,
            LocalAlloc,
            lstrlenW,
            lstrcmpiW,
            LocaleNameToLCID,
            GetLocaleInfoEx,
            FormatMessageA,
            FormatMessageW,
            LocalFree,
            GlobalFree,
            EncodePointer,
            DecodePointer,
            GetSystemTimeAsFileTime,
            GetSystemTimePreciseAsFileTime,
            GetSystemTime,
            GetLocalTime,
            FileTimeToSystemTime,
            SystemTimeToFileTime,
            FileTimeToLocalFileTime,
            LocalFileTimeToFileTime,
            GetDateFormatEx,
            GetTimeFormatEx,
            GetNativeSystemInfo,
            GetSystemInfo,
            GlobalMemoryStatusEx,
            GetLogicalProcessorInformation,
            IsProcessorFeaturePresent,
            IsDebuggerPresent,
            UnhandledExceptionFilter,
            RaiseFailFastException,
            RtlCaptureStackBackTrace,
            RtlUnwind,
            WerRegisterFile,
            GetCurrentProcess,
            GetCurrentThread,
            GetProcessId,
            GetThreadPriority,
            SetThreadPriority,
            GetThreadPriorityBoost,
            SetThreadPriorityBoost,
            SetThreadStackGuarantee,
            GetActiveProcessorCount,
            GetCurrentProcessorNumber,
            SetProcessPriorityBoost,
            GetErrorMode,
            SetErrorMode,
            SetThreadErrorMode,
            SetPriorityClass,
            GetPriorityClass,
            IsWow64Process2,
            QueryFullProcessImageNameW,
            SetThreadIdealProcessorEx,
            GetThreadGroupAffinity,
            SetThreadGroupAffinity,
            SetThreadAffinityMask,
            GetCurrentThreadStackLimits,
            GetStartupInfoW,
            GetCommandLineW,
            GetCommandLineA,
            GetEnvironmentVariableA,
            GetEnvironmentVariableW,
            SetEnvironmentVariableA,
            SetEnvironmentVariableW,
            GetModuleHandleExW,
            VirtualQuery,
            VirtualQueryEx,
            CreateMutexW,
            CreateMutexA,
            CreateMutexExW,
            ReleaseMutex,
            CreateEventA,
            CreateEventExW,
            CreateSemaphoreExW,
            CreateWaitableTimerW,
            CreateWaitableTimerExW,
            SetWaitableTimer,
            SetWaitableTimerEx,
            CancelWaitableTimer,
            InitializeSListHead,
            InterlockedPushEntrySList,
            InterlockedPopEntrySList,
            InterlockedFlushSList,
            QueryDepthSList,
            InitOnceExecuteOnce,
            InitOnceBeginInitialize,
            InitOnceComplete,
            FlsAlloc,
            FlsFree,
            FlsGetValue,
            FlsSetValue,
            CreateThreadpoolWork,
            SubmitThreadpoolWork,
            WaitForThreadpoolWorkCallbacks,
            CloseThreadpoolWork,
            CreateThreadpoolTimer,
            SetThreadpoolTimer,
            WaitForThreadpoolTimerCallbacks,
            CloseThreadpoolTimer,
            ExitProcess,
            CreateThreadpoolWait,
            SetThreadpoolWait,
            WaitForThreadpoolWaitCallbacks,
            CloseThreadpoolWait,
            CallbackMayRunLong,
            DisassociateCurrentThreadFromCallback,
            FreeLibraryWhenCallbackReturns,
            GetFileInformationByHandle,
            GetFileTime,
            SetFileTime,
            SetFileAttributesW,
            GetDiskFreeSpaceExW,
            GetTempPathW,
            MoveFileExW,
            CreateFile2,
            CopyFile2,
            CreateHardLinkW,
            CreateSymbolicLinkW,
            SetNamedPipeHandleState,
            WaitNamedPipeW,
            RegisterAppStateChangeNotification,
            UnregisterAppStateChangeNotification,
            RegisterAppConstrainedChangeNotification,
            UnregisterAppConstrainedChangeNotification,
            CoInitializeEx,
            CoUninitialize,
            CoCreateInstance,
            CoTaskMemAlloc,
            CoGetApartmentType,
            CoAddRefServerProcess,
            CoReleaseServerProcess,
            CoGetObjectContext,
            CoGetContextToken,
            CoCreateFreeThreadedMarshaler,
            CoMarshalInterThreadInterfaceInStream,
            CoGetInterfaceAndReleaseStream,
            RoInitialize,
            RoUninitialize,
            RoGetApartmentIdentifier,
            RoRegisterForApartmentShutdown,
            RoUnregisterForApartmentShutdown,
            RoGetActivationFactory,
            RoRegisterActivationFactories,
            RoRevokeActivationFactories,
            RoOriginateError,
            RoTransformError,
            SetRestrictedErrorInfo,
            GetRestrictedErrorInfo,
            CoTaskMemFree,
            CoCreateGuid,
            StringFromGUID2,
            PropVariantClear,
            EventRegister,
            EventUnregister,
            EventSetInformation,
            EventWriteTransfer,
        );
    }
    if dll == "advapi32.dll" || dll == "api-ms-win-eventing-provider-l1-1-0.dll" {
        return table!(
            RegOpenKeyExW,
            RegOpenKeyW,
            RegQueryValueExW,
            RegCloseKey,
            RegisterTraceGuidsW,
            UnregisterTraceGuids,
            GetTraceLoggerHandle,
            GetTraceEnableLevel,
            GetTraceEnableFlags,
            TraceMessage,
            EventRegister,
            EventUnregister,
            EventSetInformation,
            EventWriteTransfer,
        );
    }
    if dll == "bcrypt.dll" {
        return table!(
            BCryptOpenAlgorithmProvider,
            BCryptCloseAlgorithmProvider,
            BCryptGenRandom
        );
    }
    if dll == "crypt32.dll" {
        return table!(
            CertOpenSystemStoreW,
            CertOpenStore,
            CertCloseStore,
            CryptStringToBinaryW,
            CertFindChainInStore,
            CertFindCertificateInStore,
            CertFreeCertificateContext,
            CertFreeCertificateChain
        );
    }
    if dll.starts_with("ext-ms-win-rtcore-ntuser-") || dll == "user32.dll" {
        return table!(
            RegisterClassExA,
            CreateWindowExA,
            DefWindowProcA,
            ShowWindow,
            PeekMessageA,
            TranslateMessage,
            DispatchMessageA,
        );
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `amd_fidelityfx_upscaler_xbox.dll` imports by these names resolves (#123's run).
    #[test]
    fn the_upscaler_dll_finds_its_debug_localization_and_math_imports() {
        for (dll, name) in [
            ("api-ms-win-core-debug-l1-1-0.dll", "DebugBreak"),
            ("api-ms-win-core-localization-l1-2-0.dll", "FormatMessageW"),
            ("api-ms-win-crt-math-l1-1-0.dll", "_ldsign"),
        ] {
            assert!(super::super::lookup(dll, name).is_some(), "{dll}!{name}");
        }
    }

    /// `FormatMessageW` as a library logs with it: system texts (from an `HRESULT` too), the
    /// inserts of a string with an argument array or a `va_list`, an allocated buffer, and the
    /// documented failures.
    #[test]
    fn format_message_w_formats_system_errors_and_inserts() {
        const SYSTEM: u32 = 0x1000 | 0x200; // FROM_SYSTEM | IGNORE_INSERTS
        let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
        let format = |flags, source: &[u16], id, args: *const c_void| {
            let mut out = [0u16; 128];
            let n = FormatMessageW(
                flags,
                source.as_ptr().cast(),
                id,
                0,
                out.as_mut_ptr(),
                128,
                args,
            );
            (n, String::from_utf16_lossy(&out[..n as usize]))
        };
        assert_eq!(
            format(SYSTEM, &[0], 5, std::ptr::null()),
            (19, "Access is denied.\r\n".into())
        );
        let missing = format(SYSTEM, &[0], 0x8007_0002, std::ptr::null()).1;
        assert_eq!(missing, "The system cannot find the file specified.\r\n");
        assert_eq!(
            format(SYSTEM, &[0], 0xDEAD, std::ptr::null()).1,
            "Error 0x0000DEAD\r\n"
        );
        assert_eq!(
            format(SYSTEM | 0xFF, &[0], 5, std::ptr::null()).1,
            "Access is denied."
        );

        // %1 is a wide string by default, %2!d! an int; %n, %t, %% and %0 are escapes.
        let name = wide("dll");
        let args = [name.as_ptr() as u64, u64::from(-3i32 as u32)];
        let source = wide("%1%t%2!d!%n100%%%0 never");
        let expanded = "dll\t-3\r\n100%";
        assert_eq!(
            format(0x400 | 0x2000, &source, 0, args.as_ptr().cast()).1,
            expanded
        );
        let list = args.as_ptr();
        assert_eq!(
            format(0x400, &source, 0, (&raw const list).cast()).1,
            expanded,
            "va_list"
        );
        let kept = format(0x400 | 0x200, &wide("%1!x! %n"), 0, std::ptr::null()).1;
        assert_eq!(kept, "%1!x! \r\n", "IGNORE_INSERTS keeps the inserts");

        let mut allocated: *mut u16 = std::ptr::null_mut();
        let n = FormatMessageW(
            SYSTEM | 0x100,
            std::ptr::null(),
            6,
            0,
            (&raw mut allocated).cast(),
            0,
            std::ptr::null(),
        );
        // SAFETY: `n` units and a NUL, leaked for good.
        let text = unsafe { std::slice::from_raw_parts(allocated, n as usize + 1) };
        assert_eq!(
            String::from_utf16_lossy(text),
            "The handle is invalid.\r\n\0"
        );

        let mut small = [0u16; 4];
        assert_eq!(
            FormatMessageW(
                SYSTEM,
                std::ptr::null(),
                5,
                0,
                small.as_mut_ptr(),
                4,
                std::ptr::null()
            ),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_INSUFFICIENT_BUFFER);
        assert_eq!(
            format(0x800, &[0], 5, std::ptr::null()).0,
            0,
            "FROM_HMODULE"
        );
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(
            format(0x400 | 0x2000, &wide("%1!f!"), 0, args.as_ptr().cast()).0,
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn environment_variables_are_set_read_and_removed_ignoring_case() {
        // The AMD upscaler imports them through this API set.
        let api = "api-ms-win-core-processenvironment-l1-1-0.dll";
        assert!(lookup(api, "GetEnvironmentVariableA").is_some());
        assert!(lookup(api, "SetEnvironmentVariableA").is_some());
        let (name, lower) = (b"VICTORY_TEST_ENV_5E2B\0", b"victory_test_env_5e2b\0");
        let get = |size: u32| {
            let mut buf = [b'x'; 8];
            let n = GetEnvironmentVariableA(lower.as_ptr(), buf.as_mut_ptr(), size);
            (n, buf)
        };
        assert_eq!(SetEnvironmentVariableA(name.as_ptr(), c"abc".as_ptr().cast()), 1);
        // The C runtime's getenv reads the same environment.
        assert_eq!(std::env::var("VICTORY_TEST_ENV_5E2B").as_deref(), Ok("abc"));
        assert_eq!(get(8), (3, *b"abc\0xxxx"));
        // Too small for the value and its NUL: the room needed, nothing written.
        assert_eq!(get(3), (4, *b"xxxxxxxx"));
        assert_eq!(GetEnvironmentVariableA(name.as_ptr(), std::ptr::null_mut(), 0), 4);
        let wide: Vec<u16> = "victory_test_env_5e2b".encode_utf16().chain([0]).collect();
        let mut out = [0xFFFFu16; 8];
        assert_eq!(GetEnvironmentVariableW(wide.as_ptr(), out.as_mut_ptr(), 8), 3);
        assert_eq!(out[..4], [97, 98, 99, 0]);
        // Another case names the same variable.
        let value: Vec<u16> = "xyzw".encode_utf16().chain([0]).collect();
        assert_eq!(SetEnvironmentVariableW(wide.as_ptr(), value.as_ptr()), 1);
        assert_eq!(std::env::var("VICTORY_TEST_ENV_5E2B").as_deref(), Ok("xyzw"));
        assert!(std::env::var("victory_test_env_5e2b").is_err());
        assert_eq!(SetEnvironmentVariableA(name.as_ptr(), c"".as_ptr().cast()), 1);
        assert_eq!(get(8), (0, *b"\0xxxxxxx"));
        // A null value removes it.
        assert_eq!(SetEnvironmentVariableA(name.as_ptr(), std::ptr::null()), 1);
        assert_eq!(get(8), (0, *b"xxxxxxxx"));
        assert_eq!(super::super::GetLastError(), ERROR_ENVVAR_NOT_FOUND);
        assert_eq!(SetEnvironmentVariableA(c"".as_ptr().cast(), c"v".as_ptr().cast()), 0);
        assert_eq!(SetEnvironmentVariableA(c"A=B".as_ptr().cast(), c"v".as_ptr().cast()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn file_times_and_system_times_round_trip() {
        // 2024-02-29 13:45:07.250, a Thursday.
        let st: SystemTimeFields = [2024, 2, 4, 29, 13, 45, 7, 250];
        let ticks = from_system_time(&st).unwrap();
        assert_eq!(to_system_time(ticks), st);
        // The Unix epoch is 116444736000000000 ticks after 1601.
        assert_eq!(
            from_system_time(&[1970, 1, 4, 1, 0, 0, 0, 0]),
            Some(EPOCH_DIFFERENCE)
        );
        assert_eq!(from_system_time(&[2024, 13, 0, 1, 0, 0, 0, 0]), None);
    }

    #[test]
    fn strings_convert_and_report_the_size_needed() {
        let text = b"h\xC3\xA9llo\0";
        let n = MultiByteToWideChar(65001, 0, text.as_ptr(), -1, std::ptr::null_mut(), 0);
        assert_eq!(n, 6);
        let mut wide = [0u16; 6];
        assert_eq!(
            MultiByteToWideChar(65001, 0, text.as_ptr(), -1, wide.as_mut_ptr(), 6),
            6
        );
        assert_eq!(wide[1], 0xE9);
        let mut small = [0u16; 2];
        assert_eq!(
            MultiByteToWideChar(65001, 0, text.as_ptr(), -1, small.as_mut_ptr(), 2),
            0
        );
        let mut back = [0u8; 8];
        assert_eq!(
            WideCharToMultiByte(
                65001,
                0,
                wide.as_ptr(),
                6,
                back.as_mut_ptr(),
                8,
                std::ptr::null(),
                std::ptr::null_mut()
            ),
            7
        );
        assert_eq!(&back[..7], b"h\xC3\xA9llo\0");
    }

    #[test]
    fn a_mutex_is_recursive_and_only_its_owner_releases_it() {
        let m = Arc::new(MutexObject::new());
        assert!(m.acquire(None));
        assert!(m.acquire(None));
        let other = m.clone();
        let blocked = std::thread::spawn(move || other.acquire(Some(Duration::from_millis(20))));
        assert!(!blocked.join().unwrap());
        assert_eq!(m.release(), Some(false));
        assert_eq!(m.release(), Some(true));
        assert_eq!(m.release(), None);
        let other = m.clone();
        assert!(
            std::thread::spawn(move || other.acquire(Some(Duration::ZERO)))
                .join()
                .unwrap()
        );
    }

    #[test]
    fn interlocked_lists_are_last_in_first_out() {
        let mut head = [0usize; 2];
        let mut a = [0usize; 2];
        let mut b = [0usize; 2];
        InterlockedPushEntrySList(head.as_mut_ptr(), a.as_mut_ptr());
        InterlockedPushEntrySList(head.as_mut_ptr(), b.as_mut_ptr());
        assert_eq!(QueryDepthSList(head.as_ptr()), 2);
        assert_eq!(InterlockedPopEntrySList(head.as_mut_ptr()), b.as_mut_ptr());
        assert_eq!(InterlockedFlushSList(head.as_mut_ptr()), a.as_mut_ptr());
        assert!(InterlockedPopEntrySList(head.as_mut_ptr()).is_null());
    }

    /// `AllocationBase` is the base `VirtualAlloc` returned, not where a run of one protection
    /// starts.
    #[test]
    fn virtual_query_names_the_allocation_base() {
        let g = super::super::GRANULARITY;
        let p = super::super::VirtualAlloc(std::ptr::null_mut(), 2 * g, 0x3000, 0x04);
        assert!(!p.is_null());
        let mut old = 0;
        let second = p.cast::<u8>().wrapping_add(g).cast();
        assert_eq!(super::super::VirtualProtect(second, g, 0x02, &mut old), 1);
        let mut info = [0u8; 48];
        assert_eq!(VirtualQuery(second, info.as_mut_ptr(), 48), 48);
        let field = |at: usize| usize::from_le_bytes(info[at..at + 8].try_into().unwrap());
        assert_eq!((field(0), field(8)), (second as usize, p as usize));
        assert_eq!(super::super::VirtualFree(p, 0, 0x8000), 1);
    }

    #[test]
    fn fiber_local_storage_is_per_thread() {
        let slot = FlsAlloc(0);
        assert_ne!(slot, FLS_OUT_OF_INDEXES);
        assert_eq!(FlsSetValue(slot, 7), 1);
        let other = std::thread::spawn(move || FlsGetValue(slot))
            .join()
            .unwrap();
        assert_eq!((FlsGetValue(slot), other), (7, 0));
        assert_eq!(FlsFree(slot), 1);
        assert_eq!(FlsSetValue(slot, 1), 0);
    }

    /// An FLS slot freed and handed out again reads 0 in every thread, not what it held before.
    #[test]
    fn a_reused_fls_slot_reads_zero_in_every_thread() {
        let slot = FlsAlloc(0);
        assert_ne!(slot, FLS_OUT_OF_INDEXES);
        let (set, go) = (std::sync::mpsc::channel(), std::sync::mpsc::channel::<u32>());
        let (set_tx, go_rx) = (set.0, go.1);
        let other = std::thread::spawn(move || {
            assert_eq!(FlsSetValue(slot, 5), 1);
            set_tx.send(()).unwrap();
            FlsGetValue(go_rx.recv().unwrap())
        });
        set.1.recv().unwrap();
        assert_eq!(FlsFree(slot), 1);
        let again = FlsAlloc(0);
        assert_ne!(again, FLS_OUT_OF_INDEXES);
        go.0.send(again).unwrap();
        assert_eq!(other.join().unwrap(), 0);
        assert_eq!(FlsFree(again), 1);
    }

    const ONE_HOUR_DUE: i64 = -3600 * TICKS_PER_SECOND as i64;

    #[test]
    fn disarming_a_threadpool_timer_does_not_wait_for_its_deadline() {
        static CALLS: AtomicU32 = AtomicU32::new(0);
        extern "C" fn count(_instance: usize, _context: usize, _timer: usize) {
            CALLS.fetch_add(1, Ordering::Relaxed);
        }

        let (done, result) = std::sync::mpsc::channel();
        let callback = native::host_callbacks(&[(count as *const () as usize, false)]).unwrap()[0];
        std::thread::spawn(move || {
            let timer = CreateThreadpoolTimer(callback, 0, std::ptr::null());
            SetThreadpoolTimer(timer, &ONE_HOUR_DUE, 0, 0);
            SetThreadpoolTimer(timer, std::ptr::null(), 0, 0);
            WaitForThreadpoolTimerCallbacks(timer, 0);
            CloseThreadpoolTimer(timer);
            let _ = done.send(());
        });
        result.recv_timeout(Duration::from_millis(750)).unwrap();
        assert_eq!(CALLS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn canceling_a_queued_timer_callback_preserves_the_armed_deadline() {
        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            extern "C" fn unused(_instance: usize, _context: usize, _timer: usize) {}
            let callback =
                native::host_callbacks(&[(unused as *const () as usize, false)]).unwrap()[0];
            let timer = CreateThreadpoolTimer(callback, 0, std::ptr::null());
            SetThreadpoolTimer(timer, &ONE_HOUR_DUE, 0, 0);
            WaitForThreadpoolTimerCallbacks(timer, 0);

            // Hold a submission between queueing and dispatch without racing a worker thread.
            // SAFETY: the allocated timer lives through close.
            let o = unsafe { &*timer };
            let (generation, ticket) = {
                let mut state = lock(&o.state);
                (state.timer_generation, state.queue_callback())
            };
            WaitForThreadpoolTimerCallbacks(timer, 1);
            {
                let mut state = lock(&o.state);
                assert_eq!(state.timer_generation, generation);
                assert!(!state.take_queued(ticket));
            }
            CloseThreadpoolTimer(timer);
            let _ = done.send(());
        });
        result.recv_timeout(Duration::from_millis(750)).unwrap();
    }

    #[test]
    fn a_threadpool_timer_waits_for_a_running_callback_after_rearm() {
        use std::sync::mpsc::{self, RecvTimeoutError};

        struct Gate {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
        }
        extern "C" fn block(_instance: usize, context: usize, _timer: usize) {
            // SAFETY: the test keeps this gate alive through close.
            let gate = unsafe { &*(context as *const Gate) };
            let _ = gate.entered.send(());
            let _ = lock(&gate.release).recv();
        }

        let callback = native::host_callbacks(&[(block as *const () as usize, false)]).unwrap()[0];
        for cancel in [0, 1] {
            let (entered, started) = mpsc::channel();
            let (release, resume) = mpsc::channel();
            let gate = Box::leak(Box::new(Gate {
                entered,
                release: Mutex::new(resume),
            }));
            let timer =
                CreateThreadpoolTimer(callback, gate as *const Gate as usize, std::ptr::null());
            SetThreadpoolTimer(timer, &ONE_HOUR_DUE, 0, 0);
            SetThreadpoolTimer(timer, &0, 0, 0);
            started.recv_timeout(Duration::from_secs(1)).unwrap();
            SetThreadpoolTimer(timer, std::ptr::null(), 0, 0);

            let (waiting, ready) = mpsc::sync_channel(0);
            let (done, result) = mpsc::channel();
            let handle = timer as usize;
            let waiter = std::thread::spawn(move || {
                waiting.send(()).unwrap();
                WaitForThreadpoolTimerCallbacks(handle as *mut PoolObject, cancel);
                let _ = done.send(());
            });
            ready.recv().unwrap();
            let before_release = result.recv_timeout(Duration::from_millis(50));
            release.send(()).unwrap();
            assert_eq!(before_release, Err(RecvTimeoutError::Timeout));
            result.recv_timeout(Duration::from_secs(1)).unwrap();
            waiter.join().unwrap();
            CloseThreadpoolTimer(timer);
        }
    }

    #[test]
    fn a_waitable_timer_signals_when_due() {
        let timer = CreateWaitableTimerW(std::ptr::null(), 1, std::ptr::null());
        let due = -(TICKS_PER_SECOND as i64) / 100; // 10 ms
        assert_eq!(
            SetWaitableTimer(timer, &due, 0, std::ptr::null(), std::ptr::null(), 0),
            1
        );
        assert_eq!(super::super::WaitForSingleObject(timer, 5000), 0);
        let start = std::time::Instant::now();
        assert_eq!(CancelWaitableTimer(timer), 1);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(super::super::CloseHandle(timer), 1);
    }

    #[test]
    fn rearming_and_canceling_waitable_timers_retire_old_deadlines_cleanly() {
        assert_eq!(
            SetWaitableTimer(std::ptr::null_mut(), &0, 0, std::ptr::null(), std::ptr::null(), 0),
            0
        );
        let timer = CreateWaitableTimerW(std::ptr::null(), 0, std::ptr::null());
        assert_eq!(
            SetWaitableTimer(timer, std::ptr::null(), 0, std::ptr::null(), std::ptr::null(), 0),
            0
        );
        assert_eq!(
            SetWaitableTimer(timer, &0, -1, std::ptr::null(), std::ptr::null(), 0),
            0
        );

        // Re-arming from a short deadline to a far deadline must prevent the superseded short
        // deadline from signalling the reset event.
        let short_due = -((TICKS_PER_SECOND as i64) * 15 / 1000); // 15 ms
        assert_eq!(
            SetWaitableTimer(timer, &short_due, 0, std::ptr::null(), std::ptr::null(), 0),
            1
        );
        assert_eq!(
            SetWaitableTimer(timer, &ONE_HOUR_DUE, 0, std::ptr::null(), std::ptr::null(), 0),
            1
        );
        assert_eq!(
            super::super::WaitForSingleObject(timer, 35),
            super::super::WAIT_TIMEOUT
        );

        // Re-arming from a far deadline down to a short periodic deadline wakes the existing
        // worker immediately, fires at `due = 0` and on period, and stops on cancel + close.
        assert_eq!(
            SetWaitableTimer(timer, &0, 10, std::ptr::null(), std::ptr::null(), 0),
            1
        );
        assert_eq!(super::super::WaitForSingleObject(timer, 0), 0);
        assert_eq!(super::super::WaitForSingleObject(timer, 5000), 0);
        assert_eq!(CancelWaitableTimer(timer), 1);
        let _ = super::super::WaitForSingleObject(timer, 0);
        assert_eq!(
            super::super::WaitForSingleObject(timer, 30),
            super::super::WAIT_TIMEOUT
        );

        // Closing an armed timer wakes and retires its worker thread promptly.
        assert_eq!(
            SetWaitableTimer(timer, &ONE_HOUR_DUE, 0, std::ptr::null(), std::ptr::null(), 0),
            1
        );
        let inner = timer_of(timer).unwrap().inner.clone();
        assert_eq!(super::super::CloseHandle(timer), 1);
        let deadline = Instant::now() + Duration::from_secs(1);
        while lock(&inner.state).worker_running && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!lock(&inner.state).worker_running);
    }

    #[test]
    fn wait_for_threadpool_work_callbacks_cancels_queued_submissions_when_requested() {
        static CALLS: AtomicU32 = AtomicU32::new(0);
        extern "C" fn bump(_instance: usize, _context: usize, _work: usize) {
            CALLS.fetch_add(1, Ordering::Relaxed);
        }

        let (done, result) = std::sync::mpsc::channel();
        let callback = native::host_callbacks(&[(bump as *const () as usize, false)]).unwrap()[0];
        std::thread::spawn(move || {
            let work = CreateThreadpoolWork(callback, 0, std::ptr::null());
            SubmitThreadpoolWork(work);
            WaitForThreadpoolWorkCallbacks(work, 0);
            assert_eq!(CALLS.load(Ordering::Relaxed), 1);

            // Hold a queued work ticket and cancel pending callbacks via `cancel = 1`.
            // SAFETY: `work` lives until `CloseThreadpoolWork` below.
            let o = unsafe { &*work };
            let ticket = lock(&o.state).queue_callback();
            WaitForThreadpoolWorkCallbacks(work, 1);
            assert!(!lock(&o.state).take_queued(ticket));
            assert_eq!(CALLS.load(Ordering::Relaxed), 1);

            CloseThreadpoolWork(work);
            let _ = done.send(());
        });
        result.recv_timeout(Duration::from_millis(750)).unwrap();
    }

    #[test]
    fn system_time_rejects_pre_1601_and_invalid_calendar_dates() {
        let mut ft = 0u64;
        for bad in [
            [1600, 12, 0, 31, 0, 0, 0, 0],
            [0, 1, 0, 1, 0, 0, 0, 0],
            [2023, 2, 0, 29, 0, 0, 0, 0],
            [1900, 2, 0, 29, 0, 0, 0, 0],
            [2024, 4, 0, 31, 0, 0, 0, 0],
            [30828, 1, 0, 1, 0, 0, 0, 0],
        ] {
            assert_eq!(SystemTimeToFileTime(&bad, &mut ft), 0, "accepted {bad:?}");
            assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        }
        // Leap-year 2000-02-29 and epoch 1601-01-01 are valid.
        assert_eq!(SystemTimeToFileTime(&[1601, 1, 0, 1, 0, 0, 0, 0], &mut ft), 1);
        assert_eq!(ft, 0);
        assert_eq!(SystemTimeToFileTime(&[2000, 2, 0, 29, 0, 0, 0, 0], &mut ft), 1);
        assert_eq!(SystemTimeToFileTime(std::ptr::null(), &mut ft), 0);
        assert_eq!(FileTimeToSystemTime(&ft, std::ptr::null_mut()), 0);
    }

    #[test]
    fn string_guid_and_processor_helpers_guard_edge_cases() {
        // LCMapStringEx with len < 0 includes the terminating NUL.
        let src: Vec<u16> = "AbC\0".encode_utf16().collect();
        let mut out = [0xFFFFu16; 8];
        let n = LCMapStringEx(
            std::ptr::null(),
            LCMAP_LOWERCASE,
            src.as_ptr(),
            -1,
            out.as_mut_ptr(),
            8,
            std::ptr::null(),
            std::ptr::null(),
            0,
        );
        assert_eq!(n, 4);
        assert_eq!(&out[..4], &[b'a' as u16, b'b' as u16, b'c' as u16, 0]);
        assert_eq!(
            LCMapStringEx(
                std::ptr::null(),
                LCMAP_LOWERCASE,
                src.as_ptr(),
                -1,
                out.as_mut_ptr(),
                -1,
                std::ptr::null(),
                std::ptr::null(),
                0,
            ),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);

        // WideCharToMultiByte reports used_default and honors a custom default byte.
        let wide: Vec<u16> = "A\u{20AC}B".encode_utf16().collect();
        let mut mb = [0u8; 8];
        let mut used = 0i32;
        let def = b'#';
        assert_eq!(
            WideCharToMultiByte(1252, 0, wide.as_ptr(), 3, mb.as_mut_ptr(), 8, &def, &mut used),
            3
        );
        assert_eq!((&mb[..3], used), (&b"A#B"[..], 1));

        // Negative buffer size in copy_out / StringFromGUID2 and null buffer in GetLogicalProcessorInformation.
        assert_eq!(
            MultiByteToWideChar(65001, 0, c"hi".as_ptr().cast(), -1, out.as_mut_ptr(), -1),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        let guid = [0u8; 16];
        assert_eq!(StringFromGUID2(guid.as_ptr(), out.as_mut_ptr(), -1), 0);
        assert_eq!(StringFromGUID2(std::ptr::null(), out.as_mut_ptr(), 64), 0);

        let mut len = 1024u32;
        assert_eq!(GetLogicalProcessorInformation(std::ptr::null_mut(), &mut len), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INSUFFICIENT_BUFFER);
        assert!(len >= 64);
        assert_eq!(GetLogicalProcessorInformation(std::ptr::null_mut(), std::ptr::null_mut()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }
}

#[cfg(test)]
mod console_mode_tests {
    use super::*;

    #[test]
    fn device_type_key_answers_console_mode() {
        let w = |s: &str| -> Vec<u16> { s.encode_utf16().chain([0]).collect() };
        let mut key = std::ptr::null_mut();
        assert_eq!(RegOpenKeyW(HKEY_LOCAL_MACHINE, w("software\\other").as_ptr(), &mut key), 2);
        assert_eq!(RegOpenKeyW(HKEY_LOCAL_MACHINE, w(OEM_KEY).as_ptr(), &mut key), 0);
        let (mut kind, mut value, mut size) = (0u32, 0u32, 4u32);
        let got = RegQueryValueExW(
            key as isize,
            w("ConsoleMode").as_ptr(),
            std::ptr::null(),
            &mut kind,
            (&raw mut value).cast(),
            &mut size,
        );
        assert_eq!((got, kind, size), (0, 4, 4));
        assert_eq!(value, crate::modules::xsystem::console_mode(victory_gpu::console::current()));
    }

    #[test]
    fn the_api_set_that_names_the_registry_answers_as_advapi32_does() {
        // A DLL built against the API sets — `XCurl.dll` is one — imports the
        // registry from `api-ms-win-core-registry-*`, not from `advapi32.dll`.
        // Stubbed, `RegOpenKeyExW` returns 0, which the caller reads as success
        // with a null key in hand.
        for dll in [
            "api-ms-win-core-registry-l1-1-0.dll",
            "api-ms-win-core-registry-l2-1-0.dll",
        ] {
            for name in [
                "RegOpenKeyExW",
                "RegOpenKeyW",
                "RegQueryValueExW",
                "RegCloseKey",
            ] {
                let found = super::lookup(dll, name);
                assert!(found.is_some(), "{dll}!{name} is not answered");
                assert_eq!(found, super::lookup("advapi32.dll", name), "{dll}!{name}");
            }
        }
    }
}

#[cfg(test)]
mod trace_tests {
    use super::*;

    #[test]
    fn classic_tracing_registers_and_then_reports_no_logger() {
        for name in [
            "RegisterTraceGuidsW",
            "UnregisterTraceGuids",
            "GetTraceLoggerHandle",
            "GetTraceEnableLevel",
            "GetTraceEnableFlags",
            "TraceMessage",
        ] {
            assert!(
                super::lookup("api-ms-win-eventing-classicprovider-l1-1-0.dll", name).is_some(),
                "{name} is not answered"
            );
        }
        let control = 0u8;
        let mut registration = 0u64;
        assert_eq!(
            RegisterTraceGuidsW(
                std::ptr::null(),
                std::ptr::null(),
                (&raw const control).cast(),
                1,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                &mut registration,
            ),
            0
        );
        // A registration is a non-null token, and every gate that follows says
        // "no trace session", which is what this host has.
        assert_ne!(registration, 0);
        assert_eq!(UnregisterTraceGuids(registration), 0);
        assert_eq!(GetTraceLoggerHandle(std::ptr::null()), 0);
        assert_eq!(GetTraceEnableLevel(registration), 0);
        assert_eq!(GetTraceEnableFlags(registration), 0);
        assert_eq!(TraceMessage(0, 0, std::ptr::null(), 1), 0);
        // Nothing to register is refused, as the API refuses it.
        assert_eq!(
            RegisterTraceGuidsW(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                &mut registration,
            ),
            ERROR_INVALID_PARAMETER
        );
    }
}

#[cfg(test)]
mod cert_and_fail_fast_tests {
    use super::*;

    #[test]
    fn a_store_that_does_not_exist_is_reported_as_missing() {
        // `XCurl` opens the store by provider rather than by name, and looks a
        // certificate up in it. This host has no store: saying so is what a
        // machine without one does, and a handle nobody can read would have the
        // caller believe it has certificates.
        for name in [
            "CertOpenStore",
            "CertFindCertificateInStore",
            "CertFreeCertificateContext",
        ] {
            assert!(
                lookup("crypt32.dll", name).is_some(),
                "{name} is not answered"
            );
        }
        assert!(CertOpenStore(std::ptr::null(), 0, 0, 0, std::ptr::null()).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_FILE_NOT_FOUND);
        assert!(
            CertFindCertificateInStore(
                std::ptr::null_mut(),
                0,
                0,
                0,
                std::ptr::null(),
                std::ptr::null()
            )
            .is_null()
        );
        assert_eq!(super::super::GetLastError(), ERROR_FILE_NOT_FOUND);
        // Freeing a certificate nobody has is not an error.
        assert_eq!(CertFreeCertificateContext(std::ptr::null()), 1);
    }

    #[test]
    fn the_fail_fast_call_is_answered_rather_than_stubbed() {
        // Not called: it ends the process, which is the point of it.
        assert!(
            lookup(
                "api-ms-win-core-errorhandling-l1-1-2.dll",
                "RaiseFailFastException"
            )
            .is_some()
        );
    }
}

#[cfg(test)]
mod threadpool_wait_tests {
    use super::*;

    /// 100 ms and an hour in the unit a due time is given in: 100 ns per tick.
    const HUNDRED_MS: i64 = -1_000_000;
    const ONE_HOUR: i64 = -3600 * TICKS_PER_SECOND as i64;

    /// A wait the title armed on an auto-reset event runs its callback once the
    /// event is signalled, and reports what it saw.
    #[test]
    fn a_signalled_object_runs_the_callback_once() {
        static CALLS: AtomicU32 = AtomicU32::new(0);
        static SEEN: AtomicU32 = AtomicU32::new(u32::MAX);
        extern "C" fn note(_instance: usize, _context: usize, _wait: usize, result: u32) {
            SEEN.store(result, Ordering::Relaxed);
            CALLS.fetch_add(1, Ordering::Relaxed);
        }

        let callback = native::host_callbacks(&[(note as *const () as usize, false)]).unwrap()[0];
        let event = super::super::CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
        assert!(!event.is_null());
        let wait = CreateThreadpoolWait(callback, 0, std::ptr::null());
        SetThreadpoolWait(wait, event, std::ptr::null());
        assert_eq!(super::super::SetEvent(event), 1);
        WaitForThreadpoolWaitCallbacks(wait, 0);
        assert_eq!(
            (CALLS.load(Ordering::Relaxed), SEEN.load(Ordering::Relaxed)),
            (1, WAIT_OBJECT_0)
        );

        // One callback per arming: a signal with nothing armed is not a callback.
        assert_eq!(super::super::SetEvent(event), 1);
        WaitForThreadpoolWaitCallbacks(wait, 0);
        assert_eq!(CALLS.load(Ordering::Relaxed), 1);

        CloseThreadpoolWait(wait);
        super::super::CloseHandle(event);
    }

    /// An object that is never signalled still calls back when the due time
    /// passes, and says the wait timed out.
    #[test]
    fn a_wait_that_is_never_signalled_runs_the_callback_on_its_due_time() {
        static CALLS: AtomicU32 = AtomicU32::new(0);
        static SEEN: AtomicU32 = AtomicU32::new(u32::MAX);
        extern "C" fn note(_instance: usize, _context: usize, _wait: usize, result: u32) {
            SEEN.store(result, Ordering::Relaxed);
            CALLS.fetch_add(1, Ordering::Relaxed);
        }

        let callback = native::host_callbacks(&[(note as *const () as usize, false)]).unwrap()[0];
        let event = super::super::CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
        let wait = CreateThreadpoolWait(callback, 0, std::ptr::null());
        SetThreadpoolWait(wait, event, &HUNDRED_MS);
        WaitForThreadpoolWaitCallbacks(wait, 0);
        assert_eq!(
            (CALLS.load(Ordering::Relaxed), SEEN.load(Ordering::Relaxed)),
            (1, WAIT_TIMEOUT)
        );
        CloseThreadpoolWait(wait);
        super::super::CloseHandle(event);
    }

    /// Cancelling and disarming both return without waiting for the object to
    /// change, and neither leaves the callback to run later.
    #[test]
    fn canceling_or_disarming_a_wait_keeps_its_callback_from_running() {
        static CALLS: AtomicU32 = AtomicU32::new(0);
        extern "C" fn count(_instance: usize, _context: usize, _wait: usize, _result: u32) {
            CALLS.fetch_add(1, Ordering::Relaxed);
        }

        let (done, result) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let callback =
                native::host_callbacks(&[(count as *const () as usize, false)]).unwrap()[0];
            let event = super::super::CreateEventW(std::ptr::null(), 0, 0, std::ptr::null());
            let wait = CreateThreadpoolWait(callback, 0, std::ptr::null());
            SetThreadpoolWait(wait, event, &ONE_HOUR);
            WaitForThreadpoolWaitCallbacks(wait, 1);

            // Disarming is the same call with a null handle.
            SetThreadpoolWait(wait, event, &ONE_HOUR);
            SetThreadpoolWait(wait, std::ptr::null_mut(), std::ptr::null());
            WaitForThreadpoolWaitCallbacks(wait, 0);

            CloseThreadpoolWait(wait);
            super::super::CloseHandle(event);
            let _ = done.send(());
        });
        result.recv_timeout(Duration::from_millis(750)).unwrap();
        assert_eq!(CALLS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_wait_calls_are_answered_rather_than_stubbed() {
        for name in [
            "CreateThreadpoolWait",
            "SetThreadpoolWait",
            "WaitForThreadpoolWaitCallbacks",
            "CloseThreadpoolWait",
        ] {
            assert!(
                lookup("api-ms-win-core-threadpool-l1-2-0.dll", name).is_some(),
                "{name} is not answered"
            );
        }
    }
}

#[cfg(test)]
mod crypt_string_tests {
    use super::*;

    /// Calls the way a title calls it: a NUL-terminated UTF-16 string, and a
    /// buffer the caller believes is big enough.
    fn decode(text: &str, flags: u32) -> (i32, Vec<u8>, u32, u32) {
        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
        let mut buffer = [0u8; 128];
        let mut size = buffer.len() as u32;
        let mut skip = u32::MAX;
        let mut used = u32::MAX;
        let ok = CryptStringToBinaryW(
            wide.as_ptr(),
            u32::MAX,
            flags,
            buffer.as_mut_ptr(),
            &mut size,
            &mut skip,
            &mut used,
        );
        (ok, buffer[..size as usize].to_vec(), skip, used)
    }

    /// The vectors from RFC 4648 section 10, which is what "base64" means.
    #[test]
    fn the_rfc_vectors_decode_to_their_bytes() {
        for (text, bytes) in [
            ("", &b""[..]),
            ("Zg==", b"f"),
            ("Zm8=", b"fo"),
            ("Zm9v", b"foo"),
            ("Zm9vYg==", b"foob"),
            ("Zm9vYmE=", b"fooba"),
            ("Zm9vYmFy", b"foobar"),
        ] {
            let (ok, decoded, _, used) = decode(text, CRYPT_STRING_BASE64);
            assert_eq!((ok, decoded.as_slice()), (1, bytes), "{text}");
            assert_eq!(used, CRYPT_STRING_BASE64);
        }
    }

    /// The encoder writes the body across lines, and a proxy URL is not line
    /// wrapped at all: both are the same bytes.
    #[test]
    fn line_ends_and_spaces_are_not_part_of_the_data() {
        let (ok, decoded, _, _) = decode("Zm9v\r\nYmFy\n", CRYPT_STRING_BASE64);
        assert_eq!((ok, decoded.as_slice()), (1, &b"foobar"[..]));
        let (ok, decoded, _, _) = decode(" Zm9v YmFy ", CRYPT_STRING_BASE64);
        assert_eq!((ok, decoded.as_slice()), (1, &b"foobar"[..]));
    }

    /// Armored text carries the header lines and the dashes, and says how far in
    /// the body began.
    #[test]
    fn armored_text_is_read_past_its_header() {
        let armored = "-----BEGIN CERTIFICATE-----\r\nProc-Type: 4,ENCRYPTED\r\n\r\nZm9vYmFy\r\n-----END CERTIFICATE-----\r\n";
        let (ok, decoded, skip, used) = decode(armored, CRYPT_STRING_BASE64_ANY);
        assert_eq!((ok, decoded.as_slice()), (1, &b"foobar"[..]));
        assert_eq!(used, CRYPT_STRING_BASE64HEADER);
        // The body starts after the second header line's CRLF.
        let expected = armored.find("Zm9v").unwrap() as u32;
        assert_eq!(skip, expected);
    }

    /// A buffer too small is told how much it needs, and a null buffer is the
    /// size query rather than a failure.
    #[test]
    fn a_short_buffer_is_told_the_size_it_needs() {
        let wide: Vec<u16> = "Zm9vYmFy".encode_utf16().chain([0]).collect();
        let mut size = 2u32;
        let mut small = [0u8; 2];
        assert_eq!(
            CryptStringToBinaryW(
                wide.as_ptr(),
                u32::MAX,
                CRYPT_STRING_BASE64,
                small.as_mut_ptr(),
                &mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            0
        );
        assert_eq!((size, super::super::GetLastError()), (6, ERROR_MORE_DATA));

        let mut size = 0u32;
        assert_eq!(
            CryptStringToBinaryW(
                wide.as_ptr(),
                u32::MAX,
                CRYPT_STRING_BASE64,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            1
        );
        assert_eq!(size, 6);
    }

    /// What is not base64 is refused as invalid data, not decoded into
    /// something that looks like bytes.
    #[test]
    fn what_is_not_base64_is_refused() {
        for text in ["Zm9v!", "Z", "Zm9vY", "Zg==Zm9v"] {
            let (ok, _, _, _) = decode(text, CRYPT_STRING_BASE64);
            assert_eq!(ok, 0, "{text} was accepted");
            assert_eq!(super::super::GetLastError(), ERROR_INVALID_DATA, "{text}");
        }
        // An encoding this host does not decode says so instead of guessing.
        let (ok, _, _, _) = decode("6d396f", 0x10);
        assert_eq!(ok, 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }

    /// `CRYPT_STRING_BINARY` is the copy: the bytes are already bytes.
    #[test]
    fn binary_is_a_copy() {
        let (ok, decoded, _, used) = decode("Zm9v", CRYPT_STRING_BINARY);
        assert_eq!((ok, decoded.as_slice()), (1, &b"Zm9v"[..]));
        assert_eq!(used, CRYPT_STRING_BINARY);
    }

    #[test]
    fn the_codec_is_answered_rather_than_stubbed() {
        assert!(lookup("crypt32.dll", "CryptStringToBinaryW").is_some());
    }
}

#[cfg(test)]
mod exit_process_tests {
    use super::*;

    #[test]
    fn exit_process_is_answered_rather_than_stubbed() {
        // Not called: it ends the process, which is its whole purpose.
        assert!(
            lookup("kernel32.dll", "ExitProcess").is_some(),
            "ExitProcess is not answered"
        );
        assert!(lookup(
            "api-ms-win-core-processthreads-l1-1-0.dll",
            "ExitProcess"
        )
        .is_some());
    }
}

#[cfg(test)]
mod obsolete_and_system_info_tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    /// `LocalAlloc` is the process heap's block, zeroed when asked, and a
    /// movable block is refused rather than answered with a pointer.
    #[test]
    fn local_alloc_gives_a_block_or_says_why_not() {
        let block = LocalAlloc(0x0040, 16).cast::<u8>();
        assert!(!block.is_null());
        // SAFETY: the 16 bytes this call allocated.
        unsafe {
            for i in 0..16 {
                assert_eq!(*block.add(i), 0, "byte {i} was not zeroed");
            }
            *block = 7;
        }
        assert_eq!(LocalFree(block.cast()), std::ptr::null_mut());

        assert!(LocalAlloc(0, 0).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        assert!(LocalAlloc(0x0002, 16).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn the_obsolete_strings_measure_and_compare() {
        let a = wide("h\u{e9}llo");
        assert_eq!(lstrlenW(a.as_ptr()), 5);
        assert_eq!(lstrlenW(std::ptr::null()), 0);
        let empty = wide("");
        assert_eq!(lstrlenW(empty.as_ptr()), 0);

        let mixed = wide("AbC");
        let folded = wide("aBc");
        assert_eq!(lstrcmpiW(mixed.as_ptr(), folded.as_ptr()), 0);
        let earlier = wide("abc");
        let later = wide("abd");
        assert_eq!(lstrcmpiW(earlier.as_ptr(), later.as_ptr()), -1);
        assert_eq!(lstrcmpiW(later.as_ptr(), earlier.as_ptr()), 1);
        // A shorter string that is a prefix comes first.
        let short = wide("ab");
        assert_eq!(lstrcmpiW(short.as_ptr(), earlier.as_ptr()), -1);
        // The accents fold too, since case folding is by character and not by
        // the host's locale.
        let accented = wide("\u{c9}cole");
        let accented_lower = wide("\u{e9}COLE");
        assert_eq!(lstrcmpiW(accented.as_ptr(), accented_lower.as_ptr()), 0);
    }

    /// The clock is not adjusted, and the language is the one the locale answers
    /// with.
    #[test]
    fn the_system_answers_what_this_host_is() {
        let (mut adjustment, mut increment, mut disabled) = (u32::MAX, u32::MAX, -1);
        assert_eq!(
            GetSystemTimeAdjustment(&mut adjustment, &mut increment, &mut disabled),
            1
        );
        assert_eq!((adjustment, increment, disabled), (0, 156_250, 1));
        assert_eq!(GetUserDefaultUILanguage(), 0x0409);
        // A fence with nothing to order is the call's whole effect.
        FlushProcessWriteBuffers();
    }

    #[test]
    fn the_six_calls_are_answered_rather_than_stubbed() {
        for (dll, name) in [
            ("api-ms-win-core-heap-l2-1-0.dll", "LocalAlloc"),
            ("api-ms-win-core-string-obsolete-l1-1-0.dll", "lstrlenW"),
            ("api-ms-win-core-string-obsolete-l1-1-0.dll", "lstrcmpiW"),
            (
                "api-ms-win-core-localization-obsolete-l1-2-0.dll",
                "GetUserDefaultUILanguage",
            ),
            (
                "api-ms-win-core-sysinfo-l1-1-0.dll",
                "GetSystemTimeAdjustment",
            ),
            (
                "api-ms-win-core-processthreads-l1-1-0.dll",
                "FlushProcessWriteBuffers",
            ),
        ] {
            assert!(lookup(dll, name).is_some(), "{dll}!{name} is not answered");
        }
    }
}

#[cfg(test)]
mod console_tests {
    use super::*;

    /// The console is here: a window handle, and the shape a title sizes its
    /// output to.
    #[test]
    fn the_console_answers_its_shape() {
        assert!(!GetConsoleWindow().is_null());
        // The same window on every call, as on Windows.
        assert_eq!(GetConsoleWindow(), GetConsoleWindow());

        let mut info = [0u8; 22];
        let handle = files::GetStdHandle(0xFFFF_FFF5);
        assert_eq!(GetConsoleScreenBufferInfo(handle, info.as_mut_ptr()), 1);
        let field = |at: usize| i16::from_le_bytes([info[at], info[at + 1]]);
        assert_eq!((field(0), field(2)), (80, 25));
        assert_eq!((field(4), field(6)), (0, 0));
        assert_eq!(field(8), 7);
        assert_eq!((field(10), field(12), field(14), field(16)), (0, 0, 79, 24));
        assert_eq!((field(18), field(20)), (80, 25));

        assert_eq!(
            GetConsoleScreenBufferInfo(std::ptr::null_mut(), info.as_mut_ptr()),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }

    /// A console write reaches a stream instead of going nowhere, and the count
    /// it reports is characters, not bytes.
    #[test]
    fn text_written_to_the_console_reaches_a_stream() {
        let out = files::GetStdHandle(0xFFFF_FFF5);
        // One space: the write is real, and this does not fill the log the test
        // runs in (the call writes to the descriptor, past the harness's capture).
        let text: Vec<u16> = " ".encode_utf16().collect();
        let mut written = u32::MAX;
        assert_eq!(
            WriteConsoleW(out, text.as_ptr(), text.len() as u32, &mut written, std::ptr::null()),
            1
        );
        assert_eq!(written, 1, "the count is characters, not the bytes written");

        let mut written = u32::MAX;
        assert_eq!(
            WriteConsoleW(out, std::ptr::null(), 0, &mut written, std::ptr::null()),
            1
        );
        assert_eq!(written, 0);

        assert_eq!(
            WriteConsoleW(std::ptr::null_mut(), text.as_ptr(), 1, &mut written, std::ptr::null()),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn the_console_calls_are_answered_rather_than_stubbed() {
        for (dll, name) in [
            ("api-ms-win-core-console-l3-2-0.dll", "GetConsoleWindow"),
            (
                "api-ms-win-core-console-l2-1-0.dll",
                "GetConsoleScreenBufferInfo",
            ),
            ("api-ms-win-core-console-l1-1-0.dll", "WriteConsoleW"),
        ] {
            assert!(lookup(dll, name).is_some(), "{dll}!{name} is not answered");
        }
    }
}

#[cfg(test)]
mod work_queue_and_attribute_tests {
    use super::*;

    /// The queued callback runs, on a thread that is not this one, and receives
    /// its context.
    #[test]
    fn a_queued_work_item_runs_with_its_context() {
        static RAN: AtomicU32 = AtomicU32::new(0);
        static GOT: AtomicU64 = AtomicU64::new(0);
        extern "C" fn work(context: usize, _timer: u32) {
            GOT.store(context as u64, Ordering::SeqCst);
            RAN.fetch_add(1, Ordering::SeqCst);
        }

        let callback = native::host_callbacks(&[(work as *const () as usize, false)]).unwrap()[0];
        assert_eq!(
            QueueUserWorkItem(callback, 0x4321 as *mut c_void, 0),
            1
        );
        let waiting = Instant::now();
        while RAN.load(Ordering::SeqCst) == 0 && waiting.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(RAN.load(Ordering::SeqCst), 1);
        assert_eq!(GOT.load(Ordering::SeqCst), 0x4321);

        assert_eq!(QueueUserWorkItem(0, std::ptr::null_mut(), 0), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }

    /// The attribute list is for a process this host does not start: it answers
    /// how big a list would be and refuses the rest with a reason.
    #[test]
    fn the_process_attribute_list_is_refused_with_a_reason() {
        let mut size = 0usize;
        assert_eq!(
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut size),
            0
        );
        assert!(size > 0, "the size query wrote nothing");
        assert_eq!(super::super::GetLastError(), ERROR_INSUFFICIENT_BUFFER);

        let mut list = [0u8; 64];
        assert_eq!(
            InitializeProcThreadAttributeList(list.as_mut_ptr().cast(), 2, 0, &mut size),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_NOT_SUPPORTED);
        assert_eq!(
            UpdateProcThreadAttribute(
                list.as_mut_ptr().cast(),
                0,
                0x20009, // PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY
                std::ptr::null(),
                8,
                std::ptr::null_mut(),
                std::ptr::null_mut()
            ),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_NOT_SUPPORTED);
        DeleteProcThreadAttributeList(list.as_mut_ptr().cast());
    }

    #[test]
    fn the_five_calls_are_answered_rather_than_stubbed() {
        for (dll, name) in [
            (
                "api-ms-win-core-threadpool-legacy-l1-1-0.dll",
                "QueueUserWorkItem",
            ),
            (
                "api-ms-win-core-processthreads-l1-1-0.dll",
                "CreateRemoteThreadEx",
            ),
            (
                "api-ms-win-core-processthreads-l1-1-0.dll",
                "InitializeProcThreadAttributeList",
            ),
            (
                "api-ms-win-core-processthreads-l1-1-0.dll",
                "UpdateProcThreadAttribute",
            ),
            (
                "api-ms-win-core-processthreads-l1-1-0.dll",
                "DeleteProcThreadAttributeList",
            ),
        ] {
            // The table the loader consults, which knows the names this module
            // does not answer itself (`CreateRemoteThreadEx` is the handle
            // table's there, next to `DuplicateHandle`).
            assert!(
                super::super::lookup(dll, name).is_some(),
                "{dll}!{name} is not answered"
            );
        }
    }
}

#[cfg(test)]
mod create_remote_thread_tests {
    use super::*;

    /// A thread of another process is not this process's to start.
    #[test]
    fn a_thread_in_another_process_is_refused() {
        let mut id = 0u32;
        assert!(
            super::super::CreateRemoteThreadEx(
                std::ptr::null_mut(),
                std::ptr::null(),
                0,
                0,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
                &mut id
            )
            .is_null()
        );
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_HANDLE);
    }
}

#[cfg(test)]
mod logical_processor_tests {
    use super::*;

    /// Walks the entries a call filled: each carries its own relationship and
    /// size, and the sizes step to the next entry.
    fn entries(bytes: &[u8]) -> Vec<(u32, u32, &[u8])> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at < bytes.len() {
            let relationship = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
            let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            assert!(size >= 8 && at + size <= bytes.len(), "entry size {size} runs out");
            out.push((relationship, size as u32, &bytes[at + 8..at + size]));
            at += size;
        }
        assert_eq!(at, bytes.len(), "the entries do not fill the buffer exactly");
        out
    }

    fn fill(relationship: u32) -> Vec<u8> {
        let mut needed = 0u32;
        assert_eq!(
            GetLogicalProcessorInformationEx(relationship, std::ptr::null_mut(), &mut needed),
            0,
            "the size query answered as if it had filled a buffer"
        );
        assert_eq!(super::super::GetLastError(), ERROR_INSUFFICIENT_BUFFER);
        assert!(needed > 0);
        let mut buffer = vec![0u8; needed as usize];
        assert_eq!(
            GetLogicalProcessorInformationEx(relationship, buffer.as_mut_ptr(), &mut needed),
            1
        );
        buffer.truncate(needed as usize);
        buffer
    }

    /// One entry per logical processor, each with its own bit and its place in
    /// the single group, and a package that covers all of them.
    #[test]
    fn every_logical_processor_gets_a_core_entry() {
        let n = cpus().min(64) as usize;
        let core = fill(RELATION_PROCESSOR_CORE);
        let cores = entries(&core);
        assert_eq!(cores.len(), n);
        for (i, (relationship, size, payload)) in cores.iter().enumerate() {
            assert_eq!(*relationship, RELATION_PROCESSOR_CORE);
            assert_eq!(*size, 48, "PROCESSOR_RELATIONSHIP with one group mask");
            assert_eq!(payload[0], 0, "a whole core, not two of them");
            let groups = u16::from_le_bytes([payload[22], payload[23]]);
            assert_eq!(groups, 1);
            let mask = u64::from_le_bytes(payload[24..32].try_into().unwrap());
            assert_eq!(mask, 1 << i, "core {i} has the wrong processor");
            // Group 0: the low word after the mask, and the reserved words zero.
            assert_eq!(&payload[32..40], &[0u8; 8]);
        }

        // The package covers every processor, and the group says the same.
        let package_bytes = fill(RELATION_PROCESSOR_PACKAGE);
        let package = entries(&package_bytes);
        assert_eq!(package.len(), 1);
        let every = if n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
        assert_eq!(
            u64::from_le_bytes(package[0].2[24..32].try_into().unwrap()),
            every
        );

        let group_bytes = fill(RELATION_GROUP);
        let group = entries(&group_bytes);
        assert_eq!(group.len(), 1);
        let (_, size, payload) = group[0];
        assert_eq!(size, 80, "GROUP_RELATIONSHIP with one group");
        assert_eq!(u16::from_le_bytes([payload[0], payload[1]]), 1);
        assert_eq!(u16::from_le_bytes([payload[2], payload[3]]), 1);
        assert_eq!((payload[24], payload[25]), (n as u8, n as u8));
        assert_eq!(
            u64::from_le_bytes(payload[64..72].try_into().unwrap()),
            every
        );

        // Everything at once is the three answers in one buffer.
        let all_bytes = fill(RELATION_ALL);
        let all = entries(&all_bytes);
        assert_eq!(all.len(), n + 2);
        assert_eq!(
            all.iter().filter(|e| e.0 == RELATION_PROCESSOR_CORE).count(),
            n
        );
    }

    /// A buffer that is too small is told the size, and a relationship this host
    /// does not describe is refused rather than answered with something else.
    #[test]
    fn a_short_buffer_and_an_unanswered_relationship_are_told_apart() {
        let mut needed = 0u32;
        assert_eq!(
            GetLogicalProcessorInformationEx(RELATION_PROCESSOR_CORE, std::ptr::null_mut(), &mut needed),
            0
        );
        let full = needed;
        let mut small = vec![0u8; 8];
        let mut room = small.len() as u32;
        assert_eq!(
            GetLogicalProcessorInformationEx(
                RELATION_PROCESSOR_CORE,
                small.as_mut_ptr(),
                &mut room
            ),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_INSUFFICIENT_BUFFER);
        assert_eq!(room, full, "the size a buffer needs");

        for unanswered in [RELATION_CACHE, 1 /* a NUMA node */, 0x1234] {
            let mut room = 0u32;
            let mut buffer = vec![0u8; 4096];
            assert_eq!(
                GetLogicalProcessorInformationEx(unanswered, buffer.as_mut_ptr(), &mut room),
                0,
                "{unanswered:#x} was answered"
            );
            assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        }
    }

    #[test]
    fn the_extended_call_is_answered_rather_than_stubbed() {
        assert!(
            lookup(
                "api-ms-win-core-sysinfo-l1-1-0.dll",
                "GetLogicalProcessorInformationEx"
            )
            .is_some()
        );
    }
}

#[cfg(test)]
mod string_type_tests {
    use super::*;

    /// The categories of `winnls.h`, which is what `ispunct`, `isdigit` and
    /// their neighbours ask about through this call.
    const UPPER: u16 = 0x0001;
    const LOWER: u16 = 0x0002;
    const DIGIT: u16 = 0x0004;
    const SPACE: u16 = 0x0008;
    const PUNCT: u16 = 0x0010;
    const CNTRL: u16 = 0x0020;
    const BLANK: u16 = 0x0040;
    const XDIGIT: u16 = 0x0080;
    const ALPHA: u16 = 0x0100;
    const DEFINED: u16 = 0x0200;

    fn kinds_of(text: &str) -> Vec<u16> {
        let wide: Vec<u16> = text.encode_utf16().chain([0]).collect();
        let mut out = vec![0u16; text.encode_utf16().count()];
        assert_eq!(
            GetStringTypeW(1, wide.as_ptr(), -1, out.as_mut_ptr()),
            1,
            "{text}"
        );
        out
    }

    /// A letter in each case, a digit, a blank, punctuation, a control and the
    /// characters that are all of those at once (a hex digit is a digit).
    #[test]
    fn the_categories_are_what_the_characters_are() {
        // `A` is a hex digit as well as a capital letter.
        assert_eq!(kinds_of("A")[0], UPPER | ALPHA | XDIGIT | DEFINED);
        // And `a` is a hex digit too, which the lower-case letters `b`..`z` after
        // `f` are not.
        assert_eq!(kinds_of("a")[0], LOWER | ALPHA | XDIGIT | DEFINED);
        assert_eq!(kinds_of("7")[0], DIGIT | XDIGIT | DEFINED);
        assert_eq!(kinds_of("f")[0], LOWER | ALPHA | XDIGIT | DEFINED);
        assert_eq!(kinds_of(" ") [0], SPACE | BLANK | DEFINED);
        assert_eq!(kinds_of("\t")[0], SPACE | BLANK | CNTRL | DEFINED);
        assert_eq!(kinds_of("\n")[0], SPACE | CNTRL | DEFINED);
        assert_eq!(kinds_of("'")[0], PUNCT | DEFINED);
        assert_eq!(kinds_of(".")[0], PUNCT | DEFINED);
        // A non-ASCII punctuation is still punctuation, which an ASCII-only test
        // would miss.
        assert_eq!(kinds_of("\u{ab}")[0], PUNCT | DEFINED);
        // An accented letter is a letter, and a lower-case one.
        assert_eq!(kinds_of("\u{e9}")[0], LOWER | ALPHA | DEFINED);
        assert_eq!(kinds_of("\u{c9}")[0], UPPER | ALPHA | DEFINED);
        // `g` is a letter and not a hex digit, which is where the hex letters
        // stop.
        assert_eq!(kinds_of("g")[0], LOWER | ALPHA | DEFINED);
        // A digit outside ASCII is a digit, not punctuation.
        assert_eq!(kinds_of("\u{660}")[0], DIGIT | DEFINED);
        // A non-breaking space is a space, not a blank.
        assert_eq!(kinds_of("\u{a0}")[0], SPACE | DEFINED);
    }

    /// A count is honoured, and a negative one means the string's own end.
    #[test]
    fn the_count_decides_how_much_is_classified() {
        let wide: Vec<u16> = "a1bc".encode_utf16().collect();
        let mut out = [0u16; 2];
        assert_eq!(GetStringTypeExW(0x400, 1, wide.as_ptr(), 2, out.as_mut_ptr()), 1);
        assert_eq!(out, [LOWER | ALPHA | XDIGIT | DEFINED, DIGIT | XDIGIT | DEFINED]);

        // A NUL-terminated call stops at the terminator, and the call takes the
        // locale as its first word.
        let mut all = [0u16; 4];
        assert_eq!(
            GetStringTypeExW(0x400, 1, wide.as_ptr(), -1, all.as_mut_ptr()),
            1
        );
        assert_eq!(all[2], LOWER | ALPHA | XDIGIT | DEFINED, "`b` is a hex digit");

        // The tables this host does not answer are refused, and a null pointer
        // with them.
        for kind in [2, 3, 0] {
            assert_eq!(GetStringTypeW(kind, wide.as_ptr(), 1, all.as_mut_ptr()), 0);
            assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        }
        assert_eq!(
            GetStringTypeW(1, std::ptr::null(), 1, all.as_mut_ptr()),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn the_two_names_are_answered_rather_than_stubbed() {
        assert!(lookup("api-ms-win-core-string-l1-1-0.dll", "GetStringTypeExW").is_some());
        assert!(lookup("api-ms-win-core-string-l1-1-0.dll", "GetStringTypeW").is_some());
    }
}

#[cfg(test)]
mod resource_tests {
    use super::*;

    fn put(bytes: &mut [u8], at: usize, value: u32) {
        bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put16(bytes: &mut [u8], at: usize, value: u16) {
        bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }

    /// An entry of a directory: the key (an id, or an offset with the top bit
    /// set) and where it points (another directory's offset with the top bit
    /// set, or a leaf's offset).
    fn entry(bytes: &mut [u8], at: usize, key: u32, offset: u32) {
        put(bytes, at, key);
        put(bytes, at + 4, offset);
    }

    /// An image with the headers and the resource tree a real one has:
    ///
    /// - type 10 (by id) and type `ABC` (by name) both holding
    /// - name 101 in language 0x409, whose data is
    /// - the five bytes `hello`, at 0x300.
    fn image() -> Vec<u8> {
        let mut bytes = vec![0u8; 0x400];
        // The headers: `e_lfanew`, the signature, and a PE32+ optional header
        // whose data directory entry 2 names the resource directory at 0x200.
        put(&mut bytes, 0x3C, 0x40);
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        put16(&mut bytes, 0x58, 0x20B);
        let directory = 0x40 + 24 + 2 * 8;
        put(&mut bytes, directory + 112, 0x200);
        put(&mut bytes, directory + 116, 0x200);

        // The root: one id and one named type, both pointing at 0x230.
        put16(&mut bytes, 0x200 + 12, 1); // NumberOfNamedEntries
        put16(&mut bytes, 0x200 + 14, 1); // NumberOfIdEntries
        entry(&mut bytes, 0x210, 10, 0x8000_0030);
        entry(&mut bytes, 0x218, 0x8000_0090, 0x8000_0030);

        // Type 10's names: id 101, whose languages are at 0x250.
        put16(&mut bytes, 0x230 + 14, 1);
        entry(&mut bytes, 0x240, 101, 0x8000_0050);

        // Name 101's languages: 0x409, whose data entry is at 0x270.
        put16(&mut bytes, 0x250 + 14, 1);
        entry(&mut bytes, 0x260, 0x409, 0x70);

        // The data entry: where the bytes are, and how many.
        put(&mut bytes, 0x270, 0x300);
        put(&mut bytes, 0x274, 5);

        // The name `ABC`: a length and then its characters.
        put16(&mut bytes, 0x290, 3);
        for (i, c) in "ABC".encode_utf16().enumerate() {
            put16(&mut bytes, 0x292 + i * 2, c);
        }
        bytes[0x300..0x305].copy_from_slice(b"hello");
        bytes
    }

    /// A resource is found through its type, its name and its language, and what
    /// comes back is the image's own data, not a copy.
    #[test]
    fn a_resource_is_found_by_its_three_levels_and_loaded() {
        let bytes = image();
        let base = bytes.as_ptr() as usize;
        let module = base as *mut c_void;
        let found = FindResourceExW(module, 10, 101, 0x409);
        assert!(!found.is_null(), "type 10 name 101 was not found");
        // The handle is the image's data entry, as Windows hands out for a mapped
        // module.
        assert_eq!(found as usize, base + 0x270);

        let data = LoadResource(module, found);
        assert_eq!(data as usize, base + 0x300);
        // SAFETY: the five bytes this test wrote into its own image.
        assert_eq!(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), 5) }, b"hello");

        // Language zero means any, and the same entry comes back.
        assert_eq!(FindResourceExW(module, 10, 101, 0), found);
        // A type given by name, without case.
        let name: Vec<u16> = "abc\0".encode_utf16().collect();
        assert_eq!(
            FindResourceExW(module, name.as_ptr() as usize, 101, 0x409),
            found
        );
    }

    /// Three levels, three reasons: a type, a name and a language that are not
    /// there are told apart, and a handle that is not one is refused.
    #[test]
    fn what_is_missing_is_named() {
        let bytes = image();
        let module = bytes.as_ptr() as *mut c_void;
        assert!(FindResourceExW(module, 11, 101, 0x409).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_RESOURCE_TYPE_NOT_FOUND);
        assert!(FindResourceExW(module, 10, 102, 0x409).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_RESOURCE_NAME_NOT_FOUND);
        assert!(FindResourceExW(module, 10, 101, 0x40C).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_RESOURCE_LANG_NOT_FOUND);

        assert!(LoadResource(module, std::ptr::null_mut()).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }

    #[test]
    fn the_two_names_are_answered_rather_than_stubbed() {
        for name in ["FindResourceExW", "LoadResource"] {
            assert!(
                lookup("api-ms-win-core-libraryloader-l1-2-0.dll", name).is_some(),
                "{name} is not answered"
            );
        }
    }
}

#[cfg(test)]
mod init_once_tests {
    use super::*;

    #[test]
    fn a_begin_claims_the_word_and_complete_hands_the_context_back() {
        let once = AtomicU64::new(0);
        let word = (&once) as *const AtomicU64 as *mut AtomicU64;
        let data = 0u64;
        let context_value = (&data) as *const u64 as *mut c_void;
        let (mut pending, mut context) = (-1i32, std::ptr::null_mut());

        // Check-only claims nothing: an untouched word is pending and has no context yet.
        assert_eq!(
            InitOnceBeginInitialize(word, INIT_ONCE_CHECK_ONLY, &mut pending, &mut context),
            0
        );
        assert_eq!((pending, context, once.load(Ordering::Relaxed)), (1, std::ptr::null_mut(), 0));
        assert_eq!(super::super::GetLastError(), ERROR_IO_PENDING);

        // A plain begin claims it, and says the caller must run the initialization.
        assert_eq!(InitOnceBeginInitialize(word, 0, &mut pending, &mut context), 1);
        assert_eq!((pending, once.load(Ordering::Relaxed)), (1, 1));

        // A second begin waits for the claim, so it is only answered by the complete.
        assert_eq!(InitOnceComplete(word, 0, context_value), 1);
        assert_eq!(once.load(Ordering::Relaxed), context_value as u64 | 2);
        assert_eq!(InitOnceComplete(word, 0, context_value), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);

        for flags in [0, INIT_ONCE_CHECK_ONLY] {
            context = std::ptr::null_mut();
            assert_eq!(InitOnceBeginInitialize(word, flags, &mut pending, &mut context), 1);
            assert_eq!((pending, context), (0, context_value));
        }
    }

    #[test]
    fn a_failed_run_hands_the_word_back_and_async_says_it_is_not_implemented() {
        let once = AtomicU64::new(0);
        let word = (&once) as *const AtomicU64 as *mut AtomicU64;
        let (mut pending, mut context) = (-1i32, std::ptr::null_mut());

        assert_eq!(InitOnceBeginInitialize(word, 0, &mut pending, &mut context), 1);
        assert_eq!(InitOnceComplete(word, INIT_ONCE_INIT_FAILED, std::ptr::null_mut()), 1);
        assert_eq!(once.load(Ordering::Relaxed), 0);
        assert_eq!(InitOnceBeginInitialize(word, 0, &mut pending, &mut context), 1);
        assert_eq!(pending, 1);

        assert_eq!(
            InitOnceBeginInitialize(word, INIT_ONCE_ASYNC, &mut pending, &mut context),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_CALL_NOT_IMPLEMENTED);
        assert_eq!(InitOnceComplete(word, INIT_ONCE_ASYNC, std::ptr::null_mut()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_CALL_NOT_IMPLEMENTED);

        // A complete with nothing claimed is the caller's own mistake.
        let fresh = AtomicU64::new(2);
        let done = (&fresh) as *const AtomicU64 as *mut AtomicU64;
        assert_eq!(InitOnceComplete(done, 0, std::ptr::null_mut()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(fresh.load(Ordering::Relaxed), 2);
    }
}

#[cfg(test)]
mod apartment_and_winrt_tests {
    use super::*;

    const S_FALSE: i32 = 1;
    const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
    const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;
    const E_POINTER: i32 = 0x8000_4003u32 as i32;
    const RPC_E_CHANGED_MODE: i32 = 0x8001_0106u32 as i32;
    const REGDB_E_CLASSNOTREG: i32 = 0x8004_0154u32 as i32;

    /// This host has one apartment, and the calls that describe it say the same
    /// thing rather than leaving it unanswered.
    #[test]
    fn the_apartment_is_the_multithreaded_one() {
        let (mut kind, mut qualifier) = (u32::MAX, u32::MAX);
        assert_eq!(CoGetApartmentType(&mut kind, &mut qualifier), 0);
        assert_eq!((kind, qualifier), (1, 1));

        // Initialising again with the same model is S_FALSE, with another model
        // is a mode change, as COM answers it. Each thread has its own answer, so
        // this runs on one.
        std::thread::spawn(|| {
            assert_eq!(RoInitialize(1), 0);
            assert_eq!(RoInitialize(1), S_FALSE);
            assert_eq!(RoInitialize(0), RPC_E_CHANGED_MODE);
            let mut apartment = 0u64;
            assert_eq!(RoGetApartmentIdentifier(&mut apartment), 0);
            assert_eq!(apartment, 1);
            let mut registration = 0u64;
            assert_eq!(
                RoRegisterForApartmentShutdown(std::ptr::null(), std::ptr::null_mut(), &mut registration),
                0
            );
            assert_ne!(registration, 0);
            RoUnregisterForApartmentShutdown(registration);
            RoUninitialize();
            // The next initialisation is the first one again.
            assert_eq!(RoInitialize(1), 0);
            RoUninitialize();
        })
        .join()
        .unwrap();

        // A count of callers, which never goes below zero.
        assert_eq!(CoAddRefServerProcess(), 1);
        assert_eq!(CoAddRefServerProcess(), 2);
        assert_eq!(CoReleaseServerProcess(), 1);
        assert_eq!(CoReleaseServerProcess(), 0);
        assert_eq!(CoReleaseServerProcess(), 0);
        CoAddRefServerProcess();
    }

    /// What needs an object behind it is refused with the reason, so a caller's
    /// failure is visible rather than silent.
    #[test]
    fn what_needs_an_object_is_refused_with_its_reason() {
        let mut context: *mut c_void = 1 as *mut c_void;
        assert_eq!(CoGetObjectContext(std::ptr::null(), &mut context), E_NOINTERFACE);
        assert!(context.is_null());
        let mut token = usize::MAX;
        assert_eq!(CoGetContextToken(&mut token), E_NOINTERFACE);
        assert_eq!(token, 0);

        let mut marshaler: *mut c_void = 1 as *mut c_void;
        assert_eq!(
            CoCreateFreeThreadedMarshaler(std::ptr::null_mut(), &mut marshaler),
            E_NOTIMPL
        );
        assert!(marshaler.is_null());
        let mut stream: *mut c_void = 1 as *mut c_void;
        assert_eq!(
            CoMarshalInterThreadInterfaceInStream(std::ptr::null(), std::ptr::null_mut(), &mut stream),
            E_NOTIMPL
        );
        assert!(stream.is_null());
        assert_eq!(
            CoGetInterfaceAndReleaseStream(std::ptr::null_mut(), std::ptr::null(), &mut stream),
            E_NOTIMPL
        );

        // No error-info object is handed out, and none is claimed when there is
        // nothing to report.
        let mut info: *mut c_void = 1 as *mut c_void;
        assert_eq!(GetRestrictedErrorInfo(&mut info), S_FALSE);
        assert!(info.is_null());
        assert_eq!(SetRestrictedErrorInfo(std::ptr::null_mut()), E_NOTIMPL);
        assert_eq!(GetRestrictedErrorInfo(std::ptr::null_mut()), E_POINTER);
    }

    /// A task block is the process heap's, and the constants of the calls are the
    /// ones Windows uses.
    #[test]
    fn task_memory_comes_from_the_heap() {
        let block = CoTaskMemAlloc(16).cast::<u8>();
        assert!(!block.is_null());
        // SAFETY: the 16 bytes this call allocated.
        unsafe {
            std::ptr::write_bytes(block, 0xAB, 16);
            assert_eq!(*block, 0xAB);
        }
        // Freeing it is the documented shortcut, so this only checks the call is
        // answered.
        CoTaskMemFree(block.cast());
        assert!(CoTaskMemAlloc(0).is_null());
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
    }

    /// A runtime class this host does not implement is reported as not
    /// registered, and its name comes out of the `HSTRING` the caller passed.
    #[test]
    fn a_runtime_class_is_reported_as_not_registered() {
        let wide: Vec<u16> = "Windows.Foundation.Uri".encode_utf16().collect();
        let mut class = std::ptr::null_mut();
        assert_eq!(
            super::super::winrt::WindowsCreateString(wide.as_ptr(), wide.len() as u32, &mut class),
            0
        );
        let mut factory: *mut c_void = 1 as *mut c_void;
        assert_eq!(
            RoGetActivationFactory(class as usize, std::ptr::null(), &mut factory),
            REGDB_E_CLASSNOTREG
        );
        assert!(factory.is_null());

        // Registering factories is answered with a cookie, and the cookie is
        // revocable.
        let mut cookie = 0u32;
        assert_eq!(
            RoRegisterActivationFactories(std::ptr::null(), std::ptr::null(), 0, &mut cookie),
            0
        );
        assert_ne!(cookie, 0);
        RoRevokeActivationFactories(cookie);

        // An error's message is read out of its `HSTRING` and kept, which is what
        // makes a failure inside the title readable.
        let message: Vec<u16> = "no".encode_utf16().collect();
        let mut text = std::ptr::null_mut();
        assert_eq!(
            super::super::winrt::WindowsCreateString(message.as_ptr(), message.len() as u32, &mut text),
            0
        );
        assert_eq!(RoOriginateError(0x8000_0001u32 as i32, text as usize), 1);
        assert_eq!(RoTransformError(0x8000_0001u32 as i32, 0x8000_0005u32 as i32, text as usize), 1);
        assert_eq!(super::super::winrt::WindowsDeleteString(text), 0);
        assert_eq!(super::super::winrt::WindowsDeleteString(class), 0);
    }

    #[test]
    fn the_com_and_winrt_names_are_answered_rather_than_stubbed() {
        for (dll, name) in [
            ("api-ms-win-core-com-l1-1-0.dll", "CoTaskMemAlloc"),
            ("api-ms-win-core-com-l1-1-0.dll", "CoGetApartmentType"),
            ("api-ms-win-core-com-l1-1-0.dll", "CoAddRefServerProcess"),
            ("api-ms-win-core-com-l1-1-0.dll", "CoReleaseServerProcess"),
            ("api-ms-win-core-com-l1-1-0.dll", "CoGetObjectContext"),
            ("api-ms-win-core-com-l1-1-0.dll", "CoGetContextToken"),
            ("api-ms-win-core-com-l1-1-0.dll", "CoCreateFreeThreadedMarshaler"),
            ("api-ms-win-core-com-l1-1-0.dll", "CoMarshalInterThreadInterfaceInStream"),
            ("api-ms-win-core-com-l1-1-0.dll", "CoGetInterfaceAndReleaseStream"),
            ("api-ms-win-core-winrt-l1-1-0.dll", "RoInitialize"),
            ("api-ms-win-core-winrt-l1-1-0.dll", "RoUninitialize"),
            ("api-ms-win-core-winrt-l1-1-0.dll", "RoGetApartmentIdentifier"),
            ("api-ms-win-core-winrt-l1-1-0.dll", "RoRegisterForApartmentShutdown"),
            ("api-ms-win-core-winrt-l1-1-0.dll", "RoUnregisterForApartmentShutdown"),
            ("api-ms-win-core-winrt-l1-1-0.dll", "RoGetActivationFactory"),
            ("api-ms-win-core-winrt-l1-1-0.dll", "RoRegisterActivationFactories"),
            ("api-ms-win-core-winrt-l1-1-0.dll", "RoRevokeActivationFactories"),
            ("api-ms-win-core-winrt-error-l1-1-0.dll", "RoOriginateError"),
            ("api-ms-win-core-winrt-error-l1-1-0.dll", "RoTransformError"),
            ("api-ms-win-core-winrt-error-l1-1-0.dll", "GetRestrictedErrorInfo"),
            ("api-ms-win-core-winrt-error-l1-1-0.dll", "SetRestrictedErrorInfo"),
        ] {
            assert!(lookup(dll, name).is_some(), "{dll}!{name} is not answered");
        }
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;

    /// The error mode a title sets at startup is the one it and everything else in
    /// the process reads back.
    #[test]
    fn the_error_mode_round_trips_and_reports_what_it_replaced() {
        let original = GetErrorMode();
        assert_eq!(SetErrorMode(0x1 | 0x2), original);
        assert_eq!(GetErrorMode(), 0x1 | 0x2);
        let mut replaced = 0u32;
        assert_eq!(SetThreadErrorMode(0x8000, &mut replaced), 1);
        assert_eq!(replaced, 0x1 | 0x2);
        assert_eq!(GetErrorMode(), 0x8000);
        // A caller that wants no old value passes no pointer.
        assert_eq!(SetThreadErrorMode(original, std::ptr::null_mut()), 1);
        assert_eq!(GetErrorMode(), original);
    }

    /// The priority class is the one the title set, an unknown class is refused, and
    /// a handle that is not this process is not answered.
    #[test]
    fn the_priority_class_is_remembered_and_validated() {
        let process = GetCurrentProcess() as usize as *mut c_void;
        let original = GetPriorityClass(process);
        assert!(PRIORITY_CLASSES.contains(&original));
        assert_eq!(SetPriorityClass(process, ABOVE_NORMAL_PRIORITY_CLASS), 1);
        assert_eq!(GetPriorityClass(process), ABOVE_NORMAL_PRIORITY_CLASS);
        set_error(0);
        assert_eq!(SetPriorityClass(process, 0x1234), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);
        assert_eq!(GetPriorityClass(process), ABOVE_NORMAL_PRIORITY_CLASS);
        assert_eq!(SetPriorityClass(process, original), 1);
        assert_eq!(GetPriorityClass(0xdead as *mut c_void), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_HANDLE);
    }

    /// The title is the machine's own code, not a 32-bit process under emulation,
    /// and its image path comes back with its length in the caller's buffer.
    #[test]
    fn the_process_is_amd64_and_knows_where_its_image_is() {
        let process = GetCurrentProcess() as usize as *mut c_void;
        let (mut machine, mut native) = (0xFFFFu16, 0u16);
        assert_eq!(IsWow64Process2(process, &mut machine, &mut native), 1);
        assert_eq!((machine, native), (0, 0x8664));
        assert_eq!(
            IsWow64Process2(process, std::ptr::null_mut(), std::ptr::null_mut()),
            1,
            "both outputs are optional"
        );
        assert_eq!(
            IsWow64Process2(0xdead as *mut c_void, &mut machine, &mut native),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_HANDLE);

        let mut buffer = [0u16; 4096];
        let mut size = buffer.len() as u32;
        assert_eq!(
            QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut size),
            1
        );
        assert!(size > 0 && (size as usize) < buffer.len());
        let text = String::from_utf16_lossy(&buffer[..size as usize]);
        assert!(!text.is_empty(), "no image path");
        assert_eq!(buffer[size as usize], 0, "the path is NUL-terminated");
        assert_eq!(text.len(), size as usize, "the length excludes the NUL");

        let mut small = [0u16; 2];
        let mut too_small = 2u32;
        set_error(0);
        assert_eq!(
            QueryFullProcessImageNameW(process, 0, small.as_mut_ptr(), &mut too_small),
            0
        );
        assert_eq!(super::super::GetLastError(), ERROR_INSUFFICIENT_BUFFER);

        for dll in ["kernel32.dll", "api-ms-win-core-processthreads-l1-1-0.dll"] {
            for name in [
                "GetErrorMode",
                "SetErrorMode",
                "SetThreadErrorMode",
                "SetPriorityClass",
                "GetPriorityClass",
                "IsWow64Process2",
                "QueryFullProcessImageNameW",
            ] {
                assert!(super::super::lookup(dll, name).is_some(), "{dll}!{name}");
            }
        }
    }
}

#[cfg(test)]
mod processor_tests {
    use super::*;

    /// A title that sizes a worker pool by the processor count gets the host's
    /// processors, in groups of 64 as Windows lays them out, and not the 0 a stub
    /// would answer.
    #[test]
    fn the_processor_count_is_the_hosts_and_the_groups_add_up() {
        let n = cpus();
        assert_eq!(GetActiveProcessorCount(ALL_PROCESSOR_GROUPS), n);
        assert_eq!(GetActiveProcessorCount(0), n.min(GROUP_SIZE));
        let groups = n.div_ceil(GROUP_SIZE);
        let mut total = 0;
        for group in 0..groups {
            let in_group = GetActiveProcessorCount(group as u16);
            assert!(in_group > 0 && in_group <= GROUP_SIZE, "group {group}");
            total += in_group;
        }
        assert_eq!(total, n, "the groups do not add up to the count");
        assert_eq!(GetActiveProcessorCount(groups as u16), 0, "a group that is not there");

        let number = GetCurrentProcessorNumber();
        assert!(number < n, "{number} is not one of {n} processors");
    }

    /// The boost setting is remembered rather than applied, and the stack
    /// guarantee is accepted as the title wrote it.
    #[test]
    fn the_priority_boost_round_trips_and_the_stack_guarantee_is_left_alone() {
        assert_eq!(SetThreadPriorityBoost(std::ptr::null_mut(), 1), 1);
        let mut disable = -1;
        assert_eq!(GetThreadPriorityBoost(std::ptr::null_mut(), &mut disable), 1);
        assert_eq!(disable, 1);
        assert_eq!(SetThreadPriorityBoost(std::ptr::null_mut(), 0), 1);
        assert_eq!(GetThreadPriorityBoost(std::ptr::null_mut(), &mut disable), 1);
        assert_eq!(disable, 0);

        let mut guarantee = 0x1_0000u32;
        assert_eq!(SetThreadStackGuarantee(&mut guarantee), 1);
        assert_eq!(guarantee, 0x1_0000);
        assert_eq!(SetThreadStackGuarantee(std::ptr::null_mut()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_PARAMETER);

        for dll in ["kernel32.dll", "api-ms-win-core-processthreads-l1-1-0.dll"] {
            for name in [
                "GetActiveProcessorCount",
                "GetCurrentProcessorNumber",
                "GetThreadPriorityBoost",
                "SetThreadPriorityBoost",
                "SetThreadStackGuarantee",
            ] {
                assert!(super::super::lookup(dll, name).is_some(), "{dll}!{name}");
            }
        }
    }
}

#[cfg(test)]
mod thread_and_processor_tests {
    use super::*;

    /// The largest count and the group count are this machine's numbers, not the
    /// stub's zero.
    #[test]
    fn the_counts_agree_with_the_machine() {
        let n = cpus();
        assert_eq!(GetMaximumProcessorCount(0), n);
        assert_eq!(GetMaximumProcessorCount(0xFFFF), n);
        assert_eq!(GetMaximumProcessorCount(1), 0, "a group that is not here");
        assert_eq!(GetActiveProcessorGroupCount(), 1);
    }

    /// A thread of this process has its process's id, and a handle that is not a
    /// thread is refused.
    #[test]
    fn a_thread_belongs_to_this_process() {
        let me = GetCurrentThread() as usize as *mut c_void;
        assert_eq!(GetProcessIdOfThread(me), super::super::GetCurrentProcessId());
        assert_eq!(GetProcessIdOfThread(std::ptr::null_mut()), 0);
        assert_eq!(super::super::GetLastError(), ERROR_INVALID_HANDLE);
    }

    /// A thread's name is kept and handed back as a `LocalAlloc`ed string, and a
    /// thread with no name answers success with no string.
    #[test]
    fn a_thread_can_be_named() {
        let me = GetCurrentThread() as usize as *mut c_void;
        let mut name = usize::MAX;
        assert_eq!(GetThreadDescription(me, &mut name), 0);
        assert_eq!(name, 0, "a thread starts with no name");

        let text: Vec<u16> = "worker 3".encode_utf16().collect();
        let mut description = std::ptr::null_mut();
        assert_eq!(
            super::super::winrt::WindowsCreateString(
                text.as_ptr(),
                text.len() as u32,
                &mut description
            ),
            0
        );
        assert_eq!(SetThreadDescription(me, description as usize), 0);
        assert_eq!(GetThreadDescription(me, &mut name), 0);
        assert_ne!(name, 0);
        // SAFETY: the string this call just allocated: eight characters and a
        // terminator.
        unsafe {
            assert_eq!(std::slice::from_raw_parts(name as *const u16, 8), &text[..]);
            assert_eq!(*(name as *const u16).add(8), 0);
        }
        super::LocalFree(name as *mut c_void);
        super::super::winrt::WindowsDeleteString(description);

        // A name belongs to its thread: another thread does not have this one's.
        std::thread::spawn(|| {
            let me = GetCurrentThread() as usize as *mut c_void;
            let mut theirs = usize::MAX;
            assert_eq!(GetThreadDescription(me, &mut theirs), 0);
            assert_eq!(theirs, 0);
        })
        .join()
        .unwrap();

        // Clearing a name, and the refusals.
        assert_eq!(SetThreadDescription(me, 0), 0);
        assert_eq!(GetThreadDescription(me, &mut name), 0);
        assert_eq!(name, 0);
        assert_eq!(
            GetThreadDescription(me, std::ptr::null_mut()),
            0x8000_4003u32 as i32
        );
        assert_eq!(
            SetThreadDescription(std::ptr::null_mut(), 0),
            0x8007_0006u32 as i32
        );
    }

    #[test]
    fn the_five_calls_are_answered_rather_than_stubbed() {
        for (dll, name) in [
            ("api-ms-win-core-sysinfo-l1-1-0.dll", "GetMaximumProcessorCount"),
            ("api-ms-win-core-sysinfo-l1-1-0.dll", "GetActiveProcessorGroupCount"),
            ("api-ms-win-core-processthreads-l1-1-0.dll", "GetProcessIdOfThread"),
            ("api-ms-win-core-processthreads-l1-2-0.dll", "SetThreadDescription"),
            ("api-ms-win-core-processthreads-l1-2-0.dll", "GetThreadDescription"),
        ] {
            assert!(lookup(dll, name).is_some(), "{dll}!{name} is not answered");
        }
    }
}
