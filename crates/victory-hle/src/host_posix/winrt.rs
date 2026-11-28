//! The WinRT strings (`HSTRING`) for the native macOS host.
//!
//! A title treats an `HSTRING` as a handle, so only this module needs to know
//! what is behind one. Behind it is a 16-byte header - the character count, the
//! flags, and where the characters are - which is also the whole of what
//! `WindowsCreateStringReference` writes into the caller's `HSTRING_HEADER`, so a
//! reference string costs no allocation and copies nothing.
//!
//! An owned string is one allocation: the reference count, the header, then the
//! characters. `WindowsDuplicateString` shares it by count; `WindowsDeleteString`
//! frees it when the last handle goes. A reference string is never freed - the
//! characters are the caller's, and deleting the handle is only forgetting it.
//!
//! shortcut: `WindowsPreallocateStringBuffer` and its pair are not here; the
//! shipped dlls build strings through `WindowsCreateString`.
#![allow(non_snake_case)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};

type Hstring = *mut u8;

const S_OK: i32 = 0;
const E_POINTER: i32 = 0x8000_4003u32 as i32;

/// The header's layout, as this module reads it.
const LENGTH_AT: usize = 0;
const FLAGS_AT: usize = 4;
const DATA_AT: usize = 8;
const HEADER_SIZE: usize = 16;

/// Set for a string whose characters are the caller's, not this module's.
const REFERENCE: u32 = 1;

/// The count sits before the header, so a handle is the header alone.
const COUNT_BEFORE: usize = 16;

/// What `WindowsGetStringRawBuffer` answers for a null handle: a string of no
/// characters, which is what Windows answers and what a caller may read.
static EMPTY: [u16; 1] = [0];

/// The length in characters of `string`, zero for no handle.
///
/// # Safety
///
/// `string` is an `HSTRING` this module made, or null.
unsafe fn length_of(string: Hstring) -> u32 {
    if string.is_null() {
        return 0;
    }
    unsafe { string.add(LENGTH_AT).cast::<u32>().read_unaligned() }
}

/// # Safety
///
/// See [`length_of`].
unsafe fn flags_of(string: Hstring) -> u32 {
    unsafe { string.add(FLAGS_AT).cast::<u32>().read_unaligned() }
}

/// # Safety
///
/// See [`length_of`].
unsafe fn data_of(string: Hstring) -> *const u16 {
    if string.is_null() {
        return EMPTY.as_ptr();
    }
    unsafe { string.add(DATA_AT).cast::<usize>().read_unaligned() as *const u16 }
}

/// An owned string: the count, the header, then a copy of the characters.
fn own(characters: &[u16]) -> Hstring {
    let bytes = COUNT_BEFORE + HEADER_SIZE + characters.len() * size_of::<u16>();
    let layout = std::alloc::Layout::from_size_align(bytes, 8).expect("a string's layout");
    // SAFETY: a non-zero layout, and the block is kept until the last handle
    // goes.
    let block = unsafe { std::alloc::alloc(layout) };
    if block.is_null() {
        return std::ptr::null_mut();
    }
    let string = block.wrapping_add(COUNT_BEFORE);
    // SAFETY: `bytes` of the block just allocated.
    unsafe {
        block.cast::<AtomicU32>().write(AtomicU32::new(1));
        string.add(LENGTH_AT).cast::<u32>().write_unaligned(characters.len() as u32);
        string.add(FLAGS_AT).cast::<u32>().write_unaligned(0);
        let data = string.add(HEADER_SIZE).cast::<u16>();
        string.add(DATA_AT).cast::<usize>().write_unaligned(data as usize);
        std::ptr::copy_nonoverlapping(characters.as_ptr(), data, characters.len());
    }
    string
}

