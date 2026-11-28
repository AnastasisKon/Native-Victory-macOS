//! `OLEAUT32`'s strings (`BSTR`) for the native macOS host.
//!
//! A `BSTR` is a pointer to UTF-16 characters, and the four bytes immediately
//! before it hold the string's length in bytes, not counting the terminator. That
//! length is what the `SysString*` calls read, and it is why a `BSTR` can hold a
//! terminator as a character of its own.
//!
//! The calls here are the ones `vccorlib140.dll` imports - by ordinal, which is
//! how C++/CX reaches them - and the rest of the family that goes with them.
#![allow(non_snake_case)]

use std::ffi::c_void;

/// The length in bytes sits before the characters.
const PREFIX: usize = size_of::<u32>();

/// The bytes of an allocation for `chars` characters and their terminator.
fn allocation(chars: usize) -> usize {
    PREFIX + (chars + 1) * size_of::<u16>()
}

/// A string of `chars` characters from `source`, which may be null for a string
/// of zeros.
///
/// # Safety
///
/// `source` is null, or `chars` characters of the caller's.
unsafe fn allocate(source: *const u16, chars: usize) -> *mut u16 {
    // SAFETY: a non-zero allocation; the block is kept until `SysFreeString`.
    let block = unsafe { libc::malloc(allocation(chars)) }.cast::<u8>();
    if block.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the block just allocated, and the caller's `chars` characters.
    unsafe {
        block
            .cast::<u32>()
            .write_unaligned((chars * size_of::<u16>()) as u32);
        let data = block.add(PREFIX).cast::<u16>();
        if source.is_null() {
            std::ptr::write_bytes(data, 0, chars + 1);
        } else {
            std::ptr::copy_nonoverlapping(source, data, chars);
            data.add(chars).write_unaligned(0);
        }
        data
    }
}

/// The length in bytes of `string`, or zero for no string.
///
/// # Safety
///
/// `string` is a `BSTR` this module made, or null.
unsafe fn byte_length(string: *const u16) -> u32 {
    if string.is_null() {
        return 0;
    }
    unsafe { string.cast::<u8>().sub(PREFIX).cast::<u32>().read_unaligned() }
}

/// # Safety
///
/// `text` is null or NUL-terminated.
unsafe fn characters(text: *const u16) -> usize {
    if text.is_null() {
        return 0;
    }
    let mut n = 0usize;
    // SAFETY: the caller's NUL-terminated string.
    unsafe {
        while *text.add(n) != 0 {
            n += 1;
        }
    }
    n
}

/// No string in is no string out, as Windows answers it; a string of no
/// characters is only what `SysAllocStringLen` can ask for.
pub extern "system" fn SysAllocString(text: *const u16) -> *mut u16 {
    if text.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller's NUL-terminated string.
    unsafe { allocate(text, characters(text)) }
}

/// The length is the caller's, so a terminator inside the characters is a
/// character like any other.
pub extern "system" fn SysAllocStringLen(text: *const u16, chars: u32) -> *mut u16 {
    // SAFETY: the caller's optional buffer of `chars` characters.
    unsafe { allocate(text, chars as usize) }
}

pub extern "system" fn SysFreeString(string: *mut u16) {
    if string.is_null() {
        return;
    }
    // SAFETY: the block `allocate` made for this string.
    unsafe { libc::free(string.cast::<u8>().sub(PREFIX).cast()) };
}

pub extern "system" fn SysStringLen(string: *const u16) -> u32 {
    // SAFETY: a string this module made, or null.
    unsafe { byte_length(string) / size_of::<u16>() as u32 }
}

pub extern "system" fn SysStringByteLen(string: *const u16) -> u32 {
    // SAFETY: a string this module made, or null.
    unsafe { byte_length(string) }
}

/// `SysReAllocString`: the string a pointer points at becomes `text`.
pub extern "system" fn SysReAllocString(string: *mut *mut u16, text: *const u16) -> i32 {
    if string.is_null() || text.is_null() {
        return 0;
    }
    // SAFETY: the caller's string slot and its NUL-terminated text.
    unsafe {
        let chars = characters(text);
        let fresh = allocate(text, chars);
        if fresh.is_null() && chars > 0 {
            return 0;
        }
        SysFreeString(*string);
        *string = fresh;
    }
    1
}

/// `SysReAllocStringLen`: the same, with the length taken from the caller.
pub extern "system" fn SysReAllocStringLen(
    string: *mut *mut u16,
    text: *const u16,
    chars: u32,
) -> i32 {
    if string.is_null() {
        return 0;
    }
    // SAFETY: the caller's string slot and optional text.
    unsafe {
        let fresh = allocate(text, chars as usize);
        if fresh.is_null() && chars > 0 {
            return 0;
        }
        SysFreeString(*string);
        *string = fresh;
    }
    1
}

