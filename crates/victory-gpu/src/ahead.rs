//! Buffers of 64 KiB made ahead of the title's need for them.
//!
//! Every title buffer gets a copy of the host's, rounded up to 64 KiB, and with
//! `VICTORY_CARD_BUFFERS` a second one in the card's memory. Making one pair takes about
//! 0.8 ms (two committed resources, each waited for until its memory is there), and the title
//! asks for thousands in the frame a scene starts in: the first frame with a 3D scene of a new
//! game made 3,450 of them and stood for 2.8 s of its 4 s over them alone (run full1,
//! 2026-11-27, RTX 4070 SUPER + Ryzen 7 7800X3D), and while a scene streams in about a
//! thousand a second are made by the threads a frame waits for.
//!
//! A thread of low priority makes them while the title shows its logos and keeps some in
//! store afterwards (`VICTORY_BUFFERS_AHEAD=<n>`: that many at first, at most 1024 kept
//! afterwards; default 4096; `0`: none, every buffer is made when it is asked for, as before).
//! A buffer taken from here is what [`crate::host::Host::shared_buffer`] or
//! [`crate::host::Host::uav_buffer`] would have made then: new, zeroed, its memory there.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

/// The size of the buffers made ahead: what a title buffer of up to 64 KiB is rounded to.
pub(crate) const SIZE: u64 = 1 << 16;

/// Kept in store once the first lot is made.
const KEPT: usize = 1024;

/// A buffer the CPU maps, and where.
struct Shared(ID3D12Resource, usize);

// SAFETY: D3D12 objects are free-threaded; the address is of the buffer's own mapping, which
// lives as long as it does.
unsafe impl Send for Shared {}

struct Card(ID3D12Resource);

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Card {}

static SHARED: Mutex<Vec<Shared>> = Mutex::new(Vec::new());
static CARD: Mutex<Vec<Card>> = Mutex::new(Vec::new());
/// Whether the title's buffers get a side in the card's memory (seen by `crate::shadow`).
static CARD_WANTED: AtomicBool = AtomicBool::new(false);
/// Taken from the store: buffers the CPU maps, buffers on the card.
static TAKEN: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];

fn first() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_BUFFERS_AHEAD").ok().and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(4096).min(16384)
    })
}

/// Starts the thread that makes the buffers, once. Called when the first title buffer of any
/// size gets a copy in the card's memory ([`want_card`]), which is in a run's first second:
/// the store is full long before the first frame with a scene (started at the first buffer of
/// [`SIZE`], as until run v1q of 2026-11-27, it was 46 s into the run and 2 s before that
/// frame: 1,806 of its 3,674 buffers were still made when asked for).
///
/// Not at the first buffer of ours of any kind: the tools make such buffers too, and `victory
/// warm` starts a process for every 40 pipelines. With 4,096 buffers made in each of them it
/// compiled 18 pipelines a second in place of 350 (2026-11-27, 88,604 pipelines).
pub(crate) fn start() {
    if first() == 0 {
        return;
    }
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        if std::thread::Builder::new().name("victory-ahead".into()).spawn(fill).is_err() {
            tracing::warn!("no thread for the buffers made ahead: each is made when it is asked for");
        }
    });
}

/// Says that the title's buffers get a side in the card's memory, so such buffers are made
/// ahead from now on.
pub(crate) fn want_card() {
    CARD_WANTED.store(true, Ordering::Relaxed);
    start();
}

/// A buffer of [`SIZE`] the CPU maps, if one is in store.
pub(crate) fn shared() -> Option<(ID3D12Resource, *mut u8)> {
    if first() == 0 {
        return None;
    }
    let made = SHARED.lock().unwrap_or_else(|e| e.into_inner()).pop()?;
    TAKEN[0].fetch_add(1, Ordering::Relaxed);
    Some((made.0, made.1 as *mut u8))
}

/// A buffer of [`SIZE`] in the card's memory, if one is in store. Asking says that the
/// title's buffers get such a side, so they are made ahead from now on.
pub(crate) fn card() -> Option<ID3D12Resource> {
    if first() == 0 {
        return None;
    }
    CARD_WANTED.store(true, Ordering::Relaxed);
    let made = CARD.lock().unwrap_or_else(|e| e.into_inner()).pop()?;
    TAKEN[1].fetch_add(1, Ordering::Relaxed);
    Some(made.0)
}

// Opt-in: only idle, never-issued buffers are affected. The original path is unchanged.
fn adaptive() -> bool {
    std::env::var_os("VICTORY_LOWSPEC_ADAPTIVE").is_some_and(|v| v == "1")
}

const GIB: u64 = 1 << 30;

fn ram_pressure(was: bool, physical: u64, commit_free: u64) -> bool {
    if was {
        physical < 3 * GIB || commit_free < 2 * GIB
    } else {
        physical < 2 * GIB || commit_free < GIB
    }
}

fn card_pressure(was: bool, budget: u64, used: u64) -> bool {
    budget == 0 || used >= budget / 100 * if was { 65 } else { 80 }
}

fn system_commit_free() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{K32GetPerformanceInfo, PERFORMANCE_INFORMATION};
    let mut info = PERFORMANCE_INFORMATION {
        cb: std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    // SAFETY: the structure and length describe writable storage.
    if unsafe { K32GetPerformanceInfo(&mut info, info.cb) } == 0 {
        return None;
    }
    Some((info.CommitLimit.saturating_sub(info.CommitTotal) as u64).saturating_mul(info.PageSize as u64))
}