/// Drops one handle to a string this module owns.
///
/// # Safety
///
/// `string` is an owned `HSTRING` this module made.
unsafe fn release(string: Hstring) {
    let block = string.wrapping_sub(COUNT_BEFORE);
    // SAFETY: the count of an owned string, written when it was made.
    let count = unsafe { &*block.cast::<AtomicU32>() };
    if count.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    let characters = unsafe { length_of(string) } as usize;
    let bytes = COUNT_BEFORE + HEADER_SIZE + characters * size_of::<u16>();
    if let Ok(layout) = std::alloc::Layout::from_size_align(bytes, 8) {
        // SAFETY: the block `own` allocated with this layout.
        unsafe { std::alloc::dealloc(block, layout) };
    }
}

/// `WindowsCreateString`: the characters are copied, so the caller's buffer may
/// go away as soon as this returns.
pub extern "system" fn WindowsCreateString(source: *const u16, len: u32, out: *mut Hstring) -> i32 {
    if out.is_null() || (source.is_null() && len > 0) {
        return E_POINTER;
    }
    let characters: &[u16] = if len == 0 {
        &[]
    } else {
        // SAFETY: the caller's buffer of `len` characters.
        unsafe { std::slice::from_raw_parts(source, len as usize) }
    };
    // An empty string has no handle, as Windows answers it.
    let string = if len == 0 {
        std::ptr::null_mut()
    } else {
        own(characters)
    };
    if len > 0 && string.is_null() {
        return 0x8007_000Eu32 as i32; // E_OUTOFMEMORY
    }
    // SAFETY: the caller's out handle.
    unsafe { out.write(string) };
    S_OK
}

/// `WindowsCreateStringReference`: the header goes into the caller's
/// `HSTRING_HEADER` and the characters are not copied at all.
pub extern "system" fn WindowsCreateStringReference(
    source: *const u16,
    len: u32,
    header: *mut c_void,
    out: *mut Hstring,
) -> i32 {
    if out.is_null() || header.is_null() || (source.is_null() && len > 0) {
        return E_POINTER;
    }
    let header = header.cast::<u8>();
    // SAFETY: the caller's header, which Windows lets this call fill, and its out
    // handle. The header this writes is 16 bytes, less than any `HSTRING_HEADER`.
    unsafe {
        header.add(LENGTH_AT).cast::<u32>().write_unaligned(len);
        header.add(FLAGS_AT).cast::<u32>().write_unaligned(REFERENCE);
        let data = if len == 0 { EMPTY.as_ptr() } else { source };
        header.add(DATA_AT).cast::<usize>().write_unaligned(data as usize);
        out.write(header);
    }
    S_OK
}

/// `WindowsDeleteString`: a null handle is not an error, and a reference string
/// is only forgotten.
pub extern "system" fn WindowsDeleteString(string: Hstring) -> i32 {
    if string.is_null() {
        return S_OK;
    }
    // SAFETY: a handle this module made.
    let flags = unsafe { flags_of(string) };
    if flags & REFERENCE == 0 {
        // SAFETY: an owned string, whose block this frees.
        unsafe { release(string) };
    }
    S_OK
}

/// `WindowsDuplicateString`: another handle to the same characters.
pub extern "system" fn WindowsDuplicateString(string: Hstring, out: *mut Hstring) -> i32 {
    if out.is_null() {
        return E_POINTER;
    }
    if string.is_null() {
        // SAFETY: the caller's out handle.
        unsafe { out.write(std::ptr::null_mut()) };
        return S_OK;
    }
    // SAFETY: a handle this module made.
    let (flags, length, data) = unsafe { (flags_of(string), length_of(string), data_of(string)) };
    let copy = if flags & REFERENCE == 0 {
        // Another handle to the same block.
        // SAFETY: the count of an owned string.
        unsafe { &*string.wrapping_sub(COUNT_BEFORE).cast::<AtomicU32>() }.fetch_add(1, Ordering::AcqRel);
        string
    } else {
        // A reference string has no count to share, so the characters are copied
        // into a string of this module's own.
        // SAFETY: the caller's characters, of `length`.
        own(unsafe { std::slice::from_raw_parts(data, length as usize) })
    };
    if copy.is_null() {
        return 0x8007_000Eu32 as i32; // E_OUTOFMEMORY
    }
    // SAFETY: the caller's out handle.
    unsafe { out.write(copy) };
    S_OK
}

