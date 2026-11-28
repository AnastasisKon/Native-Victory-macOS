//! `VICTORY_DUMP_THREADS=<seconds>`: after that long, log what every title
//! thread is doing: in a host call (which import, from where in the title)
//! or in title code, with a call chain from the unwind tables. A debugging
//! aid for a title that stops making progress.

use std::time::Duration;

use super::bridge::traced_name;
use super::seh::context::{Context, RBP, RSP};
use super::seh::unwind;
use super::teb::{
    OFF_IN_HOST, OFF_IN_HOST_CTX, OFF_IN_HOST_RBP, OFF_IN_HOST_RET, OFF_STACK_BASE, OFF_STACK_LIMIT,
};

fn seconds() -> Option<u64> {
    static SECS: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *SECS.get_or_init(|| std::env::var("VICTORY_DUMP_THREADS").ok()?.parse().ok())
}

/// Whether the dump is asked for (imports then carry their names).
pub fn enabled() -> bool {
    seconds().is_some()
}

/// `VICTORY_WATCH_MEM=<hex address>`: every second, the qwords of the 4 KiB around the address
/// that changed since the last look are logged (data only; for finding what writes a flag).
pub fn start_mem_watch() {
    let Some(at) = std::env::var("VICTORY_WATCH_MEM")
        .ok()
        .and_then(|v| usize::from_str_radix(v.trim_start_matches("0x"), 16).ok())
    else {
        return;
    };
    std::thread::spawn(move || {
        let base = (at & !0xfff) as usize;
        let mut last: Vec<Option<usize>> = vec![None; 512];
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let mut lines = Vec::new();
            for (i, old) in last.iter_mut().enumerate() {
                let now = read_checked(base + i * 8);
                if *old != now && old.is_some() {
                    lines.push(format!("{:#x}: {:x?} -> {:x?}", base + i * 8, old, now));
                }
                *old = now;
            }
            if !lines.is_empty() {
                tracing::warn!("mem watch: {} changed: {}", lines.len(), lines[..lines.len().min(24)].join("; "));
            }
        }
    });
}

/// `VICTORY_HOLD_BYTE=<hex address>:<hex byte>`: keeps that byte of title
/// memory at the value, rewritten every 250 ms. Debugging aid, e.g. holding a
/// title watchdog's pause flag while slow storage stretches its init.
pub fn start_hold_byte() {
    let Some((at, value)) = std::env::var("VICTORY_HOLD_BYTE").ok().and_then(|v| {
        let (a, b) = v.split_once(':')?;
        Some((usize::from_str_radix(a.trim_start_matches("0x"), 16).ok()?, u8::from_str_radix(b, 16).ok()?))
    }) else {
        return;
    };
    tracing::warn!("holding byte {at:#x} at {value:#x}");
    std::thread::spawn(move || loop {
        // SAFETY: debugging aid; the address is title data the user named.
        unsafe { (at as *mut u8).write_volatile(value) };
        std::thread::sleep(Duration::from_millis(250));
    });
}

/// Starts the dumping thread if `VICTORY_DUMP_THREADS` is set.
pub fn start() {
    let Some(secs) = seconds() else { return };
    std::thread::spawn(move || {
        // 0: only at the title's first breakpoint or fatal fault (`report_at_fault`).
        if secs == 0 {
            return;
        }
        std::thread::sleep(Duration::from_secs(secs));
        report();
    });
}

/// How long a faulting thread waits for the dump it asked for.
const FAULT_WAIT: Duration = Duration::from_secs(60);

