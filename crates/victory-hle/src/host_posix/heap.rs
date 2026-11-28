//! The Win32 heap, for the CRT DLLs the title ships in its own folder.
//!
//! `XCurl.dll`, `msvcp140.dll` and the rest import `GetProcessHeap`, `HeapAlloc`, `HeapReAlloc`
//! and `HeapFree` straight from `api-ms-win-core-heap-l1-1-0.dll` instead of reaching the C
//! runtime's `malloc` that `ucrt` serves, so a title that allocates through them had nowhere to
//! go: `victory run --list-imports` on the title's own `XCurl.dll` showed all four stubbed. There
//! is one process heap and it is never closed, so `GetProcessHeap` answers a fixed handle and the
//! other three forward to the host's `malloc`/`realloc`/`free` — the same allocator `ucrt::heap`
//! gives the C runtime, with Win32's `GetLastError` instead of the CRT's `errno` — which has the
//! alignment and the lifetime a title expects from a heap.
//!
//! Not done: `HeapSize`, `HeapCreate`/`HeapDestroy` (private heaps), `HeapValidate` and
//! `HeapSetInformation`. A title that makes a private heap still traps, and no run has shown that
//! this one does.

use std::ffi::c_void;

use super::{ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_NOT_ENOUGH_MEMORY, set_error};
use crate::modules::xmem_physical::enter;

/// `HEAP_ZERO_MEMORY`: zero the block before handing it over.
const HEAP_ZERO_MEMORY: u32 = 0x0000_0008;
/// `HEAP_REALLOC_IN_PLACE_ONLY`: grow the block where it is, or fail.
const HEAP_REALLOC_IN_PLACE_ONLY: u32 = 0x0000_0010;

/// The handle `GetProcessHeap` answers with. Windows' is opaque, so ours only has to be non-null,
/// the same every time, and clear of the small numbers `new_handle` hands out for files, events
/// and threads — it spells "HEAP" above them.
const PROCESS_HEAP: usize = 0x4845_4150_0000_0001;

/// Whether a handle is the process heap this module serves.
fn is_process_heap(heap: *mut c_void) -> bool {
    heap as usize == PROCESS_HEAP
}

/// The heap calls, by name, for `lookup`.
pub(crate) fn lookup(name: &str) -> Option<usize> {
    let address = match name {
        "GetProcessHeap" => GetProcessHeap as *const (),
        "HeapAlloc" => HeapAlloc as *const (),
        "HeapReAlloc" => HeapReAlloc as *const (),
        "HeapFree" => HeapFree as *const (),
        _ => return None,
    };
    Some(address as usize)
}

/// `GetProcessHeap`: the one heap every thread of the title allocates from.
pub extern "system" fn GetProcessHeap() -> *mut c_void {
    PROCESS_HEAP as *mut c_void
}

/// `HeapAlloc`: a block from the process heap, zeroed when `HEAP_ZERO_MEMORY` is asked for. A
/// zero-byte request fails, as Windows fails it, since the host would hand back a block the title
/// could write to, and no caller of `HeapAlloc` expects one.
pub extern "system" fn HeapAlloc(heap: *mut c_void, flags: u32, bytes: usize) -> *mut c_void {
    if !is_process_heap(heap) {
        set_error(ERROR_INVALID_HANDLE);
        return std::ptr::null_mut();
    }
    if bytes == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    let _call = enter("HeapAlloc");
    // SAFETY: a non-zero size; the block is the title's until it frees it with `HeapFree`.
    let block = unsafe { libc::malloc(bytes) };
    if block.is_null() {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    }
    if flags & HEAP_ZERO_MEMORY != 0 {
        // SAFETY: `malloc` gave `bytes` writable bytes at `block`.
        unsafe { std::ptr::write_bytes(block.cast::<u8>(), 0, bytes) };
    }
    block
}