/// `WindowsGetStringLen`: the count in characters, without a terminator.
pub extern "system" fn WindowsGetStringLen(string: Hstring) -> u32 {
    // SAFETY: a handle this module made, or null.
    unsafe { length_of(string) }
}

/// `WindowsGetStringRawBuffer`: where the characters are, and how many.
pub extern "system" fn WindowsGetStringRawBuffer(string: Hstring, len: *mut u32) -> *const u16 {
    if !len.is_null() {
        // SAFETY: the caller's out count.
        unsafe { len.write_unaligned(unsafe { length_of(string) }) };
    }
    // SAFETY: a handle this module made, or null.
    unsafe { data_of(string) }
}

/// `WindowsIsStringEmpty`: no handle is empty, and so is a string of no
/// characters.
pub extern "system" fn WindowsIsStringEmpty(string: Hstring) -> i32 {
    // SAFETY: a handle this module made, or null.
    i32::from(unsafe { length_of(string) } == 0)
}

/// `WindowsStringHasEmbeddedNull`: whether a terminator sits before the end.
pub extern "system" fn WindowsStringHasEmbeddedNull(string: Hstring, has_null: *mut i32) -> i32 {
    if has_null.is_null() {
        return E_POINTER;
    }
    // SAFETY: a handle this module made, or null, and the caller's out flag.
    let found = unsafe {
        let length = length_of(string) as usize;
        let data = data_of(string);
        (0..length).any(|i| *data.add(i) == 0)
    };
    // SAFETY: the caller's out flag.
    unsafe { has_null.write_unaligned(i32::from(found)) };
    S_OK
}

/// `WindowsCompareStringOrdinal`: the characters themselves, in code unit order,
/// compared over their length rather than to a terminator.
pub extern "system" fn WindowsCompareStringOrdinal(
    left: Hstring,
    right: Hstring,
    result: *mut i32,
) -> i32 {
    if result.is_null() {
        return E_POINTER;
    }
    // SAFETY: handles this module made, or null.
    let compare = unsafe {
        let l = std::slice::from_raw_parts(data_of(left), length_of(left) as usize);
        let r = std::slice::from_raw_parts(data_of(right), length_of(right) as usize);
        l.cmp(r)
    };
    let answer = match compare {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    };
    // SAFETY: the caller's out result.
    unsafe { result.write_unaligned(answer) };
    S_OK
}

