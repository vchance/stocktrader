//! Keeping strategy research honest. Ordinary runs never see a held-back period at the end of the data,
//! and every run is logged, so the number of ideas tried (and of looks at the held-back period) stays
//! visible when judging a result.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Days, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// Research settings, in the plans directory (skipped when listing plans).
pub const CONFIG_FILE: &str = "_research.toml";
/// History replayed before the held-back period so indicators and liquidity windows are warm.
const HOLDOUT_WARMUP_DAYS: u64 = 400;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Research {
    /// Results from this date on are held back: ordinary runs end the day before it.
    #[serde(default, deserialize_with = "crate::plans::toml_date")]
    pub holdout_start: Option<NaiveDate>,
}

impl Research {
    /// The settings in `plans_dir`, or none (no hold-out) if the file doesn't exist.
    pub fn load(plans_dir: &Path) -> Result<Self> {
        let path = plans_dir.join(CONFIG_FILE);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

/// The dates one run covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// First session of data read.
    pub start: Option<NaiveDate>,
    /// Last session of data read.
    pub end: Option<NaiveDate>,
    /// First traded session; data before it only warms the strategy up.
    pub trade_from: Option<NaiveDate>,
    /// This run evaluates the held-back period.
    pub holdout: bool,
}

impl Window {
    pub fn describe(&self) -> String {
        let d = |d: Option<NaiveDate>| d.map_or("…".to_string(), |d| d.to_string());
        match self.trade_from {
            Some(from) if self.holdout => format!("HELD-BACK PERIOD {from} .. {}", d(self.end)),
            Some(from) => format!("{from} .. {}", d(self.end)),
            None => format!("{} .. {}", d(self.start), d(self.end)),
        }
    }
}

/// The window for a run. `requested` is what `--start/--end` asked for, `planned` what the plan(s) ask for.
/// Without `holdout`, the run ends before the held-back period; asking for dates inside it is an error, so it
/// can't be looked at by accident. With `holdout`, the run trades only the held-back period.
pub fn resolve(
    research: &Research,
    requested: (Option<NaiveDate>, Option<NaiveDate>),
    planned: (Option<NaiveDate>, Option<NaiveDate>),
    holdout: bool,
) -> Result<Window> {
    let end = requested.1.or(planned.1);
    if holdout {
        let Some(h) = research.holdout_start else {
            bail!("no held-back period is configured: set holdout_start in plans/{CONFIG_FILE}");
        };
        if requested.0.is_some() {
            bail!("--start can't be combined with --holdout: the held-back period starts at {h}");
        }
        if requested.1.is_some_and(|e| e < h) {
            bail!("--end is before the held-back period, which starts at {h}");
        }
        // A plan's own end before the hold-out bounds its development period, not the held-back test.
        let end = requested.1.or(planned.1.filter(|e| *e >= h));
        return Ok(Window { start: Some(h - Days::new(HOLDOUT_WARMUP_DAYS)), end, trade_from: Some(h), holdout: true });
    }
    let start = requested.0.or(planned.0);
    let Some(h) = research.holdout_start else {
        return Ok(Window { start, end, trade_from: None, holdout: false });
    };
    let last = h - Days::new(1);
    for (flag, date) in [("--start", requested.0), ("--end", requested.1)] {
        if let Some(d) = date
            && d >= h
        {
            bail!("{flag} {d} reaches into the held-back period (from {h}); evaluate it deliberately with --holdout");
        }
    }
    if start.is_some_and(|s| s >= h) {
        bail!("the plan starts at or after the held-back period (from {h}); move its start earlier");
    }
    Ok(Window { start, end: Some(end.map_or(last, |e| e.min(last))), trade_from: None, holdout: false })
}

/// One plan's outcome in a logged run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanResult {
    pub name: String,
    /// The plan file as it was, so any logged result can be reproduced.
    pub source: String,
    pub total_return: f64,
    pub cagr: f64,
    pub max_drawdown: f64,
    pub sharpe: f64,
    pub fills: usize,
    pub benchmark: Option<String>,
    pub benchmark_cagr: Option<f64>,
}

/// One logged run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub at: DateTime<Utc>,
    /// `backtest` or `compare`.
    pub command: String,
    pub synthetic: bool,
    pub window: Window,
    pub plans: Vec<PlanResult>,
}

pub fn append(log: &Path, entry: &Entry) -> Result<()> {
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(log).with_context(|| format!("opening {}", log.display()))?;
    // One write per entry, so concurrent runs can't interleave within a line.
    let line = format!("{}\n", serde_json::to_string(entry)?);
    file.write_all(line.as_bytes()).with_context(|| format!("writing {}", log.display()))
}

/// Every entry, oldest first, and how many lines couldn't be read. A missing log is empty.
pub fn read(log: &Path) -> Result<(Vec<Entry>, usize)> {
    if !log.exists() {
        return Ok((Vec::new(), 0));
    }
    let text = fs::read_to_string(log).with_context(|| format!("reading {}", log.display()))?;
    let mut entries = Vec::new();
    let mut bad = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str(line) {
            Ok(e) => entries.push(e),
            Err(_) => bad += 1,
        }
    }
    Ok((entries, bad))
}

/// Real-data runs that evaluated `plan` on the held-back period.
pub fn holdout_looks(entries: &[Entry], plan: &str) -> usize {
    entries.iter().filter(|e| !e.synthetic && e.window.holdout && e.plans.iter().any(|p| p.name == plan)).count()
}

