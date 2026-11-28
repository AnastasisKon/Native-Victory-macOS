//! `api-ms-win-crt-heap`: the host allocator (16-byte aligned, and
//! `_aligned_*` memory is freeable by `free`, so either free works).

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};

use super::{EINVAL, ENOMEM, exports, libc, set_errno};

static NEW_MODE: AtomicI32 = AtomicI32::new(0);

fn checked(p: *mut c_void) -> *mut c_void {
    if p.is_null() {
        set_errno(ENOMEM);
    }
    p
}

pub extern "system" fn malloc(size: usize) -> *mut c_void {
    // SAFETY: plain allocation.
    checked(unsafe { libc::malloc(size.max(1)) })
}

pub extern "system" fn calloc(count: usize, size: usize) -> *mut c_void {
    let (count, size) = if count == 0 || size == 0 {
        (1, 1)
    } else {
        (count, size)
    };
    // SAFETY: plain allocation (overflow gives null).
    checked(unsafe { libc::calloc(count, size) })
}

pub extern "system" fn realloc(p: *mut c_void, size: usize) -> *mut c_void {
    if size == 0 && !p.is_null() {
        // The CRT frees on a zero size and returns null.
        // SAFETY: the title's own allocation.
        unsafe { libc::free(p) };
        return std::ptr::null_mut();
    }
    // SAFETY: null or the title's own allocation.
    checked(unsafe { libc::realloc(p, size.max(1)) })
}

pub extern "system" fn free(p: *mut c_void) {
    if !p.is_null() {
        // SAFETY: the title's own allocation.
        unsafe { libc::free(p) };
    }
}

pub extern "system" fn _recalloc(p: *mut c_void, count: usize, size: usize) -> *mut c_void {
    let Some(total) = count.checked_mul(size) else {
        set_errno(ENOMEM);
        return std::ptr::null_mut();
    };
    if total == 0 {
        return if p.is_null() {
            calloc(1, 1)
        } else {
            free(p);
            std::ptr::null_mut()
        };
    }
    let old = if p.is_null() {
        0
    } else {
        // SAFETY: the title's own allocation.
        unsafe { libc::malloc_size(p) }
    };
    let q = realloc(p, total);
    if !q.is_null() && total > old {
        // SAFETY: the new tail of the block just resized.
        unsafe { q.cast::<u8>().add(old).write_bytes(0, total - old) };
    }
    q
}

pub extern "system" fn _msize(p: *mut c_void) -> usize {
    if p.is_null() {
        set_errno(EINVAL);
        return usize::MAX;
    }
    // SAFETY: the title's own allocation.
    unsafe { libc::malloc_size(p) }
}

pub extern "system" fn _expand(_p: *mut c_void, _size: usize) -> *mut c_void {
    // Never grows in place.
    std::ptr::null_mut()
}

/// The header an aligned block carries before its pointer: the block to free,
/// then the size that was asked for. Offset alignment needs it - the returned
/// pointer is not the block's start - and having it makes `_aligned_msize` the
/// size the caller asked for rather than the host's own idea of it.
const ALIGNED_HEADER: usize = 16;

fn checked_align(align: usize) -> Option<usize> {
    (align.is_power_of_two() && align >= size_of::<usize>()).then_some(align)
}

/// A block whose `p` is `offset` past a multiple of `align`, with the header
/// written before it.
fn aligned(size: usize, align: usize, offset: usize) -> *mut c_void {
    let Some(align) = checked_align(align) else {
        set_errno(EINVAL);
        return std::ptr::null_mut();
    };
    let shift = offset % align;
    let bytes = size.max(1) + align + ALIGNED_HEADER;
    let mut out = std::ptr::null_mut();
    // SAFETY: a valid out pointer, and 16 is a power of two.
    let r = unsafe { libc::posix_memalign(&mut out, align.max(16), bytes) };
    if r != 0 {
        set_errno(ENOMEM);
        return std::ptr::null_mut();
    }
    let block = out.cast::<u8>();
    // The first address at or after the header that is `shift` past a multiple of
    // `align`: that is what makes `p - offset` aligned.
    let start = block as usize + ALIGNED_HEADER;
    let p = start + (shift + align - start % align) % align;
    // SAFETY: `p` is inside the block, with the header's room before it.
    unsafe {
        let header = (p - ALIGNED_HEADER) as *mut usize;
        header.write_unaligned(block as usize);
        header.add(1).write_unaligned(size);
    }
    p as *mut c_void
}