pub(super) fn lookup(dll: &str, name: &str) -> Option<usize> {
    // `WindowsCreateString` lives in the winrt-string API set, and `combase.dll`
    // forwards the same names.
    if !dll.starts_with("api-ms-win-core-winrt-string-") && dll != "combase.dll" {
        return None;
    }
    let address = match name {
        "WindowsCreateString" => WindowsCreateString as *const (),
        "WindowsCreateStringReference" => WindowsCreateStringReference as *const (),
        "WindowsDeleteString" => WindowsDeleteString as *const (),
        "WindowsDuplicateString" => WindowsDuplicateString as *const (),
        "WindowsGetStringLen" => WindowsGetStringLen as *const (),
        "WindowsGetStringRawBuffer" => WindowsGetStringRawBuffer as *const (),
        "WindowsIsStringEmpty" => WindowsIsStringEmpty as *const (),
        "WindowsStringHasEmbeddedNull" => WindowsStringHasEmbeddedNull as *const (),
        "WindowsCompareStringOrdinal" => WindowsCompareStringOrdinal as *const (),
        _ => return None,
    };
    Some(address as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(text: &str) -> Hstring {
        let wide: Vec<u16> = text.encode_utf16().collect();
        let mut string: Hstring = std::ptr::null_mut();
        assert_eq!(WindowsCreateString(wide.as_ptr(), wide.len() as u32, &mut string), S_OK);
        string
    }

    fn text_of(string: Hstring) -> String {
        let mut len = u32::MAX;
        // SAFETY: a handle from this module.
        let data = WindowsGetStringRawBuffer(string, &mut len);
        assert_eq!(len, WindowsGetStringLen(string));
        // SAFETY: the characters of that handle, of that length.
        String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(data, len as usize) })
    }

    /// A created string holds what it was given, and its length is in
    /// characters, not bytes.
    #[test]
    fn a_created_string_holds_its_characters() {
        let string = create("h\u{e9}llo");
        assert!(!string.is_null());
        assert_eq!(WindowsGetStringLen(string), 5);
        assert_eq!(text_of(string), "h\u{e9}llo");
        assert_eq!(WindowsIsStringEmpty(string), 0);

        let empty = create("");
        // An empty string is no handle at all, as Windows answers it.
        assert!(empty.is_null());
        assert_eq!(WindowsIsStringEmpty(empty), 1);
        assert_eq!(WindowsGetStringLen(empty), 0);
        // The raw buffer of no handle is a string of no characters, not a null
        // pointer a caller might dereference.
        let mut len = u32::MAX;
        assert!(!WindowsGetStringRawBuffer(empty, &mut len).is_null());
        assert_eq!(len, 0);

        assert_eq!(WindowsDeleteString(string), S_OK);
        // Deleting no handle is not an error either.
        assert_eq!(WindowsDeleteString(std::ptr::null_mut()), S_OK);
    }

    /// A reference string points at the caller's characters: nothing is copied,
    /// and deleting it leaves them alone.
    #[test]
    fn a_reference_string_borrows_the_callers_characters() {
        let wide: Vec<u16> = "borrowed".encode_utf16().collect();
        let mut header = [0u64; 3]; // an `HSTRING_HEADER`, which is no bigger
        let mut string: Hstring = std::ptr::null_mut();
        assert_eq!(
            WindowsCreateStringReference(
                wide.as_ptr(),
                wide.len() as u32,
                header.as_mut_ptr().cast(),
                &mut string
            ),
            S_OK
        );
        assert_eq!(WindowsGetStringLen(string), 8);
        let mut len = u32::MAX;
        assert_eq!(
            WindowsGetStringRawBuffer(string, &mut len),
            wide.as_ptr(),
            "the caller's own characters"
        );
        assert_eq!(text_of(string), "borrowed");
        assert_eq!(WindowsDeleteString(string), S_OK);
        // The header is still the caller's, and reading it now is not a use of
        // freed memory because nothing was ever allocated.
        assert_eq!(wide[0], u16::from(b'b'));

        // A reference needs somewhere to put the header.
        assert_eq!(
            WindowsCreateStringReference(wide.as_ptr(), 8, std::ptr::null_mut(), &mut string),
            E_POINTER
        );
    }

    /// Duplicating an owned string shares it, and the last handle to go frees it;
    /// duplicating a reference string copies the characters.
    #[test]
    fn a_duplicate_survives_the_original() {
        let original = create("shared");
        let mut copy: Hstring = std::ptr::null_mut();
        assert_eq!(WindowsDuplicateString(original, &mut copy), S_OK);
        assert_eq!(copy, original, "an owned string is shared, not copied");
        assert_eq!(WindowsDeleteString(original), S_OK);
        // The copy still holds the characters, and frees them when it goes.
        assert_eq!(text_of(copy), "shared");
        assert_eq!(WindowsDeleteString(copy), S_OK);

        let wide: Vec<u16> = "borrowed".encode_utf16().collect();
        let mut header = [0u64; 3];
        let mut borrowed: Hstring = std::ptr::null_mut();
        assert_eq!(
            WindowsCreateStringReference(
                wide.as_ptr(),
                wide.len() as u32,
                header.as_mut_ptr().cast(),
                &mut borrowed
            ),
            S_OK
        );
        let mut copy: Hstring = std::ptr::null_mut();
        assert_eq!(WindowsDuplicateString(borrowed, &mut copy), S_OK);
        assert_ne!(copy, borrowed, "a reference has no count to share");
        assert_eq!(text_of(copy), "borrowed");
        assert_eq!(WindowsDeleteString(copy), S_OK);

        // No handle duplicates to no handle.
        let mut none: Hstring = 1 as Hstring;
        assert_eq!(WindowsDuplicateString(std::ptr::null_mut(), &mut none), S_OK);
        assert!(none.is_null());
    }

    /// Comparing is over the characters and their length, and an embedded null is
    /// a character like any other for the comparison and for the null check.
    #[test]
    fn strings_compare_by_their_characters() {
        let (a, b, c) = (create("abc"), create("abd"), create("ab"));
        let mut answer = 99;
        assert_eq!(WindowsCompareStringOrdinal(a, a, &mut answer), S_OK);
        assert_eq!(answer, 0);
        assert_eq!(WindowsCompareStringOrdinal(a, b, &mut answer), S_OK);
        assert_eq!(answer, -1);
        assert_eq!(WindowsCompareStringOrdinal(b, a, &mut answer), S_OK);
        assert_eq!(answer, 1);
        // A prefix comes first, which a comparison to a terminator would call
        // equal.
        assert_eq!(WindowsCompareStringOrdinal(c, a, &mut answer), S_OK);
        assert_eq!(answer, -1);
        // No handle is the empty string.
        assert_eq!(WindowsCompareStringOrdinal(std::ptr::null_mut(), c, &mut answer), S_OK);
        assert_eq!(answer, -1);

        // An embedded null: two characters, one of them a terminator.
        let wide = [u16::from(b'a'), 0, u16::from(b'b')];
        let mut string: Hstring = std::ptr::null_mut();
        assert_eq!(WindowsCreateString(wide.as_ptr(), 2, &mut string), S_OK);
        assert_eq!(WindowsGetStringLen(string), 2, "the length is not the text's");
        let mut has_null = 0;
        assert_eq!(WindowsStringHasEmbeddedNull(string, &mut has_null), S_OK);
        assert_eq!(has_null, 1);
        let mut has_null = 1;
        assert_eq!(WindowsStringHasEmbeddedNull(a, &mut has_null), S_OK);
        assert_eq!(has_null, 0);
        assert_eq!(WindowsDeleteString(string), S_OK);

        for string in [a, b, c] {
            assert_eq!(WindowsDeleteString(string), S_OK);
        }
    }

    /// The out parameters are required, as Windows requires them.
    #[test]
    fn a_null_out_parameter_is_refused() {
        let wide: Vec<u16> = "x".encode_utf16().collect();
        assert_eq!(
            WindowsCreateString(wide.as_ptr(), 1, std::ptr::null_mut()),
            E_POINTER
        );
        assert_eq!(
            WindowsDuplicateString(create("x"), std::ptr::null_mut()),
            E_POINTER
        );
        assert_eq!(
            WindowsCompareStringOrdinal(std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()),
            E_POINTER
        );
        assert_eq!(
            WindowsStringHasEmbeddedNull(std::ptr::null_mut(), std::ptr::null_mut()),
            E_POINTER
        );
        // A length with no characters is not a string.
        assert_eq!(WindowsCreateString(std::ptr::null(), 4, &mut std::ptr::null_mut()), E_POINTER);
    }

    #[test]
    fn the_nine_names_are_answered_rather_than_stubbed() {
        for name in [
            "WindowsCreateString",
            "WindowsCreateStringReference",
            "WindowsDeleteString",
            "WindowsDuplicateString",
            "WindowsGetStringLen",
            "WindowsGetStringRawBuffer",
            "WindowsIsStringEmpty",
            "WindowsStringHasEmbeddedNull",
            "WindowsCompareStringOrdinal",
        ] {
            assert!(
                lookup("api-ms-win-core-winrt-string-l1-1-0.dll", name).is_some(),
                "{name} is not answered"
            );
            assert!(lookup("combase.dll", name).is_some(), "{name} is not forwarded");
        }
    }
}