fn trim_idle<T>(pool: &Mutex<Vec<T>>, keep: usize) {
    let gone = {
        let mut pool = pool.lock().unwrap_or_else(|e| e.into_inner());
        let keep = keep.min(pool.len());
        pool.split_off(keep)
    };
    // Release COM resources outside the lock. These have never been handed to a title list.
    drop(gone);
}

fn fill() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
        // SAFETY: this thread's own priority.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    }
    // (The first buffer may be asked for while the host is still being set up.)
    let mut waited = 0;
    let host = loop {
        if let Some(host) = crate::host::host() {
            break host;
        }
        waited += 1;
        if waited > 1200 {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let began = std::time::Instant::now();
    let mut want = first();
    let mut said = false;
    let adaptive = adaptive();
    let mut ram_high = false;
    let mut card_high = false;
    let mut sampled = None::<std::time::Instant>;
    loop {
        if adaptive && sampled.is_none_or(|t| t.elapsed().as_secs() >= 1) {
            let physical = crate::host::available_physical_ram();
            let commit = system_commit_free();
            let memory = host.video_memory();
            let next_ram = match (physical, commit) {
                (Some(p), Some(c)) => ram_pressure(ram_high, p, c),
                _ => true,
            };
            let next_card = memory.is_none_or(|[budget, used, ..]| card_pressure(card_high, budget, used));
            if sampled.is_none() || next_ram != ram_high || next_card != card_high {
                tracing::info!(?physical, ?commit, ?memory, ram_pressure = next_ram, card_pressure = next_card,
                    "lowspec ahead pool pressure (bytes)");
            }
            ram_high = next_ram;
            card_high = next_card;
            if ram_high {
                trim_idle(&SHARED, 0);
            }
            if ram_high || card_high {
                trim_idle(&CARD, 0);
            }
            sampled = Some(std::time::Instant::now());
        }
        let shared = SHARED.lock().unwrap_or_else(|e| e.into_inner()).len();
        let card = CARD.lock().unwrap_or_else(|e| e.into_inner()).len();
        let card_wanted = CARD_WANTED.load(Ordering::Relaxed);
        // On the card only while it has room to spare: a store there must not take what
        // textures need (seven tenths of the budget; the title's own copies stop at eight).
        let card_room = card_wanted
            && host.video_memory().is_none_or(|[budget, used, ..]| used + SIZE <= budget / 100 * 70);
        let (more_shared, more_card) = (
            shared < want && !ram_high,
            card < want && card_room && !ram_high && !card_high,
        );
        if !more_shared && !more_card {
            if !said && shared >= want && (card >= want || !card_room) && card_wanted {
                said = true;
                tracing::info!(
                    "buffers of 64 KiB made ahead (VICTORY_BUFFERS_AHEAD={}): {} the CPU maps, {} on the card, in store {:.1} s after \
                     the first title buffer got its copy on the card; taken meanwhile {} and {}; from now on {} are kept",
                    first(),
                    shared,
                    card,
                    began.elapsed().as_secs_f64(),
                    TAKEN[0].load(Ordering::Relaxed),
                    TAKEN[1].load(Ordering::Relaxed),
                    first().min(KEPT)
                );
                want = first().min(KEPT);
            }
            std::thread::sleep(std::time::Duration::from_millis(if said { 20 } else { 50 }));
            continue;
        }
        if more_shared
            && let Some((buffer, data)) = host.shared_buffer_now(SIZE)
        {
            SHARED.lock().unwrap_or_else(|e| e.into_inner()).push(Shared(buffer, data as usize));
        }
        if more_card
            && let Some(buffer) = host.uav_buffer(SIZE)
        {
            CARD.lock().unwrap_or_else(|e| e.into_inner()).push(Card(buffer));
        }
        if adaptive {
            // Back off if allocation fails rather than repeatedly hammering the driver.
            let shared_now = SHARED.lock().unwrap_or_else(|e| e.into_inner()).len();
            let card_now = CARD.lock().unwrap_or_else(|e| e.into_inner()).len();
            if shared_now <= shared && card_now <= card {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_pressure_has_hysteresis() {
        assert!(ram_pressure(false, GIB, 4 * GIB));
        assert!(ram_pressure(false, 4 * GIB, GIB - 1));
        assert!(ram_pressure(true, 2 * GIB, 3 * GIB));
        assert!(!ram_pressure(true, 3 * GIB, 2 * GIB));
        assert!(!ram_pressure(false, 2 * GIB, GIB));
    }

    #[test]
    fn card_pressure_has_hysteresis() {
        assert!(card_pressure(false, 1000, 800));
        assert!(card_pressure(true, 1000, 700));
        assert!(!card_pressure(true, 1000, 649));
        assert!(card_pressure(false, 0, 0));
    }

    #[test]
    fn only_idle_pool_is_trimmed() {
        let pool = Mutex::new(vec![1, 2, 3]);
        trim_idle(&pool, 1);
        assert_eq!(*pool.lock().unwrap(), vec![1]);
        trim_idle(&pool, 0);
        assert!(pool.lock().unwrap().is_empty());
    }
}