/// With `VICTORY_DUMP_THREADS=0`, the dump is taken once: when the title first hits a breakpoint
/// (its asserts, at the first chance) or an access violation that no handler took (its
/// fatal-error path writes to address 0). `unhandled` says the dispatch is past every handler.
pub fn report_at_fault(code: u32, unhandled: bool) {
    static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if seconds() != Some(0) || !claim(&TAKEN, code, unhandled) {
        return;
    }
    // On a thread of its own, as the timed dump is: the faulting thread may hold host locks the
    // report takes (title code called back from a host call), so it only waits, and not for
    // ever: a fatal fault stays fatal if the dump hangs.
    let (done, wait) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        report();
        let _ = done.send(());
    });
    let _ = wait.recv_timeout(FAULT_WAIT);
}

/// Whether this fault takes the one dump. An access violation at the first chance does not: the
/// title's own handlers take some and go on, and the dump is for the one nobody takes. Never
/// waits: a second faulting thread goes on while the first one's dump is written.
fn claim(taken: &std::sync::atomic::AtomicBool, code: u32, unhandled: bool) -> bool {
    let wanted = match code {
        0x8000_0003 => true,
        0xC000_0005 => unhandled,
        _ => false,
    };
    wanted && !taken.swap(true, std::sync::atomic::Ordering::Relaxed)
}

fn report() {
    {
        tracing::warn!(
            "clzero emulations so far: {}",
            super::seh::signal::EMULATED.load(std::sync::atomic::Ordering::Relaxed)
        );
        if let Some((at, n)) = std::env::var("VICTORY_DUMP_MEM").ok().and_then(|v| {
            let (a, n) = v.split_once(',')?;
            Some((usize::from_str_radix(a.trim_start_matches("0x"), 16).ok()?, n.parse::<usize>().ok()?))
        }) {
            let words: Vec<String> = (0..n)
                .map(|i| read_checked(at + 8 * i).map_or("-".into(), |x| format!("{x:x}")))
                .collect();
            tracing::warn!("memory at {at:#x}: {}", words.join(" "));
        }
        if let Ok(list) = std::env::var("VICTORY_FIND_QWORD") {
            // `@<hex>`: the qword at that address (an object's method table pointer, say).
            let values: Vec<u64> = list
                .split(',')
                .filter_map(|v| match v.strip_prefix('@') {
                    Some(at) => read_checked(usize::from_str_radix(at, 16).ok()?).map(|x| x as u64),
                    None => u64::from_str_radix(v.trim_start_matches("0x"), 16).ok(),
                })
                .collect();
            find_qwords(&values);
        }
        super::bridge::report_callbacks();
        for line in collect() {
            tracing::warn!("{line}");
        }
    }
}

/// `VICTORY_FIND_QWORD=<hex>,<hex>,...` (with `VICTORY_DUMP_THREADS`): every aligned qword of
/// resident readable memory equal to one of the values is logged with the 0x100 bytes from it
/// (for finding heap objects by their method table, data only). `@<hex>` stands for the qword
/// at that address.
fn find_qwords(values: &[u64]) {
    let mut at = 0x1_0000usize;
    let mut found = 0;
    let mut chunk = vec![0u64; 0x2_0000];
    while let Some(r) = victory_platform::mem::next_region(at) {
        // A region that grows while we look is seen again from where we stopped.
        let (start, end) = (r.base.max(at), r.base + r.len);
        at = end;
        if r.protect.0 & 1 == 0 || found >= 64 {
            continue;
        }
        let mut p = start;
        while p < end && found < 64 {
            let n = (end - p).min(chunk.len() * 8);
            // Pages never touched would take minutes to read in huge reservations.
            if resident(p, n) && read_into(p, &mut chunk[..n / 8]) {
                for (i, &w) in chunk[..n / 8].iter().enumerate() {
                    // Not our own buffer or locals.
                    let own = chunk.as_ptr_range();
                    let near_stack = (p + 8 * i).abs_diff(&raw const found as usize) < 0x1_0000;
                    if values.contains(&w)
                        && !own.contains(&((p + 8 * i) as *const u64))
                        && !near_stack
                    {
                        let hit = p + 8 * i;
                        let words: Vec<String> = (0..32)
                            .map(|k| {
                                read_checked(hit + 8 * k).map_or("-".into(), |x| format!("{x:x}"))
                            })
                            .collect();
                        tracing::warn!("qword {w:#x} at {hit:#x}: {}", words.join(" "));
                        found += 1;
                    }
                }
            }
            p += n;
        }
    }
    tracing::warn!("qwords {values:x?}: {found} found");
}