pub extern "system" fn _aligned_malloc(size: usize, align: usize) -> *mut c_void {
    aligned(size, align, 0)
}

/// The pointer is `offset` bytes past an `align`-aligned address, which is how a
/// structure with a header of its own is allocated.
pub extern "system" fn _aligned_offset_malloc(size: usize, align: usize, offset: usize) -> *mut c_void {
    aligned(size, align, offset)
}

pub extern "system" fn _aligned_free(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    // SAFETY: the header `aligned` wrote before this pointer.
    let block = unsafe { p.cast::<u8>().sub(ALIGNED_HEADER).cast::<usize>().read_unaligned() };
    // SAFETY: the block `aligned` was given by `posix_memalign`.
    unsafe { libc::free(block as *mut c_void) };
}

pub extern "system" fn _aligned_msize(p: *mut c_void, _align: usize, _offset: usize) -> usize {
    if p.is_null() {
        return 0;
    }
    // SAFETY: the header's size word, written when the block was made.
    unsafe { p.cast::<u8>().sub(ALIGNED_HEADER - 8).cast::<usize>().read_unaligned() }
}

pub extern "system" fn _aligned_realloc(p: *mut c_void, size: usize, align: usize) -> *mut c_void {
    if p.is_null() {
        return _aligned_malloc(size, align);
    }
    if size == 0 {
        _aligned_free(p);
        return std::ptr::null_mut();
    }
    let was = _aligned_msize(p, align, 0);
    let q = _aligned_malloc(size, align);
    if !q.is_null() {
        // SAFETY: both are live blocks, and neither copy runs past either.
        unsafe {
            std::ptr::copy_nonoverlapping(p.cast::<u8>(), q.cast::<u8>(), was.min(size));
        }
        _aligned_free(p);
    }
    q
}

pub extern "system" fn _set_new_mode(mode: i32) -> i32 {
    if !(0..=1).contains(&mode) {
        set_errno(EINVAL);
        return -1;
    }
    NEW_MODE.swap(mode, Ordering::Relaxed)
}

pub extern "system" fn _query_new_mode() -> i32 {
    NEW_MODE.load(Ordering::Relaxed)
}

/// No new-handler: `operator new` does not retry.
pub extern "system" fn _callnewh(_size: usize) -> i32 {
    0
}

pub extern "system" fn _set_new_handler(_handler: usize) -> usize {
    0
}

pub extern "system" fn _get_heap_handle() -> usize {
    1
}

/// `_HEAPOK`.
pub extern "system" fn _heapchk() -> i32 {
    -2
}

pub extern "system" fn _heapmin() -> i32 {
    0
}

exports!(pub(super) fn lookup;
    malloc, calloc, realloc, free, _recalloc, _msize, _expand, _aligned_malloc, _aligned_free,
    _aligned_realloc, _aligned_msize, _aligned_offset_malloc, _set_new_mode, _query_new_mode, _callnewh,
    _set_new_handler, _get_heap_handle, _heapchk, _heapmin;
    "_malloc_base" => malloc, "_calloc_base" => calloc, "_realloc_base" => realloc,
    "_free_base" => free, "_msize_base" => _msize, "_recalloc_base" => _recalloc);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_16_byte_aligned_and_resizable() {
        let p = malloc(100);
        assert_eq!(p as usize % 16, 0);
        assert!(_msize(p) >= 100);
        let p = realloc(p, 1000);
        assert!(_msize(p) >= 1000);
        assert!(realloc(p, 0).is_null());
        let z = calloc(4, 8);
        // SAFETY: 32 bytes were allocated.
        assert!(
            unsafe { std::slice::from_raw_parts(z.cast::<u8>(), 32) }
                .iter()
                .all(|&b| b == 0)
        );
        let z = _recalloc(z, 8, 8);
        // SAFETY: 64 bytes now.
        assert!(
            unsafe { std::slice::from_raw_parts(z.cast::<u8>(), 64) }
                .iter()
                .all(|&b| b == 0)
        );
        free(z);
        let a = _aligned_malloc(10, 256);
        assert_eq!(a as usize % 256, 0);
        let a = _aligned_realloc(a, 5000, 256);
        assert_eq!(a as usize % 256, 0);
        _aligned_free(a);
        assert!(_aligned_malloc(8, 3).is_null());
        assert_eq!(_set_new_mode(1), 0);
        assert_eq!(_set_new_mode(0), 1);
    }

    #[test]
    fn calloc_and_recalloc_handle_zero_counts_and_sizes() {
        let a = calloc(0, usize::MAX);
        assert!(!a.is_null());
        free(a);
        let b = calloc(usize::MAX, 0);
        assert!(!b.is_null());
        free(b);

        let p = calloc(4, 8);
        assert!(!p.is_null());
        assert!(_recalloc(p, 0, 8).is_null());
        let p = calloc(4, 8);
        assert!(!p.is_null());
        assert!(_recalloc(p, 4, 0).is_null());

        let z = _recalloc(std::ptr::null_mut(), 0, 8);
        assert!(!z.is_null());
        // SAFETY: one zeroed byte was allocated.
        assert_eq!(unsafe { z.cast::<u8>().read() }, 0);
        free(z);
    }
}