/// `stocktrader plans log`: how many experiments were run, per plan, and the most recent ones.
pub fn print_log(log: &Path, last: usize) -> Result<()> {
    let (entries, bad) = read(log)?;
    let real: Vec<&Entry> = entries.iter().filter(|e| !e.synthetic).collect();
    println!("log               {}", log.display());
    println!("experiments       {} on real data ({} synthetic, not counted)", real.len(), entries.len() - real.len());
    if bad > 0 {
        println!("unreadable lines  {bad}");
    }
    #[derive(Default)]
    struct PerPlan {
        runs: usize,
        holdout: usize,
        versions: std::collections::BTreeSet<String>,
    }
    let mut per_plan: BTreeMap<&str, PerPlan> = BTreeMap::new();
    for e in &real {
        for p in &e.plans {
            let s = per_plan.entry(&p.name).or_default();
            s.runs += 1;
            s.holdout += usize::from(e.window.holdout);
            s.versions.insert(p.source.clone());
        }
    }
    if !per_plan.is_empty() {
        println!();
        println!("{:<24} {:>5} {:>9} {:>14}", "plan", "runs", "versions", "held-back looks");
        for (name, s) in &per_plan {
            println!("{name:<24} {:>5} {:>9} {:>14}", s.runs, s.versions.len(), s.holdout);
        }
    }
    let recent: Vec<&&Entry> = real.iter().rev().take(last).collect();
    if !recent.is_empty() {
        println!();
        println!("most recent {} (newest first):", recent.len());
        for e in recent {
            let results: Vec<String> = e
                .plans
                .iter()
                .map(|p| {
                    let vs = p.benchmark_cagr.map_or(String::new(), |b| format!(" ({:+.1} vs {})", (p.cagr - b) * 100.0, p.benchmark.as_deref().unwrap_or("bench")));
                    format!("{} {:.1}%{vs}", p.name, p.cagr * 100.0)
                })
                .collect();
            println!("  {} {:<8} {:<44} {}", e.at.format("%Y-%m-%d %H:%M"), e.command, e.window.describe(), results.join(", "));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn research() -> Research {
        Research { holdout_start: Some(d(2024, 1, 1)) }
    }

    #[test]
    fn ordinary_runs_stop_before_the_held_back_period() {
        let w = resolve(&research(), (None, None), (Some(d(2018, 1, 1)), None), false).unwrap();
        assert_eq!(w, Window { start: Some(d(2018, 1, 1)), end: Some(d(2023, 12, 31)), trade_from: None, holdout: false });
        // A plan's own later end is clipped; an earlier one is kept.
        assert_eq!(resolve(&research(), (None, None), (None, Some(d(2025, 6, 1))), false).unwrap().end, Some(d(2023, 12, 31)));
        assert_eq!(resolve(&research(), (None, Some(d(2022, 6, 1))), (None, None), false).unwrap().end, Some(d(2022, 6, 1)));
        // Asking for held-back dates explicitly is an error, not a silent clip.
        assert!(resolve(&research(), (None, Some(d(2024, 3, 1))), (None, None), false).is_err());
        assert!(resolve(&research(), (Some(d(2024, 2, 1)), None), (None, None), false).is_err());
        // Without a configured hold-out, everything is as asked.
        assert_eq!(resolve(&Research::default(), (None, None), (None, None), false).unwrap(), Window::default());
    }

    #[test]
    fn holdout_runs_trade_only_the_held_back_period() {
        let w = resolve(&research(), (None, None), (Some(d(2018, 1, 1)), None), true).unwrap();
        assert_eq!(w.trade_from, Some(d(2024, 1, 1)));
        assert_eq!(w.start, Some(d(2022, 11, 27)), "warm-up before the period");
        assert!(w.holdout);
        assert!(resolve(&research(), (Some(d(2020, 1, 1)), None), (None, None), true).is_err());
        // A plan's own earlier end doesn't block the held-back test; an explicit one does.
        assert_eq!(resolve(&research(), (None, None), (None, Some(d(2023, 6, 30))), true).unwrap().end, None);
        assert!(resolve(&research(), (None, Some(d(2023, 6, 30))), (None, None), true).is_err());
        assert!(resolve(&Research::default(), (None, None), (None, None), true).is_err());
    }

    #[test]
    fn log_round_trips_and_counts_held_back_looks() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("research/experiments.jsonl");
        let result = |name: &str| PlanResult {
            name: name.into(),
            source: "name = \"x\"".into(),
            total_return: 0.5,
            cagr: 0.05,
            max_drawdown: 0.2,
            sharpe: 0.4,
            fills: 10,
            benchmark: Some("SPY".into()),
            benchmark_cagr: Some(0.1),
        };
        let entry = |holdout: bool, synthetic: bool| Entry {
            at: Utc::now(),
            command: "backtest".into(),
            synthetic,
            window: Window { holdout, ..Window::default() },
            plans: vec![result("a")],
        };
        append(&log, &entry(false, false)).unwrap();
        append(&log, &entry(true, false)).unwrap();
        append(&log, &entry(true, true)).unwrap();
        fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(b"not json\n").unwrap();
        let (entries, bad) = read(&log).unwrap();
        assert_eq!((entries.len(), bad), (3, 1));
        assert_eq!(holdout_looks(&entries, "a"), 1, "synthetic runs don't count");
        assert_eq!(holdout_looks(&entries, "b"), 0);
        assert_eq!(read(&dir.path().join("missing.jsonl")).unwrap().0, vec![]);
    }
}
