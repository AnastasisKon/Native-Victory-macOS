//! The native host's warm-up (`victory translate --cache` on a Mac): the
//! containers earlier runs kept (`victory_gpu::native::shaders::keep`) are
//! translated into the cache a run reads (`<cache>/native-shaders`), by the
//! run's own code, with no title running. It is for the first run after the
//! translator changed, whose cache is empty.
//!
//! It goes from what runs saw, not from the game's files: the title hands the
//! native host pieces and the container is put together there, so a container
//! found in a file has another cache key. Only the plain translation is made
//! (`native::shaders::translate`, what a run's prewarm asks for): the
//! translations a run asks for by how a shader is drawn (rectangle lists,
//! extra parameters, thread counts) and the Metal libraries and pipelines are
//! still the run's.
//!
//! [`switch_delta`] is a measurement rather than a warm-up: every kept
//! container translated twice, once as the defaults do and once with the paired
//! form and the strict `vccz` refusal on (`VICTORY_PAIRED_WAVES`,
//! `VICTORY_STRICT_VCCZ_BRANCH`), counted. It says what those switches would
//! recover in a run, and what they would lose, without running the title twice
//! and without the on-disk cache (whose keys part by the switches anyway).

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use victory_gpu::native::shaders;

pub fn native(jobs: Option<usize>) -> anyhow::Result<()> {
    let dir = victory_core::Config::load()?.cache_subdir("native-shaders")?;
    let Some(cache) = victory_gpu::shader_cache::use_native_cache(dir.clone()) else {
        anyhow::bail!("no native shader cache in {}", dir.display());
    };
    let files = shaders::kept();
    anyhow::ensure!(
        !files.is_empty(),
        "no containers kept in {} yet: run the title once, then warm up after the translator changes",
        dir.join("containers").display()
    );
    let threads =
        jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let started = Instant::now();
    let [cached, translated, refused, damaged] = translate(&files, threads, true);
    println!("cache        {}", cache.display());
    println!(
        "containers   {} in {:.1} s: {cached} already cached, {translated} translated, {refused} refused, {damaged} damaged",
        files.len(),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// What the translation switches would do to a run's shaders: every container earlier runs kept,
/// translated twice - as the defaults do, and with the paired form and the strict `vccz` refusal on
/// - and counted. `recovered` is work the switches would bring back, `lost` work they would refuse
/// that translates today; the second is the risk a default flip carries.
pub fn switch_delta(jobs: Option<usize>) -> anyhow::Result<()> {
    let dir = victory_core::Config::load()?.cache_subdir("native-shaders")?;
    let files = shaders::kept();
    anyhow::ensure!(
        !files.is_empty(),
        "no containers kept in {} yet: run the title once, then compare the switches",
        dir.join("containers").display()
    );
    let base = victory_shader::emit::Options {
        paired_waves: false,
        strict_vccz_branch: false,
        ..shaders::options()
    };
    let switched = victory_shader::emit::Options {
        paired_waves: true,
        strict_vccz_branch: true,
        ..base
    };
    let counts = [const { AtomicUsize::new(0) }; 4];
    // The containers a switch changes, with what the translation that refused them said.
    let changed = std::sync::Mutex::new(Vec::new());
    // What the containers refused both ways were refused for, as a run sees it today: the
    // histogram that says which refusal the next translation fix should take (top 40 kept).
    let refused_why = std::sync::Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    let threads =
        jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let started = Instant::now();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads.clamp(1, files.len().max(1)))
            .map(|_| {
                s.spawn(|| {
                    while let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let Some(container) = shaders::read_kept(file) else { continue };
                        let one = victory_shader::emit::translate_with(&container, base);
                        let two = victory_shader::emit::translate_with(&container, switched);
                        let k = outcome(one.is_ok(), two.is_ok());
                        // As a run sees it today, for the histogram of what is still refused.
                        let default_why = one.err().map_or_else(String::new, |e| e.to_string());
                        let switched_why = two.err().map_or_else(String::new, |e| e.to_string());
                        if k == 3 {
                            let why = reason_key(&default_why);
                            let mut hist = refused_why.lock().unwrap_or_else(|e| e.into_inner());
                            let at = hist.iter().position(|(w, _)| *w == why);
                            match at {
                                Some(i) => hist[i].1 += 1,
                                None if hist.len() < 40 => hist.push((why, 1)),
                                None => {}
                            }
                        }
                        if k == LOST || k == RECOVERED {
                            let name = file
                                .file_name()
                                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                            // The refusal of the mode that refused it: the switch's own for a lost
                            // container, the defaults' for a recovered one.
                            let why = if k == LOST { switched_why } else { default_why };
                            tracing::info!(container = %name, lost = k == LOST, %why, "a switch changes this container");
                            let mut changed = changed.lock().unwrap_or_else(|e| e.into_inner());
                            if changed.len() < 40 {
                                changed.push((k, name, why));
                            }
                        }
                        counts[k].fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        let mut shown = Instant::now();
        while !workers.iter().all(|w| w.is_finished()) {
            std::thread::sleep(Duration::from_millis(100));
            if shown.elapsed() >= Duration::from_secs(10) {
                shown = Instant::now();
                let done: usize = counts.iter().map(|c| c.load(Ordering::Relaxed)).sum();
                eprintln!("{done} of {} containers, {:.0} s", files.len(), started.elapsed().as_secs_f64());
            }
        }
    });
    let [both, lost, recovered, refused] = counts.map(AtomicUsize::into_inner);
    println!("containers   {} in {:.1} s", both + lost + recovered + refused, started.elapsed().as_secs_f64());
    println!("  translated both ways   {both}");
    println!("  recovered by switches  {recovered} (refused today, translated with them)");
    println!("  lost by switches       {lost} (translated today, refused with them)");
    println!("  refused both ways      {refused}");
    println!("switches     paired_waves and strict_vccz_branch (VICTORY_PAIRED_WAVES, VICTORY_STRICT_VCCZ_BRANCH)");
    // Named, not only counted: a lost container is the risk a default flip carries, and the reason
    // says which refusal to look at. The first 40 of the run, as `warn_drop_pipe` does for drops.
    let mut hist = refused_why.into_inner().unwrap_or_else(|e| e.into_inner());
    hist.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if !hist.is_empty() {
        println!("refused both ways, by reason:");
        for (why, n) in hist.iter().take(10) {
            println!("  {n:6}  {why}");
        }
        if hist.len() > 10 {
            println!("  ... {} more reasons", hist.len() - 10);
        }
    }
    let changed = changed.into_inner().unwrap_or_else(|e| e.into_inner());
    for (k, label) in [(LOST, "lost"), (RECOVERED, "recovered")] {
        let named: Vec<_> = changed.iter().filter(|(o, ..)| *o == k).collect();
        if named.is_empty() {
            continue;
        }
        println!("{label}:");
        for (_, name, why) in named.iter().take(10) {
            println!("  {name}  {why}");
        }
        if named.len() > 10 {
            println!("  ... {} more", named.len() - 10);
        }
    }
    Ok(())
}

/// A refusal with its offsets blanked, so one reason counts as one line: "GFX10 code at 0x120:
/// a lane-mask join ..." and "at 0x4f8: ..." are the same defect.
fn reason_key(why: &str) -> String {
    let mut out = String::with_capacity(why.len());
    let mut rest = why;
    while let Some(at) = rest.find("0x") {
        let (before, after) = rest.split_at(at);
        out.push_str(before);
        let tail = &after[2..];
        let len = tail.chars().take_while(|c| c.is_ascii_hexdigit()).count();
        if len == 0 {
            out.push_str("0x");
            rest = tail;
        } else {
            out.push_str("0xH");
            rest = &tail[len..];
        }
    }
    out.push_str(rest);
    out
}

/// Counted outcomes of [`switch_delta`]: translated both ways, lost, recovered, refused both ways.
const LOST: usize = 1;
const RECOVERED: usize = 2;

/// Which of [`LOST`], [`RECOVERED`], both-translated (0) or refused-both-ways (3) `one`, the default
/// translation, and `two`, the switched one, are.
fn outcome(one: bool, two: bool) -> usize {
    match (one, two) {
        (true, true) => 0,
        (true, false) => LOST,
        (false, true) => RECOVERED,
        (false, false) => 3,
    }
}

/// Translates the kept containers in `files` on `threads` workers. Returns
/// how many the cache held already, were translated now, were refused now, and
/// did not read back.
fn translate(files: &[PathBuf], threads: usize, progress: bool) -> [usize; 4] {
    let counts = [const { AtomicUsize::new(0) }; 4];
    let next = AtomicUsize::new(0);
    let started = Instant::now();
    std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads.clamp(1, files.len().max(1)))
            .map(|_| {
                s.spawn(|| {
                    while let Some(file) = files.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let k = match shaders::read_kept(file) {
                            None => 3,
                            Some(container) if shaders::cached(&container) => 0,
                            Some(container) => 1 + usize::from(shaders::translate(&container).is_err()),
                        };
                        counts[k].fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();
        // A line every 10 s: a cold cache takes minutes.
        let mut shown = Instant::now();
        while progress && !workers.iter().all(|w| w.is_finished()) {
            std::thread::sleep(Duration::from_millis(100));
            if shown.elapsed() >= Duration::from_secs(10) {
                shown = Instant::now();
                let done: usize = counts.iter().map(|c| c.load(Ordering::Relaxed)).sum();
                eprintln!("{done} of {} containers, {:.0} s", files.len(), started.elapsed().as_secs_f64());
            }
        }
    });
    counts.map(AtomicUsize::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One refusal reason, whatever offsets the program happened to be at.
    #[test]
    fn a_refusal_reason_is_counted_without_its_offsets() {
        assert_eq!(
            reason_key("GFX10 code at 0x120: a lane-mask join is not supported"),
            reason_key("GFX10 code at 0x4f8: a lane-mask join is not supported")
        );
        assert_eq!(
            reason_key("GFX10 code at 0x120: a thing"),
            "GFX10 code at 0xH: a thing"
        );
        // An `0x` with no hex digits after it is left alone, and a reason without one is unchanged.
        assert_eq!(reason_key("0x and 0xZZ"), "0x and 0xZZ");
        assert_eq!(reason_key("no numbers here"), "no numbers here");
    }

    /// The four outcomes a switch can have for one container, and the counts they add up to.
    #[test]
    fn a_switch_recovers_or_loses_a_container() {
        assert_eq!(outcome(true, true), 0, "both translate");
        assert_eq!(outcome(true, false), LOST, "the switch refuses what translates today");
        assert_eq!(outcome(false, true), RECOVERED, "the switch translates what is refused today");
        assert_eq!(outcome(false, false), 3, "neither translates");
    }

    /// A container a run kept is, after the warm-up, where the run looks for
    /// its translation; a second warm-up finds it there, and a damaged file
    /// is counted, not translated.
    #[test]
    fn the_warm_up_translates_what_a_run_kept() {
        let dir = std::env::temp_dir().join(format!("victory-warm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        victory_gpu::shader_cache::use_native_cache(dir.clone()).expect("a cache folder");

        // A synthetic pixel program: v_mov_b32 v0, 1; an export; s_endpgm.
        let code: Vec<u8> = [0x7E00_0281u32, 0xF800_180F, 0x0000_0000, 0xBF81_0000]
            .iter()
            .flat_map(|w| w.to_le_bytes())
            .collect();
        let container =
            victory_shader::emit::graphics::container(victory_shader::HwStage::Ps, &code, &[0; 0x10])
                .unwrap();
        assert!(shaders::kept().is_empty());
        shaders::keep(&container);
        std::fs::write(dir.join("containers").join("00000000000000aa"), b"cut short").unwrap();
        let files = shaders::kept();
        assert_eq!(files.len(), 2);

        assert!(!shaders::cached(&container));
        let [cached, translated, refused, damaged] = translate(&files, 2, false);
        assert_eq!((cached, translated + refused, damaged), (0, 1, 1));
        assert!(shaders::cached(&container), "where a run's `translate` looks");
        assert_eq!(translate(&files, 2, false), [1, 0, 0, 1]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