#[cfg(test)]
mod aligned_tests {
    use super::*;

    /// An aligned block is aligned, says the size it was asked for, and is freed
    /// from the header it carries.
    #[test]
    fn an_aligned_block_knows_where_it_came_from() {
        for align in [16usize, 64, 256, 4096] {
            let p = _aligned_malloc(100, align);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "alignment {align}");
            assert_eq!(_aligned_msize(p, align, 0), 100);
            // SAFETY: the 100 bytes this call allocated.
            unsafe { std::ptr::write_bytes(p.cast::<u8>(), 0x5A, 100) };
            _aligned_free(p);
        }
        assert!(_aligned_malloc(8, 3).is_null());
        // Freeing nothing is not an error.
        _aligned_free(std::ptr::null_mut());
    }

    /// The offset form puts the pointer `offset` past an aligned address, which
    /// is what an allocation for a structure with its own header needs.
    #[test]
    fn the_offset_form_aligns_the_address_before_the_offset() {
        for (align, offset) in [(16usize, 4usize), (64, 8), (256, 100), (4096, 4096 - 8)] {
            let p = _aligned_offset_malloc(64, align, offset) as usize;
            assert_ne!(p, 0);
            assert_eq!((p - offset) % align, 0, "align {align} offset {offset}");
            assert_eq!(_aligned_msize(p as *mut c_void, align, offset), 64);
            // The 64 bytes are writable from the returned pointer.
            // SAFETY: the 64 bytes this call allocated.
            unsafe { std::ptr::write_bytes(p as *mut u8, 0x11, 64) };
            _aligned_free(p as *mut c_void);
        }
        assert!(_aligned_offset_malloc(16, 3, 0).is_null());
    }

    /// Reallocating keeps what fit, and the block stays aligned.
    #[test]
    fn reallocating_keeps_the_characters_that_fit() {
        const TEXT: &[u8] = b"abcdefg";
        let p = _aligned_malloc(8, 64).cast::<u8>();
        // SAFETY: the 8 bytes this call allocated, and the 7 of the text.
        unsafe {
            std::ptr::copy_nonoverlapping(TEXT.as_ptr(), p, TEXT.len());
            *p.add(7) = 0;
        }
        let q = _aligned_realloc(p.cast(), 4096, 64).cast::<u8>();
        assert_eq!(q as usize % 64, 0);
        assert_eq!(_aligned_msize(q.cast(), 64, 0), 4096);
        // SAFETY: the first block's 8 bytes were copied into this one.
        assert_eq!(unsafe { std::slice::from_raw_parts(q, 8) }, b"abcdefg\0");
        _aligned_free(q.cast());
    }

    #[test]
    fn the_offset_call_is_answered_rather_than_stubbed() {
        assert!(
            super::super::super::lookup(
                "api-ms-win-crt-heap-l1-1-0.dll",
                "_aligned_offset_malloc"
            )
            .is_some()
        );
    }
}
