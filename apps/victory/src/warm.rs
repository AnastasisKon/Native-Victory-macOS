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
                        let one = victory_shader::emit::translate_with(&container, base).is_ok();
                        let two = victory_shader::emit::translate_with(&container, switched).is_ok();
                        let k = outcome(one, two);
                        if k == LOST || k == RECOVERED {
                            let name = file.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
                            tracing::info!(container = %name, lost = k == LOST, "a switch changes this container");
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
    Ok(())
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