/// Whether any page of the range is in memory.
fn resident(at: usize, len: usize) -> bool {
    unsafe extern "C" {
        fn mincore(addr: *const std::ffi::c_void, len: usize, vec: *mut u8) -> i32;
    }
    let mut pages = vec![0u8; len.div_ceil(0x1000)];
    // SAFETY: `pages` has one byte per page of the range.
    unsafe {
        mincore(at as *const _, len, pages.as_mut_ptr()) == 0 && pages.iter().any(|&b| b & 1 != 0)
    }
}

/// Reads `out.len()` qwords from `at`, if the kernel can.
fn read_into(at: usize, out: &mut [u64]) -> bool {
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn mach_vm_read_overwrite(
            task: u32,
            address: u64,
            size: u64,
            data: u64,
            out: *mut u64,
        ) -> i32;
    }
    let mut got = 0u64;
    let size = (out.len() * 8) as u64;
    // SAFETY: `out` is that many writable bytes; the kernel checks the source.
    let r = unsafe {
        mach_vm_read_overwrite(
            mach_task_self_,
            at as u64,
            size,
            out.as_mut_ptr() as u64,
            &mut got,
        )
    };
    r == 0 && got == size
}

/// A qword at `at`, if the kernel says it can be read.
pub(crate) fn read_checked(at: usize) -> Option<usize> {
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn mach_vm_read_overwrite(task: u32, address: u64, size: u64, data: u64, out: *mut u64) -> i32;
    }
    let (mut v, mut got) = (0usize, 0u64);
    // SAFETY: `v` is 8 writable bytes; the kernel checks the source.
    let r = unsafe { mach_vm_read_overwrite(mach_task_self_, at as u64, 8, &raw mut v as u64, &mut got) };
    (r == 0 && got == 8).then_some(v)
}

fn read(teb: usize, off: usize) -> usize {
    // SAFETY: inside a live TEB block.
    unsafe { ((teb + off) as *const usize).read() }
}

/// The most stack words copied from a stopped thread (1 MiB; a chain that goes above is cut).
const STACK_WORDS: usize = 0x2_0000;

/// What the dump needs of a thread that only exists while it is stopped.
struct Snapshot<'a> {
    /// The bridge's record of a host call: the target (0 in title code), the title's return
    /// address, the stub's context word and the bridge's `rbp`.
    in_host: [usize; 4],
    /// The registers, when the thread is not in a host call and they could be read.
    ctx: Option<Context>,
    /// The stack from `start` up, as it was.
    start: usize,
    stack: &'a [u64],
}

impl Snapshot<'_> {
    /// The qword the stopped thread had at `at` on its stack.
    fn word(&self, at: usize) -> Option<usize> {
        let i = at.checked_sub(self.start)? / 8;
        (at.is_multiple_of(8) && i < self.stack.len()).then(|| self.stack[i] as usize)
    }
}