/// `HeapReAlloc`: `realloc` of a block this heap gave out. With `HEAP_REALLOC_IN_PLACE_ONLY` the
/// block stays where it is and the call fails when it is not already big enough, which is what
/// the flag promises a caller that kept the old pointer.
pub extern "system" fn HeapReAlloc(
    heap: *mut c_void,
    flags: u32,
    block: *mut c_void,
    bytes: usize,
) -> *mut c_void {
    if !is_process_heap(heap) {
        set_error(ERROR_INVALID_HANDLE);
        return std::ptr::null_mut();
    }
    if block.is_null() || bytes == 0 {
        set_error(ERROR_INVALID_PARAMETER);
        return std::ptr::null_mut();
    }
    if flags & HEAP_REALLOC_IN_PLACE_ONLY != 0 {
        // SAFETY: `block` came from this heap, so its header is one `malloc` wrote.
        let have = unsafe { libc::malloc_size(block) };
        if have >= bytes {
            return block;
        }
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    }
    let _call = enter("HeapReAlloc");
    // SAFETY: `realloc` of a block this heap gave out, which keeps its first `bytes` or the whole
    // old block if that is smaller.
    let grown = unsafe { libc::realloc(block, bytes) };
    if grown.is_null() {
        set_error(ERROR_NOT_ENOUGH_MEMORY);
        return std::ptr::null_mut();
    }
    grown
}

/// `HeapFree`: gives a block back. A null block succeeds, as Windows counts it.
pub extern "system" fn HeapFree(heap: *mut c_void, _flags: u32, block: *mut c_void) -> i32 {
    if !is_process_heap(heap) {
        set_error(ERROR_INVALID_HANDLE);
        return 0;
    }
    if block.is_null() {
        return 1;
    }
    let _call = enter("HeapFree");
    // SAFETY: the block came from this heap, and is freed once.
    unsafe { libc::free(block) };
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_process_heap_is_one_handle_between_calls() {
        let (a, b) = (GetProcessHeap(), GetProcessHeap());
        assert!(!a.is_null());
        assert_eq!(a, b);
    }

    #[test]
    fn a_block_is_zeroed_on_request_and_survives_a_grow() {
        let heap = GetProcessHeap();
        // SAFETY: the block is `bytes` long and ours until it is freed below.
        let block = HeapAlloc(heap, HEAP_ZERO_MEMORY, 64);
        assert!(!block.is_null());
        // SAFETY: 64 bytes of a block this call just made.
        assert!(unsafe { std::slice::from_raw_parts(block.cast::<u8>(), 64) }
            .iter()
            .all(|&b| b == 0));
        // SAFETY: the same 64 bytes, written once.
        unsafe { block.cast::<u8>().write_bytes(0xAB, 64) };

        let grown = HeapReAlloc(heap, 0, block, 256);
        assert!(!grown.is_null());
        // SAFETY: the first 64 bytes are the ones written above, and `realloc` kept them.
        assert!(unsafe { std::slice::from_raw_parts(grown.cast::<u8>(), 64) }
            .iter()
            .all(|&b| b == 0xAB));
        // SAFETY: the block came from this heap and is freed once.
        unsafe { assert_eq!(HeapFree(heap, 0, grown), 1) };
    }

    #[test]
    fn a_foreign_heap_a_zero_size_and_a_block_that_cannot_grow_in_place_are_refused() {
        let heap = GetProcessHeap();
        let other = 0x1234_5678usize as *mut c_void;
        assert!(HeapAlloc(other, 0, 16).is_null());
        assert!(HeapAlloc(heap, 0, 0).is_null(), "a zero-byte request");
        let block = HeapAlloc(heap, 0, 16);
        assert!(!block.is_null());
        // 1 GiB does not fit the block `malloc` gave for 16 bytes, so an in-place grow fails and
        // the caller keeps the pointer it had.
        assert!(HeapReAlloc(heap, HEAP_REALLOC_IN_PLACE_ONLY, block, 1 << 30).is_null());
        assert_eq!(HeapReAlloc(heap, HEAP_REALLOC_IN_PLACE_ONLY, block, 8), block);
        // A null block is a success, and a foreign heap a failure.
        assert_eq!(HeapFree(heap, 0, std::ptr::null_mut()), 1);
        assert_eq!(HeapFree(other, 0, block), 0);
        // SAFETY: the block came from this heap and is freed once.
        unsafe { libc::free(block) };
    }
}
