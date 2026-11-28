//! `VICTORY_PEEK` entries: what address an entry names.
//!
//! The switch prints a qword of the run at each entry, read from inside the exception dispatch that
//! reports a fault, so the entries are what a reader can write down from a log: an RVA of the
//! faulting image, an absolute address, or a register of the faulting frame plus an offset. The last
//! is what reads a field of the object a fault is about (`rsi+0x50`) without knowing where the heap
//! put it — and it is the reason the parser is here, outside the x86_64-only dispatch, where its
//! tests run on any host.

/// The index of a general register in a `CONTEXT` by name (`rsi`, `rax`, `r8`), case ignored;
/// `None` for anything else (`rip` and the flags are not general registers).
fn register_index(name: &str) -> Option<usize> {
    const NAMES: [&str; 16] = [
        "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12",
        "r13", "r14", "r15",
    ];
    NAMES.iter().position(|n| n.eq_ignore_ascii_case(name.trim()))
}

/// The address one `VICTORY_PEEK` entry names, or `None` when it names nothing: `reg+offset` in the
/// faulting frame's `gpr` (offset hex, `0x` optional), else hex — an RVA of the faulting image,
/// counted from `base`, or an absolute address when it is past every image. An entry that names
/// nothing is skipped rather than read: a fault taken while reading it would be a second one inside
/// the dispatch that reports the first.
// Asked by the native host's (x86_64) dispatcher only.
#[cfg_attr(not(all(target_os = "macos", target_arch = "x86_64")), allow(dead_code))]
pub(crate) fn address(entry: &str, gpr: &[u64; 16], base: Option<usize>) -> Option<usize> {
    let entry = entry.trim();
    if let Some((reg, terms)) = entry.split_once('+') {
        let index = register_index(reg)?;
        // Terms are summed, so `rsi+0x10020+0x38` names a field two steps into the object a writer
        // keeps: an entry the single-term form accepted as nothing and skipped without a word.
        return (gpr[index] as usize).checked_add(sum_offsets(terms)?);
    }
    let at = usize::from_str_radix(entry.trim_start_matches("0x"), 16).ok()?;
    if at > 0x1000_0000 {
        Some(at)
    } else {
        base?.checked_add(at)
    }
}

/// What an entry names in a frame, read through `read` — which is the dispatch's checked read, so
/// an address written down by hand that names nothing is reported rather than faulted on.
#[cfg_attr(not(all(target_os = "macos", target_arch = "x86_64")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Read {
    Value(usize),
    /// The entry parses, and the address it names is not mapped.
    Unmapped,
    /// The entry is not one this parser knows.
    NoEntry,
}

/// The qword an entry names, following pointers as it goes: `rsi+0x50` reads a field of the frame,
/// `[rsi+0x10020]+0x38` reads the writer pointer and then a field of what it names, and the
/// brackets nest, so `[[rsi+0x10020]+0x40]+0x60` reaches a field of an object two steps out. Each
/// level that names nothing ends the walk rather than faulting in it.
#[cfg_attr(not(all(target_os = "macos", target_arch = "x86_64")), allow(dead_code))]
pub(crate) fn read_entry(
    entry: &str,
    gpr: &[u64; 16],
    base: Option<usize>,
    read: impl Fn(usize) -> Option<usize> + Copy,
) -> Read {
    let entry = entry.trim();
    if let Some(rest) = entry.strip_prefix('[') {
        // The bracket that closes this one, counting the brackets inside it: `[[a]+8]+0x10` is a
        // field of what the pointer `[a]+8` names, not an entry called `[a`.
        let Some(end) = closing_bracket(rest) else {
            return Read::NoEntry;
        };
        let (inner, after) = (&rest[..end], &rest[end + 1..]);
        let Some(offset) = sum_offsets(after) else {
            return Read::NoEntry;
        };
        let at = match read_entry(inner, gpr, base, read) {
            Read::Value(at) => at,
            other => return other,
        };
        return match at.checked_add(offset) {
            Some(at) => match read(at) {
                Some(value) => Read::Value(value),
                None => Read::Unmapped,
            },
            None => Read::Unmapped,
        };
    }
    match address(entry, gpr, base) {
        Some(at) => match read(at) {
            Some(value) => Read::Value(value),
            None => Read::Unmapped,
        },
        None => Read::NoEntry,
    }
}