/// Stops the thread, copies its registers and stack into `raw` and lets it go on. The stopped
/// thread may hold any lock (the unwind tables', the allocator's, the log's), so nothing between
/// the suspend and the resume locks, allocates or logs: only reads and kernel calls.
fn snapshot(teb: usize, port: super::threadctl::ThreadPort, raw: &mut [u64]) -> Option<Snapshot<'_>> {
    if !port.suspend() {
        return None;
    }
    let (limit, base) = (read(teb, OFF_STACK_LIMIT), read(teb, OFF_STACK_BASE));
    let in_host = [OFF_IN_HOST, OFF_IN_HOST_RET, OFF_IN_HOST_CTX, OFF_IN_HOST_RBP].map(|off| read(teb, off));
    let mut ctx = Context::zeroed();
    let have = in_host[0] == 0 && port.context(&mut ctx);
    // In a host call: from below the bridge's frame (the registers and arguments it pushed).
    let sp = if in_host[0] != 0 { in_host[3].wrapping_sub(0x100) } else { ctx.gpr[RSP] as usize } & !7;
    let mut words = 0;
    if (limit..base).contains(&sp) {
        let n = ((base - sp) / 8).min(raw.len());
        if read_into(sp, &mut raw[..n]) {
            words = n;
        }
    }
    port.resume();
    Some(Snapshot { in_host, ctx: have.then_some(ctx), start: sp, stack: &raw[..words] })
}

/// The call chain from `ctx` over the copied stack. The unwinder reads memory at the addresses
/// the registers hold, so the copy is made to look like the stack it came from: every register
/// and stack word that points into the copied range (saved frame pointers, which the frames
/// above are found by) is moved by the distance to the copy. Return addresses point at images,
/// never into the stack, so the chain is the one the live stack would give.
fn chain_over(s: &Snapshot, mut ctx: Context) -> Vec<usize> {
    let mut copy = s.stack.to_vec();
    let (low, at) = (s.start as u64, copy.as_ptr() as usize);
    let high = low + 8 * copy.len() as u64;
    let moved = |v: &mut u64| {
        if (low..high).contains(v) {
            *v = (*v - low) + at as u64;
        }
    };
    copy.iter_mut().for_each(moved);
    ctx.gpr.iter_mut().for_each(moved);
    super::seh::backtrace(ctx, Some((at, at + 8 * copy.len())))
}

fn collect() -> Vec<String> {
    let me = super::threadctl::ThreadPort::current();
    let mut raw = vec![0u64; STACK_WORDS];
    let mut out = Vec::new();
    for (teb, port, id) in super::teb::live_threads() {
        if port == me {
            continue;
        }
        if let Some(s) = snapshot(teb, port, &mut raw) {
            out.push(describe(id, &s));
        }
    }
    out
}

