//! Page-performance instrumentation (M11): named, Rust-side wall-clock
//! measurements of the things the browser itself does — page load, each network
//! request, scripts, style, layout+paint, form submit.
//!
//! Design notes:
//! - **Rust-side only.** Pages have no clock (`Date.now`/`performance.now` are
//!   not exposed; the speed-first prelude fires timers immediately), so we time
//!   at the boundaries we already drive. Nothing here is exposed to page JS, so
//!   no high-res fingerprint/timing-attack surface is added.
//! - **Stable order.** Rows keep their insertion order and update *in place*
//!   (keyed by label), so the on-screen HUD never reorders or bounces — the
//!   user has time to read it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// One measured row: a label and its most recent duration.
#[derive(Clone, Debug)]
pub struct TimingRow {
    pub label: String,
    pub dur: Duration,
}

/// A stable-ordered table of named measurements for the current page.
#[derive(Default)]
pub struct Timings {
    rows: Vec<TimingRow>,
    index: HashMap<String, usize>,
    nav_start: Option<Instant>,
}

impl Timings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Begin a new page: clear the table and stamp the navigation start. The
    /// previous page's rows are dropped only here, so they stay readable until
    /// the next navigation actually begins.
    pub fn begin_navigation(&mut self) {
        self.rows.clear();
        self.index.clear();
        self.nav_start = Some(Instant::now());
    }

    /// Record (or update in place) `label`'s duration.
    pub fn record(&mut self, label: impl Into<String>, dur: Duration) {
        let label = label.into();
        if let Some(&i) = self.index.get(&label) {
            self.rows[i].dur = dur;
        } else {
            self.index.insert(label.clone(), self.rows.len());
            self.rows.push(TimingRow { label, dur });
        }
    }

    /// Add to a row's accumulated duration (for aggregates like "subresources"
    /// that sum many requests into one stable row).
    pub fn add(&mut self, label: impl Into<String>, dur: Duration) {
        let label = label.into();
        if let Some(&i) = self.index.get(&label) {
            self.rows[i].dur = self.rows[i].dur.saturating_add(dur);
        } else {
            self.index.insert(label.clone(), self.rows.len());
            self.rows.push(TimingRow { label, dur });
        }
    }

    /// Elapsed since [`begin_navigation`], if a navigation is in flight.
    pub fn since_nav(&self) -> Option<Duration> {
        self.nav_start.map(|t| t.elapsed())
    }

    /// Record the "page load" total from the navigation start to now.
    pub fn record_page_load(&mut self) {
        if let Some(d) = self.since_nav() {
            self.record("page load", d);
        }
    }

    pub fn rows(&self) -> &[TimingRow] {
        &self.rows
    }

    /// `(label, milliseconds)` pairs, for `RenderOutcome` / automation.
    pub fn as_pairs(&self) -> Vec<(String, f64)> {
        self.rows()
            .iter()
            .map(|r| (r.label.clone(), r.dur.as_secs_f64() * 1000.0))
            .collect()
    }

    /// `(label, formatted)` pairs for the HUD, in stable order.
    pub fn display_rows(&self) -> Vec<(String, String)> {
        self.rows()
            .iter()
            .map(|r| (r.label.clone(), fmt_dur(r.dur)))
            .collect()
    }
}

/// Format a duration with adaptive units (ns / µs / ms / s), ~3 sig figs.
pub fn fmt_dur(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns < 1_000 {
        format!("{ns} ns")
    } else if ns < 1_000_000 {
        format!("{:.2} µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2} s", ns as f64 / 1_000_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_dur_picks_sensible_units() {
        assert_eq!(fmt_dur(Duration::from_nanos(420)), "420 ns");
        assert_eq!(fmt_dur(Duration::from_nanos(1_500)), "1.50 µs");
        assert_eq!(fmt_dur(Duration::from_micros(2_500)), "2.50 ms");
        assert_eq!(fmt_dur(Duration::from_millis(1_500)), "1.50 s");
    }

    #[test]
    fn rows_keep_insertion_order_and_update_in_place() {
        let mut t = Timings::new();
        t.record("parse", Duration::from_millis(1));
        t.record("style", Duration::from_millis(2));
        t.record("parse", Duration::from_millis(5)); // update, not append
        let rows = t.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "parse");
        assert_eq!(rows[0].dur, Duration::from_millis(5));
        assert_eq!(rows[1].label, "style");
    }

    #[test]
    fn begin_navigation_resets_and_stamps() {
        let mut t = Timings::new();
        t.record("x", Duration::from_millis(1));
        t.begin_navigation();
        assert!(t.rows().is_empty());
        assert!(t.since_nav().is_some());
        t.record_page_load();
        assert_eq!(t.rows()[0].label, "page load");
    }

    #[test]
    fn add_accumulates_into_one_stable_row() {
        let mut t = Timings::new();
        t.add("subresources", Duration::from_millis(2));
        t.add("subresources", Duration::from_millis(3));
        assert_eq!(t.rows().len(), 1);
        assert_eq!(t.rows()[0].dur, Duration::from_millis(5));
    }
}