pub(super) fn lookup(dll: &str, name: &str) -> Option<usize> {
    if dll != "oleaut32.dll" && dll != "oleaut.dll" {
        return None;
    }
    // The ordinals are the ones `oleaut32.dll` has always had, which is how the
    // C++ runtime reaches these: 2 is `SysAllocString` and 7 is `SysStringLen`.
    let address = match name {
        "SysAllocString" | "#2" => SysAllocString as *const (),
        "SysReAllocString" | "#3" => SysReAllocString as *const (),
        "SysAllocStringLen" | "#4" => SysAllocStringLen as *const (),
        "SysReAllocStringLen" | "#5" => SysReAllocStringLen as *const (),
        "SysFreeString" | "#6" => SysFreeString as *const (),
        "SysStringLen" | "#7" => SysStringLen as *const (),
        "SysStringByteLen" => SysStringByteLen as *const (),
        _ => return None,
    };
    Some(address as usize)
}

/// The bytes of a string, for the tests to look at.
#[cfg(test)]
fn bytes_of(string: *const u16) -> &'static [u16] {
    if string.is_null() {
        return &[];
    }
    let len = unsafe { SysStringLen(string) } as usize;
    // SAFETY: the characters of a string this module made.
    unsafe { std::slice::from_raw_parts(string, len) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
    }

    /// A string is the characters it was given, measured in characters and in
    /// bytes.
    #[test]
    fn a_string_keeps_its_characters_and_its_length() {
        let text = wide("h\u{e9}llo");
        let string = SysAllocString(text.as_ptr());
        assert!(!string.is_null());
        assert_eq!(SysStringLen(string), 5);
        assert_eq!(SysStringByteLen(string), 10);
        assert_eq!(bytes_of(string), &text[..5]);
        // The terminator is there and is not part of the length.
        // SAFETY: one character past the string's own length.
        assert_eq!(unsafe { *string.add(5) }, 0);
        SysFreeString(string);

        // No string in, no string out, and nothing to free.
        assert!(SysAllocString(std::ptr::null()).is_null());
        assert_eq!(SysStringLen(std::ptr::null()), 0);
        SysFreeString(std::ptr::null_mut());
    }

    /// The length is the caller's, so a terminator inside the characters is a
    /// character: which is the whole point of a `BSTR` having a length.
    #[test]
    fn a_length_inside_the_characters_is_a_character() {
        let text = [u16::from(b'a'), 0, u16::from(b'b')];
        let string = SysAllocStringLen(text.as_ptr(), 3);
        assert_eq!(SysStringLen(string), 3);
        assert_eq!(bytes_of(string), &text);
        SysFreeString(string);

        // A length with no buffer is a string of zeros, of that length.
        let empty = SysAllocStringLen(std::ptr::null(), 4);
        assert_eq!(SysStringLen(empty), 4);
        assert_eq!(bytes_of(empty), &[0, 0, 0, 0]);
        SysFreeString(empty);
        // And a length of nothing is a string that is not null.
        let none = SysAllocStringLen(std::ptr::null(), 0);
        assert!(!none.is_null());
        assert_eq!(SysStringLen(none), 0);
        SysFreeString(none);
    }

    /// Reallocating replaces the characters and frees what was there.
    #[test]
    fn reallocating_replaces_the_characters() {
        let mut string = SysAllocString(wide("one").as_ptr());
        let two = wide("second");
        assert_eq!(SysReAllocString(&mut string, two.as_ptr()), 1);
        assert_eq!(bytes_of(string), &two[..6]);

        let len = SysReAllocStringLen(&mut string, two.as_ptr(), 3);
        assert_eq!(len, 1);
        assert_eq!(bytes_of(string), &two[..3]);

        // A null text or a null slot is the caller's mistake.
        assert_eq!(SysReAllocString(&mut string, std::ptr::null()), 0);
        assert_eq!(SysReAllocString(std::ptr::null_mut(), two.as_ptr()), 0);
        assert_eq!(SysReAllocStringLen(std::ptr::null_mut(), two.as_ptr(), 1), 0);
        SysFreeString(string);
    }

    #[test]
    fn the_calls_are_answered_by_name_and_by_ordinal() {
        for (name, expected) in [
            ("SysAllocString", "#2"),
            ("SysReAllocString", "#3"),
            ("SysAllocStringLen", "#4"),
            ("SysReAllocStringLen", "#5"),
            ("SysFreeString", "#6"),
            ("SysStringLen", "#7"),
        ] {
            let by_name = lookup("oleaut32.dll", name).expect(name);
            let by_ordinal = lookup("oleaut32.dll", expected).expect(expected);
            assert_eq!(by_name, by_ordinal, "{name} and {expected} differ");
            // And through the loader's own table.
            assert_eq!(
                super::super::lookup("OLEAUT32.dll", expected),
                Some(by_ordinal),
                "{expected} is not answered by the loader's table"
            );
        }
    }
}