/// One thread's line of the dump, from its snapshot (the thread runs again by now).
fn describe(id: u32, s: &Snapshot) -> String {
    let mut line = format!("thread {id}: ");
    let mut chain = Vec::new();
    let [target, ret, stub, rbp] = s.in_host;
    {
        if target != 0 {
            let name = traced_name(stub)
                .map_or(format!("host function {target:#x}"), |(dll, name, slot)| {
                    if name.is_empty() {
                        format!("{dll} method {slot:#x}")
                    } else {
                        format!("{dll}!{name}")
                    }
                });
            line += &format!("in {name}");
            // The bridge's frame: the title's rbp is at [rbp], the return
            // address at [rbp + 8], the title's rsp after it at rbp + 16.
            let mut ctx = Context::zeroed();
            ctx.rip = ret as u64;
            ctx.gpr[RSP] = (rbp + 16) as u64;
            ctx.gpr[RBP] = s.word(rbp).unwrap_or(0) as u64;
            chain = chain_over(s, ctx);
            // VICTORY_DUMP_REGS=1: the title's callee-saved registers at the call (the bridge
            // pushed them below its frame pointer) and the qwords they point at.
            if std::env::var_os("VICTORY_DUMP_REGS").is_some() {
                let regs: Vec<String> = ["rbx", "rdi", "rsi", "r12", "r13", "r14"]
                    .iter()
                    .enumerate()
                    .map(|(i, n)| {
                        let v = s.word(rbp - 8 * (i + 1)).unwrap_or(0);
                        // Not into the image: only data.
                        let words: Vec<String> = (0..4)
                            .filter(|_| !(0x1_4000_0000..0x1_4600_0000).contains(&v))
                            .map(|w| read_checked(v + 8 * w).map_or("-".into(), |x| format!("{x:x}")))
                            .collect();
                        format!("{n}={v:#x} [{}]", words.join(" "))
                    })
                    .collect();
                let arg = |off: usize| s.word(rbp - off).unwrap_or(0);
                line += &format!(" args {:#x} {:#x} regs {}", arg(0x100), arg(0xF8), regs.join(" "));
            }
            // VICTORY_DUMP_STACK=1: the title's stack words above the call, and 4 qwords behind
            // each that points at data (not at the image's code).
            if std::env::var_os("VICTORY_DUMP_STACK").is_some() && chain.len() > 2 && !read_all_bases(&chain) {
                let mut out = Vec::new();
                for i in 0..96 {
                    let at = rbp + 16 + 8 * i;
                    let Some(v) = s.word(at) else { break };
                    let data = (0x1_4600_0000..0x7fff_ffff_0000).contains(&v) && v & 7 == 0;
                    let words: Vec<String> = if data {
                        (0..4).map(|w| read_checked(v + 8 * w).map_or("-".into(), |x| format!("{x:x}"))).collect()
                    } else {
                        Vec::new()
                    };
                    out.push(format!("+{:#x}: {v:x} [{}]", 8 * i, words.join(" ")));
                }
                line += &format!(" stack {}", out.join(" | "));
            }
        } else if let Some(ctx) = s.ctx {
            if unwind::image_base_of(ctx.rip as usize).is_some() {
                line += "in title code";
                chain = chain_over(s, ctx);
            } else {
                line += &format!("in host code at {:#x}", ctx.rip);
            }
        }
        let base = chain.first().and_then(|&pc| unwind::image_base_of(pc)).unwrap_or(0);
        let offsets: Vec<usize> = chain.iter().map(|&pc| pc.wrapping_sub(base)).collect();
        format!("{line}, title image+ {offsets:x?}")
    }
}

/// The calling thread's title call chain (image offsets) while it is in a host call, for
/// diagnostics such as `VICTORY_TRACE_HANDLES`.
pub fn caller_chain() -> Vec<usize> {
    let teb = super::teb::current() as usize;
    if teb == 0 || read(teb, OFF_IN_HOST) == 0 {
        return Vec::new();
    }
    let rbp = read(teb, OFF_IN_HOST_RBP);
    let mut ctx = Context::zeroed();
    ctx.rip = read(teb, OFF_IN_HOST_RET) as u64;
    ctx.gpr[RSP] = (rbp + 16) as u64;
    // SAFETY: the bridge's own frame on this thread's stack.
    ctx.gpr[RBP] = unsafe { (rbp as *const u64).read() };
    let bounds = Some((read(teb, OFF_STACK_LIMIT), read(teb, OFF_STACK_BASE)));
    let chain = super::seh::backtrace(ctx, bounds);
    let base = chain.first().and_then(|&pc| unwind::image_base_of(pc)).unwrap_or(0);
    chain.iter().map(|&pc| pc.wrapping_sub(base)).collect()
}

/// Whether the chain is not the main wait loop's (the stack dump is for one thread).
fn read_all_bases(chain: &[usize]) -> bool {
    let base = chain.first().and_then(|&pc| unwind::image_base_of(pc)).unwrap_or(0);
    !chain.iter().any(|&pc| pc.wrapping_sub(base) == 0x1d83996)
}