/// The byte index of the `]` that closes the `[` whose contents are `rest`, counting the brackets
/// inside it; `None` when nothing closes it.
#[cfg_attr(not(all(target_os = "macos", target_arch = "x86_64")), allow(dead_code))]
pub(crate) fn closing_bracket(rest: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (at, ch) in rest.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

/// The sum of a `+`-separated hex offset list after a bracketed entry (`0x` optional; empty is 0,
/// and a leading `+` is allowed). A term that is no hex, or a sum past the address space, names
/// nothing.
#[cfg_attr(not(all(target_os = "macos", target_arch = "x86_64")), allow(dead_code))]
pub(crate) fn sum_offsets(spec: &str) -> Option<usize> {
    let spec = spec.trim();
    let spec = spec.strip_prefix('+').unwrap_or(spec).trim();
    let mut sum = 0usize;
    if spec.is_empty() {
        return Some(0);
    }
    for term in spec.split('+') {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        sum = sum.checked_add(usize::from_str_radix(term.trim_start_matches("0x"), 16).ok()?)?;
    }
    Some(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An entry names a register plus an offset, an RVA of the faulting image, or an absolute
    /// address; one that names nothing, and an RVA with no image to count from, name nothing.
    #[test]
    fn peek_entries_name_a_register_offset_or_an_address() {
        let mut gpr = [0u64; 16];
        gpr[6] = 0x7f_0000_0000; // rsi
        assert_eq!(
            address("rsi+0x50", &gpr, Some(0x140000000)),
            Some(0x7f_0000_0050)
        );
        // Spacing and case do not matter, and a register entry needs no image.
        assert_eq!(address(" RSI + 50 ", &gpr, None), Some(0x7f_0000_0050));
        assert_eq!(address("1330a44", &gpr, Some(0x140000000)), Some(0x141330a44));
        assert_eq!(address("0x1330a44", &gpr, Some(0x140000000)), Some(0x141330a44));
        assert_eq!(address("1330a44", &gpr, None), None);
        // Past every image: absolute.
        assert_eq!(address("7ff800001000", &gpr, None), Some(0x7ff8_0000_1000));
        // `rip` is not a general register, an offset that overflows wraps into nothing, and an
        // entry that is no hex names nothing.
        assert_eq!(address("rip+10", &gpr, None), None);
        gpr[6] = u64::MAX;
        assert_eq!(address("rsi+1", &gpr, None), None);
        assert_eq!(address("nonsense", &gpr, None), None);
    }

    /// An entry may follow pointers before reading, and the brackets nest: each level reads the
    /// qword the entry inside names and adds what comes after the bracket. A fake memory stands in
    /// for the title's, so the walk is checked without a run.
    #[test]
    fn peek_entries_follow_pointers_before_reading() {
        // A tiny memory: 0x1000 holds 0x2000, 0x2000 holds 0x1234, 0x2008 holds 0x3000,
        // 0x3010 holds 0xbeef.
        let mut mem = [0usize; 0x1000];
        mem[0x1000 / 8] = 0x2000;
        mem[0x2000 / 8] = 0x1234;
        mem[0x2008 / 8] = 0x3000;
        mem[0x3010 / 8] = 0xbeef;
        let read = |at: usize| mem.get(at / 8).copied().filter(|_| at.is_multiple_of(8));
        let gpr = [0u64; 16];
        // The bare hex entries are RVAs, so they are counted from an image at 0.
        assert_eq!(read_entry("[1000]+8", &gpr, Some(0), read), Read::Value(0x3000));
        assert_eq!(
            read_entry("[[1000]+8]+0x10", &gpr, Some(0), read),
            Read::Value(0xbeef)
        );
        assert_eq!(read_entry("[1000]", &gpr, Some(0), read), Read::Value(0x1234));
        // A bracket that never closes, and something after it that is no offset, name nothing.
        assert_eq!(read_entry("[1000", &gpr, Some(0), read), Read::NoEntry);
        assert_eq!(read_entry("[1000]x", &gpr, Some(0), read), Read::NoEntry);
        // An address the memory does not cover is unmapped, and an entry that is no address is none.
        assert_eq!(read_entry("[1000]+0x8000", &gpr, Some(0), read), Read::Unmapped);
        assert_eq!(read_entry("nonsense", &gpr, Some(0), read), Read::NoEntry);
    }

    /// Offsets add up: an entry names a field of the object a register points into, and two terms
    /// name a field of a field without a second run to read the first address.
    #[test]
    fn peek_entries_add_their_offsets() {
        let mut gpr = [0u64; 16];
        gpr[6] = 0x7f_0000_0000; // rsi
        assert_eq!(
            address("rsi+0x10020+0x38", &gpr, None),
            Some(0x7f_0001_0058)
        );
        assert_eq!(address("rsi+10020+38", &gpr, None), Some(0x7f_0001_0058));
        // A term that is no hex, and one that carries the sum past the address space, name nothing.
        assert_eq!(address("rsi+0x10020+nope", &gpr, None), None);
        gpr[6] = u64::MAX - 1;
        assert_eq!(address("rsi+1+1", &gpr, None), None);
    }
}
