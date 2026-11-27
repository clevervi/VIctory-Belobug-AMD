//! `VICTORY_FENCE_LAG=1`: why fence signals set behind flights reach the
//! title late. For each one, three times: queued (`late_signal`), the card
//! seen done with the flights landed with it, the value set; and the bytes
//! shaders wrote that were copied back into title memory by that landing.
//! A line every ~5 s. Nothing is recorded without the variable.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FENCE_LAG").is_some_and(|v| v == "1"))
}

thread_local! {
    /// Bytes copied back into title memory on this thread ([`copied`]).
    static COPIED: Cell<u64> = const { Cell::new(0) };
}

/// Bytes copied back into title memory (beside `COPY_BACK[1]`).
pub(crate) fn copied(bytes: u64) {
    if on() {
        COPIED.with(|c| c.set(c.get() + bytes));
    }
}

/// This thread's copied-back bytes so far; a landing takes the difference.
pub(crate) fn copied_so_far() -> u64 {
    COPIED.with(Cell::get)
}

struct State {
    queued: HashMap<(usize, u64), Instant>,
    /// (gpu_done - queued, set - gpu_done, copied back zero bytes).
    samples: Vec<(Duration, Duration, bool)>,
    since: Instant,
}

fn state() -> std::sync::MutexGuard<'static, State> {
    static STATE: std::sync::LazyLock<Mutex<State>> = std::sync::LazyLock::new(|| {
        Mutex::new(State { queued: HashMap::new(), samples: Vec::new(), since: Instant::now() })
    });
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// `late_signal` put `(fence, value)` behind the flights.
pub(crate) fn queued(fence: usize, value: u64) {
    if on() {
        state().queued.insert((fence, value), Instant::now());
    }
}

/// `(fence, value)` was set now; the card was seen done at `gpu_done` and
/// the landing copied back `copied` bytes.
pub(crate) fn set(fence: usize, value: u64, gpu_done: Instant, copied: u64) {
    if !on() {
        return;
    }
    let now = Instant::now();
    let mut s = state();
    let Some(at) = s.queued.remove(&(fence, value)) else {
        return;
    };
    s.samples.push((gpu_done.saturating_duration_since(at), now.saturating_duration_since(gpu_done), copied == 0));
    if now.duration_since(s.since) < Duration::from_secs(5) {
        return;
    }
    let samples = std::mem::take(&mut s.samples);
    s.since = now;
    drop(s);
    let ms = |d: &Duration| d.as_secs_f64() * 1e3;
    let mut card: Vec<f64> = samples.iter().map(|(c, ..)| ms(c)).collect();
    let mut told: Vec<f64> = samples.iter().map(|(_, t, _)| ms(t)).collect();
    let mut clean: Vec<f64> = samples.iter().filter(|s| s.2).map(|(c, t, _)| ms(c) + ms(t)).collect();
    tracing::info!(
        "fence signals behind flights (VICTORY_FENCE_LAG=1): {} set; queued to card done {:.2} ms median, {:.2} p90; \
         card done to set {:.2} ms median, {:.2} p90; {:.0}% behind landings that copied back nothing, their lag {:.2} ms median",
        samples.len(),
        percentile(&mut card, 50.0),
        percentile(&mut card, 90.0),
        percentile(&mut told, 50.0),
        percentile(&mut told, 90.0),
        100.0 * clean.len() as f64 / samples.len() as f64,
        percentile(&mut clean, 50.0)
    );
}

/// The `p`th percentile (nearest rank) of `values`, sorted in place; 0 if empty.
fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_unstable_by(f64::total_cmp);
    let rank = ((p / 100.0) * values.len() as f64).ceil() as usize;
    values[rank.clamp(1, values.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn nearest_rank() {
        let mut v: Vec<f64> = (1..=10).rev().map(f64::from).collect();
        assert_eq!(percentile(&mut v, 50.0), 5.0);
        assert_eq!(percentile(&mut v, 90.0), 9.0);
        assert_eq!(percentile(&mut v, 100.0), 10.0);
        assert_eq!(percentile(&mut [], 50.0), 0.0);
    }
}