/// `VICTORY_TRAP_WRITES=<hex page>,<hex watched address>,...`: the page is kept read-only and each
/// title write to it faults here; a write to a watched address is logged with the writer's title
/// call chain. The page is opened for the faulting write and closed again a moment later by a
/// thread, so most writes are not seen (statistics only: look for rare writers).
///
/// A page and its addresses are one group, and `;` separates groups, so a hunt that needs two
/// pages (an object and the table it looks in, say) is one run rather than one run per page. A
/// group may separate the page from its addresses with `:` or `,`, as `,` is what the switch took
/// before groups existed: `<page>:<addr>,<addr>;<page>[,<addr>...]`.
pub fn write_trap(c: &Context, r: &super::seh::context::ExceptionRecord) -> bool {
    let Some(groups) = trap_spec() else { return false };
    if r.code != 0xc000_0005 || r.information[0] != 1 {
        return false;
    }
    let at = r.information[1] as usize;
    let Some((page, watched)) = groups.iter().find(|(page, _)| *page == at & !0xfff) else {
        return false;
    };
    // A watched address always names its writer; a group with no addresses hunts for one, so every
    // write to the page is named — throttled, since a page the title writes in a loop would
    // otherwise fill the log with the one writer.
    if watched.is_empty() || watched.contains(&(at & !7)) {
        if watched.contains(&(at & !7)) {
            let base = unwind::image_base_of(c.rip as usize).unwrap_or(0);
            let chain = super::seh::backtrace(*c, None);
            let chain: Vec<usize> = chain.iter().map(|&pc| pc.wrapping_sub(base)).collect();
            tracing::warn!("write to {at:#x} from image+ {:x?}", chain);
        } else if note_unwatched(unwatched_key(at)) {
            let base = unwind::image_base_of(c.rip as usize).unwrap_or(0);
            let chain = super::seh::backtrace(*c, None);
            let chain: Vec<usize> = chain.iter().map(|&pc| pc.wrapping_sub(base)).collect();
            tracing::warn!(
                "write to {at:#x} (no address named for this page) from image+ {:x?}",
                chain
            );
        }
    }
    unsafe extern "C" {
        fn mprotect(addr: *mut std::ffi::c_void, len: usize, prot: i32) -> i32;
    }
    // SAFETY: the page is the image's own data.
    unsafe { mprotect(*page as *mut _, 0x1000, 3) };
    true
}

/// The word a write to a trapped page with no watched addresses is counted under: the address
/// itself, so one log line per place the title writes, not one per write.
fn unwatched_key(at: usize) -> usize {
    at & !7
}

/// Whether this place should be logged: the first few writes there, then every five-hundredth, so a
/// hunt over a page the title writes constantly still names every writer it has.
fn note_unwatched(key: usize) -> bool {
    const NAMED: u64 = 4;
    const BETWEEN: u64 = 512;
    static SEEN: std::sync::Mutex<Vec<(usize, u64)>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    match seen.iter_mut().find(|(of, _)| *of == key) {
        Some((_, n)) => {
            *n += 1;
            *n <= NAMED || n.is_multiple_of(BETWEEN)
        }
        None => {
            seen.push((key, 1));
            true
        }
    }
}

/// The trapped pages and the addresses whose writes are logged, from `VICTORY_TRAP_WRITES`./// The trapped pages and the addresses whose writes are logged, from `VICTORY_TRAP_WRITES`.
fn trap_spec() -> Option<&'static Vec<(usize, Vec<usize>)>> {
    use std::sync::OnceLock;
    static SPEC: OnceLock<Option<Vec<(usize, Vec<usize>)>>> = OnceLock::new();
    SPEC.get_or_init(|| std::env::var("VICTORY_TRAP_WRITES").ok().map(|v| parse_trap_spec(&v)))
        .as_ref()
}

/// `[<page>[:,]<addr>,...][;<page>...]`, each page rounded down to its page boundary. An entry that
/// does not parse is left out, and a group with no page is dropped: this is a debugging aid.
fn parse_trap_spec(value: &str) -> Vec<(usize, Vec<usize>)> {
    fn hex(text: &str) -> Option<usize> {
        usize::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
    }
    let mut groups = Vec::new();
    for group in value.split(';') {
        let mut parts = group.split([',', ':']);
        let Some(page) = parts.next().and_then(hex) else {
            continue;
        };
        groups.push((page & !0xfff, parts.filter_map(hex).collect()));
    }
    groups
}

