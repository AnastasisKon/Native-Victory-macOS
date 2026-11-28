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
        let mut offset = 0usize;
        for term in terms.split('+') {
            let term = usize::from_str_radix(term.trim().trim_start_matches("0x"), 16).ok()?;
            offset = offset.checked_add(term)?;
        }
        return (gpr[index] as usize).checked_add(offset);
    }
    let at = usize::from_str_radix(entry.trim_start_matches("0x"), 16).ok()?;
    if at > 0x1000_0000 {
        Some(at)
    } else {
        base?.checked_add(at)
    }
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
