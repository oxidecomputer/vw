//! Shared cargo-nextest-style runner UI, used by both `vw test` (HTCL) and
//! `vw bench run` (VHDL/cosim testbenches) so the two look identical.
//!
//! [`NextestPanel`] drives an [`indicatif::MultiProgress`] live display: a
//! `Running` progress bar with running/passed/failed counts, one live row
//! per in-flight test (ticking elapsed), and completed `PASS`/`FAIL` lines
//! scrolling above the panel. On a non-TTY the panel is hidden and only the
//! permanent `PASS`/`FAIL` lines and the summary survive.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use colored::*;
use indicatif::{
    MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle,
};

/// Format a duration the way nextest does: right-aligned seconds with
/// millisecond precision, e.g. `" 1.234s"`.
pub fn format_secs(secs: f64) -> String {
    format!("{secs:>6.3}s")
}

/// Print the final `test result: ok/FAILED. N passed; M failed; finished
/// in T` line shared by both runners.
pub fn print_result_line(
    passed: usize,
    failed: usize,
    elapsed: std::time::Duration,
) {
    let outcome = if failed == 0 {
        "ok".green().bold().to_string()
    } else {
        "FAILED".red().bold().to_string()
    };
    println!(
        "\ntest result: {outcome}. {} passed; {} failed; finished in {}",
        passed.to_string().green(),
        failed.to_string().red(),
        format_secs(elapsed.as_secs_f64()),
    );
}

/// A cargo-nextest-style live panel. Thread-safe: `&self` methods can be
/// called concurrently from parallel runner tasks (share it via `Arc`).
pub struct NextestPanel {
    multi: MultiProgress,
    bar: ProgressBar,
    is_tty: bool,
    running: AtomicUsize,
    passed: AtomicUsize,
    failed: AtomicUsize,
}

impl NextestPanel {
    /// Set up the panel for `total` items. `noun` is the plural label for
    /// the `running N <noun>` header (e.g. `"tests"`, `"testbenches"`).
    pub fn new(total: u64, noun: &str) -> Self {
        println!("\nrunning {} {noun}", total.to_string().bold());
        let is_tty = std::io::stderr().is_terminal();
        let multi = MultiProgress::new();
        if !is_tty {
            multi.set_draw_target(ProgressDrawTarget::hidden());
        }
        let bar = multi.add(ProgressBar::new(total));
        bar.set_style(
            ProgressStyle::with_template(
                "{prefix:>12.cyan.bold} [{elapsed_precise}] \
                 {wide_bar:.cyan/blue} {pos}/{len}: {msg}",
            )
            .expect("valid template")
            .progress_chars("█▉▊▋▌▍▎▏  "),
        );
        bar.set_prefix("Running");
        bar.enable_steady_tick(Duration::from_millis(120));
        let panel = Self {
            multi,
            bar,
            is_tty,
            running: AtomicUsize::new(0),
            passed: AtomicUsize::new(0),
            failed: AtomicUsize::new(0),
        };
        panel.refresh_counts();
        panel
    }

    fn refresh_counts(&self) {
        self.bar.set_message(format!(
            "{} running, {} passed, {} failed",
            self.running.load(Ordering::SeqCst),
            self.passed.load(Ordering::SeqCst),
            self.failed.load(Ordering::SeqCst),
        ));
    }

    /// Register a test as started; returns its live row (which ticks its
    /// own elapsed until [`finish`](Self::finish) is called).
    pub fn start(&self, label: &str) -> ProgressBar {
        let row = self.multi.add(ProgressBar::new_spinner());
        row.set_style(
            ProgressStyle::with_template(
                "             [{elapsed_precise}] {msg}",
            )
            .expect("valid template"),
        );
        row.set_message(label.to_string());
        row.enable_steady_tick(Duration::from_millis(120));
        self.running.fetch_add(1, Ordering::SeqCst);
        self.refresh_counts();
        row
    }

    /// Mark a test complete: clear its row, print a `PASS`/`FAIL` line above
    /// the panel, and advance the bar + counts.
    pub fn finish(
        &self,
        row: &ProgressBar,
        label: &str,
        passed: bool,
        secs: f64,
    ) {
        row.finish_and_clear();
        self.multi.remove(row);
        self.running.fetch_sub(1, Ordering::SeqCst);
        if passed {
            self.passed.fetch_add(1, Ordering::SeqCst);
        } else {
            self.failed.fetch_add(1, Ordering::SeqCst);
        }
        // Right-align PASS/FAIL to 12 cols so it lines up under `Running`.
        let word = format!("{:>12}", if passed { "PASS" } else { "FAIL" });
        let status = if passed {
            word.green().bold()
        } else {
            word.red().bold()
        };
        self.println(format!("{status} [{}] {}", format_secs(secs), label));
        self.bar.inc(1);
        self.refresh_counts();
    }

    /// Print a line above the live panel (plain `println!` on a non-TTY).
    pub fn println(&self, line: String) {
        if self.is_tty {
            let _ = self.multi.println(line);
        } else {
            println!("{line}");
        }
    }

    pub fn passed(&self) -> usize {
        self.passed.load(Ordering::SeqCst)
    }

    pub fn failed(&self) -> usize {
        self.failed.load(Ordering::SeqCst)
    }

    /// Tear the live bar down at the end of the run.
    pub fn clear(&self) {
        self.bar.finish_and_clear();
        self.multi.remove(&self.bar);
    }
}