/// Starts the thread that closes the trapped page again.
pub fn start_write_trap() {
    let Some(groups) = trap_spec() else { return };
    let pages: Vec<usize> = groups.iter().map(|(page, _)| *page).collect();
    if pages.is_empty() {
        return;
    }
    unsafe extern "C" {
        fn mprotect(addr: *mut std::ffi::c_void, len: usize, prot: i32) -> i32;
    }
    // Armed here, before the title's first thread exists: a page written once early was missed
    // while the arming waited for this thread to be scheduled, which read as "nothing writes it".
    let mut late: Vec<usize> = Vec::new();
    for page in &pages {
        // SAFETY: the page the spec named, which the caller is watching.
        if unsafe { mprotect(*page as *mut _, 0x1000, 1) } == 0 {
            tracing::info!(
                page = format_args!("{page:#x}"),
                watched = groups
                    .iter()
                    .find(|(p, _)| p == page)
                    .map_or(0, |(_, w)| w.len()),
                "write trap: page armed read-only (VICTORY_TRAP_WRITES)"
            );
        } else {
            tracing::warn!(
                page = format_args!("{page:#x}"),
                "write trap: the page is not mapped yet; the thread arms it when it is"
            );
            late.push(*page);
        }
    }
    // While a page is still unarmed the thread asks again every twenty microseconds: an object
    // created by one block copy writes all its fields in that copy, and a page armed a third of a
    // millisecond after it appears misses exactly that — which reads as "nothing fills the field".
    std::thread::spawn(move || {
        loop {
            for page in &pages {
                // SAFETY: the page the spec named, which the caller is watching.
                let ok = unsafe { mprotect(*page as *mut _, 0x1000, 1) } == 0;
                if ok && late.contains(page) {
                    tracing::info!(
                        page = format_args!("{page:#x}"),
                        "write trap: page armed read-only, late (VICTORY_TRAP_WRITES)"
                    );
                }
            }
            late.retain(|page| {
                // SAFETY: as above; the check only asks the mapping to become read-only again.
                let refused = unsafe { mprotect(*page as *mut _, 0x1000, 1) } != 0;
                refused
            });
            let wait = if late.is_empty() { 300 } else { 20 };
            std::thread::sleep(Duration::from_micros(wait));
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::channel;

    use super::*;

    /// A page hunted without a named address logs each place a few times and then rarely, so a hot
    /// page still names every writer without filling the log with one of them.
    #[test]
    fn an_unwatched_page_names_each_place_it_is_written() {
        let key = 0xdead_0000; // this test's own, so the shared counter is not disturbed
        let logged: Vec<u64> = (1..=6).map(|_| u64::from(note_unwatched(key))).collect();
        assert_eq!(logged, vec![1, 1, 1, 1, 0, 0]);
        // Another place on the same page is counted on its own.
        assert!(note_unwatched(key + 8));
    }

    /// The write trap takes one group of a page and its addresses, or several separated by `;`,
    /// and keeps the form it had before groups existed (`<page>,<addr>,...`).
    #[test]
    fn a_write_trap_spec_takes_several_pages() {
        assert_eq!(
            parse_trap_spec("181796000,181796a28,181796a30"),
            vec![(0x1_8179_6000, vec![0x1_8179_6a28, 0x1_8179_6a30])]
        );
        // ':' separates a page from its addresses, so the old form is not ambiguous either way.
        assert_eq!(
            parse_trap_spec("0x181796000:0x181796a28;0x1817c2000:0x1817c2a50"),
            vec![
                (0x1_8179_6000, vec![0x1_8179_6a28]),
                (0x1_817c_2000, vec![0x1_817c_2a50]),
            ]
        );
        // A page is rounded down, an entry that does not parse costs only itself, and a group with
        // no page is dropped.
        assert_eq!(parse_trap_spec("181796a28"), vec![(0x1_8179_6000, vec![])]);
        assert_eq!(
            parse_trap_spec("181796000,zz,181796a28"),
            vec![(0x1_8179_6000, vec![0x1_8179_6a28])]
        );
        assert!(parse_trap_spec(";zz").is_empty());
    }

    #[test]
    fn a_handled_access_violation_leaves_the_dump_for_the_fatal_one() {
        let taken = AtomicBool::new(false);
        // First chance: the title's handlers may still take it. Other codes never dump.
        assert!(!claim(&taken, 0xC000_0005, false));
        assert!(!claim(&taken, 0xC000_0005, false));
        assert!(!claim(&taken, 0xC000_001D, true));
        // Nobody took this one.
        assert!(claim(&taken, 0xC000_0005, true));
        // Once: not again for a later fault or breakpoint.
        assert!(!claim(&taken, 0xC000_0005, true));
        assert!(!claim(&taken, 0x8000_0003, false));
        // A breakpoint dumps at the first chance.
        assert!(claim(&AtomicBool::new(false), 0x8000_0003, false));
    }

    #[test]
    fn a_thread_holding_the_unwind_tables_is_snapshotted_and_let_go() {
        let (teb_tx, teb_rx) = channel();
        let (release_tx, release_rx) = channel::<()>();
        let holder = std::thread::spawn(move || {
            let teb = super::super::teb::current() as usize;
            let held = unwind::hold_tables();
            teb_tx.send(teb).unwrap();
            let _ = release_rx.recv();
            drop(held);
            // Stays live (and in the thread list) until the dump has its line.
            let _ = release_rx.recv();
        });
        let teb = teb_rx.recv().unwrap();
        let (snapped_tx, snapped_rx) = channel();
        let (line_tx, line_rx) = channel();
        std::thread::spawn(move || {
            let (_, port, id) =
                super::super::teb::live_threads().into_iter().find(|t| t.0 == teb).unwrap();
            let mut raw = vec![0u64; STACK_WORDS];
            let s = snapshot(teb, port, &mut raw).unwrap();
            snapped_tx.send((s.ctx.is_some(), s.stack.len())).unwrap();
            line_tx.send(describe(id, &s)).unwrap();
        });
        // The holder still has the lock: the snapshot must not have needed it.
        let (ctx, words) = snapped_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("the snapshot waited for a lock the stopped thread holds");
        assert!(ctx && words > 0);
        // Unwinding and formatting come after, once the holder runs again and lets go.
        assert!(line_rx.recv_timeout(Duration::from_millis(200)).is_err());
        release_tx.send(()).unwrap();
        let line = line_rx.recv_timeout(Duration::from_secs(20)).expect("the thread was not resumed");
        assert!(line.contains("in host code at"), "{line}");
        drop(release_tx);
        holder.join().unwrap();
    }

    #[test]
    fn the_chain_is_unwound_from_the_copied_stack() {
        let fake = unwind::tests::Fake::new();
        unwind::add_image(fake.base(), 0x1000, Some((0x200, 12)));
        // Two frames of the fake function (0x20 of locals, rbx, the return address), as they
        // were at an address that is not mapped here.
        let start = 0x7000_0000_1000usize;
        let mut stack = [0u64; 12];
        stack[4] = start as u64 + 0x40;
        stack[5] = fake.base() as u64 + 0x107;
        let s = Snapshot { in_host: [0; 4], ctx: None, start, stack: &stack };
        let mut ctx = Context::zeroed();
        ctx.rip = fake.base() as u64 + 0x105;
        ctx.gpr[RSP] = start as u64;
        assert_eq!(chain_over(&s, ctx), [fake.base() + 0x105, fake.base() + 0x107]);
        // The snapshot itself keeps the thread's own values.
        assert_eq!(s.word(start + 0x20), Some(start + 0x40));
        assert_eq!(s.word(start + 0x60), None);
        unwind::remove_image(fake.base());
    }
}
