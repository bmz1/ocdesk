//! Frame-time probe for perf work.
//!
//! Measures the CPU side of each frame: from the root view's `render` (the
//! first thing a frame does) to the paint of a zero-size canvas that is the
//! root's last child (the last thing it paints). GPU time and present are
//! not included. Once a second with frames in it, logs count, mean, p95 and
//! max, plus peak RSS.
//!
//! Logged at `debug` in debug builds (`OCDESK_LOG=ocdesk=debug`); set
//! `OCDESK_FRAME_LOG=1` to log at `info` in any build.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::{IntoElement, Styled as _, canvas};

#[derive(Clone)]
pub struct FrameStats(Option<Rc<RefCell<Inner>>>);

struct Inner {
    info: bool,
    frame_start: Option<Instant>,
    period_start: Instant,
    samples: Vec<Duration>,
}

impl FrameStats {
    pub fn new() -> Self {
        let info = std::env::var_os("OCDESK_FRAME_LOG").is_some();
        let enabled = info || (cfg!(debug_assertions) && tracing::enabled!(tracing::Level::DEBUG));
        Self(enabled.then(|| {
            Rc::new(RefCell::new(Inner {
                info,
                frame_start: None,
                period_start: Instant::now(),
                samples: Vec::with_capacity(256),
            }))
        }))
    }

    /// Call at the top of the root view's `render`.
    pub fn begin(&self) {
        if let Some(i) = &self.0 {
            i.borrow_mut().frame_start = Some(Instant::now());
        }
    }

    /// An element to add as the root's last child.
    pub fn end_marker(&self) -> Option<impl IntoElement + use<>> {
        let inner = self.0.clone()?;
        Some(
            canvas(
                |_, _, _| (),
                move |_, _, _, _| inner.borrow_mut().end(Instant::now()),
            )
            .absolute()
            .size_0(),
        )
    }
}

impl Inner {
    fn end(&mut self, now: Instant) {
        let Some(start) = self.frame_start.take() else {
            return;
        };
        self.samples.push(now - start);
        if now - self.period_start < Duration::from_secs(1) {
            return;
        }
        let s = summarize(&mut self.samples);
        let rss = peak_rss_mb();
        if self.info {
            tracing::info!(target: "ocdesk::frames", frames = s.count, mean_ms = s.mean_ms, p95_ms = s.p95_ms, max_ms = s.max_ms, peak_rss_mb = rss, "frame times");
        } else {
            tracing::debug!(target: "ocdesk::frames", frames = s.count, mean_ms = s.mean_ms, p95_ms = s.p95_ms, max_ms = s.max_ms, peak_rss_mb = rss, "frame times");
        }
        self.samples.clear();
        self.period_start = now;
    }
}

#[derive(Debug, PartialEq)]
struct Summary {
    count: usize,
    mean_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

fn summarize(samples: &mut [Duration]) -> Summary {
    samples.sort_unstable();
    let ms = |d: Duration| (d.as_secs_f64() * 1e4).round() / 10.;
    let n = samples.len();
    let total: Duration = samples.iter().sum();
    Summary {
        count: n,
        mean_ms: if n == 0 { 0. } else { ms(total / n as u32) },
        p95_ms: samples
            .get((n * 95).div_ceil(100).saturating_sub(1))
            .map_or(0., |d| ms(*d)),
        max_ms: samples.last().map_or(0., |d| ms(*d)),
    }
}

/// Peak resident set size in MB (`getrusage`: KB on Linux, bytes on macOS).
fn peak_rss_mb() -> u64 {
    // SAFETY: getrusage only writes into the struct we pass.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return 0;
    }
    let raw = ru.ru_maxrss as u64;
    if cfg!(target_os = "macos") {
        raw / (1024 * 1024)
    } else {
        raw / 1024
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_percentiles() {
        let mut v: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        let s = summarize(&mut v);
        assert_eq!(s.count, 100);
        assert_eq!(s.p95_ms, 95.);
        assert_eq!(s.max_ms, 100.);
        assert_eq!(s.mean_ms, 50.5);
    }
}
