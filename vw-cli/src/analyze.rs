// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `vw analyze timing` — near-critical path population analysis.
//!
//! Consumes the CSV that `vw::worst_paths` writes (one row per
//! timing endpoint under a slack threshold, out of
//! `get_timing_paths -slack_lesser_than`) and reports three tables:
//!
//! - **summary** — population size, the tightest band, the deep-logic
//!   tail, and the logic/route split
//! - **bands** — endpoint counts bucketed by slack
//! - **blocks** — endpoint counts attributed to the enclosing
//!   hierarchy block of each endpoint
//!
//! Given two CSVs it reports before/after/delta on every row, which
//! is the useful form when asking whether an RTL change actually
//! relieved timing pressure. Given one, it reports that CSV alone.
//!
//! Why counts rather than WNS: a design that closes has WNS pinned at
//! or just above zero however much headroom it really has, because
//! the implementation tools stop optimizing a path once its slack is
//! non-negative. What moves is the size and shape of the
//! near-critical population — and unlike a path-by-path diff, a
//! population statistic survives the instance renaming that any RTL
//! restructuring causes.

use std::collections::HashMap;

use camino::{Utf8Path, Utf8PathBuf};
use colored::*;
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AnalyzeError {
    #[error("reading {path}")]
    Read {
        path: Utf8PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is empty")]
    Empty { path: Utf8PathBuf },
    #[error(
        "{path}: unrecognized header\n  expected: {expected}\n  \
         found:    {found}"
    )]
    Header {
        path: Utf8PathBuf,
        expected: String,
        found: String,
    },
    #[error("{path}: no parseable rows{filtered}")]
    NoRows { path: Utf8PathBuf, filtered: String },
    #[error("no CSV given; pass one to summarize, or two to compare")]
    NoFiles,
    #[error("--total-endpoints given {given} time(s), but {files} CSV(s)")]
    TotalsArity { given: usize, files: usize },
    #[error("serializing json")]
    Json(#[from] serde_json::Error),
}

/// Header `vw::worst_paths` writes. Checked rather than assumed —
/// silently misreading column order would produce plausible-looking
/// nonsense.
const HEADER: &str =
    "path,slack,levels,delay,logic,net,pct_net,skew,uncertainty,group";
const N_FIELDS: usize = 10;

/// Logic depth at or above which a path counts toward the "deep"
/// tail. Sits above the typical 4-5 level bulk so the row tracks the
/// tail rather than the population.
const DEEP_LEVELS: u32 = 7;
// Net share is reported as percentiles rather than as counts either
// side of a threshold. The distribution is tight and unimodal — on a
// real design it spanned p10=74.0 to p90=84.7 — so a cut anywhere
// near the middle lands on the steep part and turns a small uniform
// shift into a large swing in the count. A ~1.8 point move in the
// median once showed up as a 9.45 point move in a ">75% net" count,
// which reads as paths changing character when nothing of the sort
// happened. Percentiles need no threshold to justify and leave no
// unnamed band between rows.

/// Slack band edges, in ns. The final band's upper edge is
/// inclusive; anything above it lands in a generated overflow band,
/// so a CSV cut at a threshold other than 0.2 still totals correctly.
const BANDS: &[(f64, f64)] = &[
    (0.000, 0.010),
    (0.010, 0.025),
    (0.025, 0.050),
    (0.050, 0.100),
    (0.100, 0.150),
    (0.150, 0.200),
];

// ---------------------------------------------------------------
// parsing
// ---------------------------------------------------------------

struct Row {
    endpoint: String,
    slack: f64,
    levels: u32,
    logic: f64,
    net: f64,
    pct_net: f64,
    group: String,
}

/// One CSV, parsed and filtered.
pub struct Analysis {
    file: Utf8PathBuf,
    rows: Vec<Row>,
    /// Rows that did not parse. Non-zero usually means the writer
    /// never closed the file and the last record was cut mid-line.
    skipped: usize,
    /// Every path group seen before `--group` filtering, with counts.
    groups: Vec<(String, usize)>,
}

/// Take the endpoint half of a `{start --> end}` path label.
fn endpoint_of(path: &str) -> &str {
    let p = path.trim().trim_start_matches('{').trim_end_matches('}');
    match p.split_once("-->") {
        Some((_, end)) => end.trim(),
        None => p.trim(),
    }
}

/// Enclosing block of an endpoint, to `depth` hierarchy levels.
fn block_of(endpoint: &str, depth: usize) -> String {
    endpoint
        .split('/')
        .take(depth)
        .collect::<Vec<_>>()
        .join("/")
}

/// A row's fixed trailing fields are parsed right-to-left so that a
/// comma inside the path label cannot shift the numeric columns.
fn parse_row(line: &str, depth: usize) -> Option<Row> {
    let mut f = line.rsplitn(N_FIELDS, ',');
    let group = f.next()?.trim().to_string();
    let _uncertainty: f64 = f.next()?.trim().parse().ok()?;
    let _skew: f64 = f.next()?.trim().parse().ok()?;
    let pct_net: f64 = f.next()?.trim().parse().ok()?;
    let net: f64 = f.next()?.trim().parse().ok()?;
    let logic: f64 = f.next()?.trim().parse().ok()?;
    let _delay: f64 = f.next()?.trim().parse().ok()?;
    let levels: u32 = f.next()?.trim().parse().ok()?;
    let slack: f64 = f.next()?.trim().parse().ok()?;
    let path = f.next()?;
    let _ = depth;
    Some(Row {
        endpoint: endpoint_of(path).to_string(),
        slack,
        levels,
        logic,
        net,
        pct_net,
        group,
    })
}

impl Analysis {
    pub fn load(
        path: &Utf8Path,
        group: Option<&str>,
    ) -> Result<Self, AnalyzeError> {
        let text = std::fs::read_to_string(path).map_err(|source| {
            AnalyzeError::Read {
                path: path.to_owned(),
                source,
            }
        })?;
        let mut lines = text.lines();
        let header = lines.next().ok_or_else(|| AnalyzeError::Empty {
            path: path.to_owned(),
        })?;
        if header.trim() != HEADER {
            return Err(AnalyzeError::Header {
                path: path.to_owned(),
                expected: HEADER.to_string(),
                found: header.trim().to_string(),
            });
        }

        let mut rows = Vec::new();
        let mut skipped = 0usize;
        let mut groups: HashMap<String, usize> = HashMap::new();
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            match parse_row(line, 0) {
                Some(r) => {
                    *groups.entry(r.group.clone()).or_default() += 1;
                    rows.push(r);
                }
                None => skipped += 1,
            }
        }

        if let Some(g) = group {
            rows.retain(|r| r.group == g);
        }
        if rows.is_empty() {
            return Err(AnalyzeError::NoRows {
                path: path.to_owned(),
                filtered: match group {
                    Some(g) => format!(" in path group `{g}`"),
                    None => String::new(),
                },
            });
        }

        let mut groups: Vec<(String, usize)> = groups.into_iter().collect();
        groups.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        Ok(Analysis {
            file: path.to_owned(),
            rows,
            skipped,
            groups,
        })
    }

    fn count(&self, pred: impl Fn(&Row) -> bool) -> f64 {
        self.rows.iter().filter(|r| pred(r)).count() as f64
    }

    fn median(&self, get: impl Fn(&Row) -> f64) -> f64 {
        let mut v: Vec<f64> = self.rows.iter().map(get).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = v.len();
        if n % 2 == 1 {
            v[n / 2]
        } else {
            (v[n / 2 - 1] + v[n / 2]) / 2.0
        }
    }

    /// Nearest-rank percentile. `median` stays separate because it
    /// interpolates the two middle values on an even count and this
    /// does not.
    fn percentile(&self, get: impl Fn(&Row) -> f64, p: f64) -> f64 {
        let mut v: Vec<f64> = self.rows.iter().map(get).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = (v.len() as f64 * p / 100.0).floor() as usize;
        v[idx.min(v.len() - 1)]
    }

    fn mean(&self, get: impl Fn(&Row) -> f64) -> f64 {
        self.rows.iter().map(get).sum::<f64>() / self.rows.len() as f64
    }

    fn max_slack(&self) -> f64 {
        self.rows.iter().map(|r| r.slack).fold(f64::MIN, f64::max)
    }
}

// ---------------------------------------------------------------
// metrics
// ---------------------------------------------------------------

/// How a metric's value and delta should be rendered, and which
/// direction counts as an improvement.
#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    /// Whole endpoints. Delta carries a relative percentage.
    Count,
    /// Nanoseconds, 3 decimals.
    Ns,
    /// Logic levels, 2 decimals.
    Levels,
    /// A percentage. Delta is in percentage points, never a
    /// percentage of a percentage.
    Pct,
}

#[derive(Serialize)]
struct Metric {
    key: &'static str,
    label: String,
    kind: Kind,
    #[serde(skip)]
    lower_is_better: Option<bool>,
    value: f64,
}

fn metrics(a: &Analysis, total_endpoints: Option<u64>) -> Vec<Metric> {
    let n = a.rows.len() as f64;
    let mut m = vec![Metric {
        key: "near_critical_endpoints",
        label: "near-critical endpoints".into(),
        kind: Kind::Count,
        lower_is_better: Some(true),
        value: n,
    }];

    // The clock group's full endpoint count is not in the CSV — it
    // comes from the intra-clock table of report_timing_summary — so
    // these two rows appear only when the caller supplies it. Without
    // them a raw count understates the gain on a design that grew.
    if let Some(total) = total_endpoints {
        m.push(Metric {
            key: "total_endpoints",
            label: "clock-group setup endpoints".into(),
            kind: Kind::Count,
            lower_is_better: None,
            value: total as f64,
        });
        m.push(Metric {
            key: "near_critical_fraction",
            label: "near-critical fraction".into(),
            kind: Kind::Pct,
            lower_is_better: Some(true),
            value: 100.0 * n / total as f64,
        });
    }

    let (lo, hi) = BANDS[0];
    m.push(Metric {
        key: "tightest_band",
        label: format!("tightest band [{lo:.3}, {hi:.3})"),
        kind: Kind::Count,
        lower_is_better: Some(true),
        value: a.count(|r| r.slack < hi),
    });
    m.push(Metric {
        key: "deep_paths",
        label: format!("deep paths (>={DEEP_LEVELS} levels)"),
        kind: Kind::Count,
        lower_is_better: Some(true),
        value: a.count(|r| r.levels >= DEEP_LEVELS),
    });
    m.push(Metric {
        key: "median_slack",
        label: "median slack".into(),
        kind: Kind::Ns,
        lower_is_better: Some(false),
        value: a.median(|r| r.slack),
    });
    m.push(Metric {
        key: "mean_logic_levels",
        label: "mean logic levels".into(),
        kind: Kind::Levels,
        lower_is_better: Some(true),
        value: a.mean(|r| r.levels as f64),
    });
    m.push(Metric {
        key: "median_logic_delay",
        label: "median logic delay".into(),
        kind: Kind::Ns,
        lower_is_better: Some(true),
        value: a.median(|r| r.logic),
    });
    m.push(Metric {
        key: "median_net_delay",
        label: "median net delay".into(),
        kind: Kind::Ns,
        lower_is_better: Some(true),
        value: a.median(|r| r.net),
    });
    // Routing as a share of each path's data delay, as a
    // distribution. No preferred direction is marked: after a
    // threshold filter this population is survivor-biased — what
    // stays near-critical is what the router could not help — so a
    // colour here would assert a verdict the number cannot support.
    m.push(Metric {
        key: "net_share_p10",
        label: "net share p10".into(),
        kind: Kind::Pct,
        lower_is_better: None,
        value: a.percentile(|r| r.pct_net, 10.0),
    });
    m.push(Metric {
        key: "median_net_share",
        label: "median net share".into(),
        kind: Kind::Pct,
        lower_is_better: None,
        value: a.median(|r| r.pct_net),
    });
    m.push(Metric {
        key: "net_share_p90",
        label: "net share p90".into(),
        kind: Kind::Pct,
        lower_is_better: None,
        value: a.percentile(|r| r.pct_net, 90.0),
    });
    m
}

/// Band edges for a run, extended with an overflow band when the CSV
/// reaches past the last fixed edge.
fn band_edges(max_slack: f64) -> Vec<(f64, f64)> {
    let mut b = BANDS.to_vec();
    let top = b.last().unwrap().1;
    if max_slack > top {
        b.push((top, max_slack));
    }
    b
}

fn band_label(lo: f64, hi: f64, last: bool) -> String {
    let close = if last { ']' } else { ')' };
    format!("[{lo:.3}, {hi:.3}{close}")
}

fn band_counts(a: &Analysis, edges: &[(f64, f64)]) -> Vec<f64> {
    edges
        .iter()
        .enumerate()
        .map(|(i, &(lo, hi))| {
            let last = i + 1 == edges.len();
            a.count(|r| {
                r.slack >= lo && (r.slack < hi || (last && r.slack <= hi))
            })
        })
        .collect()
}

fn block_counts(a: &Analysis, depth: usize) -> HashMap<String, f64> {
    let mut m: HashMap<String, f64> = HashMap::new();
    for r in &a.rows {
        *m.entry(block_of(&r.endpoint, depth)).or_default() += 1.0;
    }
    m
}

// ---------------------------------------------------------------
// rendering
// ---------------------------------------------------------------

fn commas(v: f64) -> String {
    let n = v.round() as i64;
    let neg = n < 0;
    let digits = n.abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

fn fmt_value(kind: Kind, v: f64) -> String {
    match kind {
        Kind::Count => commas(v),
        Kind::Ns => format!("{v:.3}"),
        Kind::Levels => format!("{v:.2}"),
        Kind::Pct => format!("{v:.2}%"),
    }
}

/// A change, split into the signed magnitude and the trailing
/// qualifier that belongs beside it. They are separate columns so
/// that a count's `(-37%)` cannot shove a neighbouring `+0.006` out
/// of alignment — every column is sized from its own content.
fn delta_parts(kind: Kind, before: f64, after: f64) -> (String, String) {
    let d = after - before;
    match kind {
        Kind::Count => {
            let mag = format!(
                "{}{}",
                if d < 0.0 { "-" } else { "+" },
                commas(d.abs())
            );
            let rel = if before == 0.0 {
                "(new)".to_string()
            } else {
                format!("({:+.0}%)", 100.0 * d / before)
            };
            (mag, rel)
        }
        Kind::Pct => (format!("{d:+.2}"), "pp".into()),
        Kind::Ns => (format!("{d:+.3}"), String::new()),
        Kind::Levels => (format!("{d:+.2}"), String::new()),
    }
}

/// Which way a change reads. `Plain` is for metrics with no preferred
/// direction — the survivor-biased ones especially, where a colour
/// would assert a verdict the number does not support.
#[derive(Clone, Copy, PartialEq)]
enum Tint {
    Plain,
    Good,
    Bad,
    Dim,
}

fn tint_of(delta: f64, lower_is_better: Option<bool>) -> Tint {
    match lower_is_better {
        _ if delta == 0.0 => Tint::Plain,
        Some(true) if delta < 0.0 => Tint::Good,
        Some(true) => Tint::Bad,
        Some(false) if delta > 0.0 => Tint::Good,
        Some(false) => Tint::Bad,
        None => Tint::Plain,
    }
}

fn paint(s: String, t: Tint) -> String {
    match t {
        Tint::Plain => s,
        Tint::Good => s.green().to_string(),
        Tint::Bad => s.red().to_string(),
        Tint::Dim => s.dimmed().to_string(),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Right,
}

struct Cell {
    text: String,
    tint: Tint,
}

impl Cell {
    fn new(text: impl Into<String>) -> Self {
        Cell {
            text: text.into(),
            tint: Tint::Plain,
        }
    }

    fn tinted(text: impl Into<String>, tint: Tint) -> Self {
        Cell {
            text: text.into(),
            tint,
        }
    }
}

/// A table whose column widths come from the widest cell actually in
/// each column, header included. Padding is applied to the plain text
/// and colour only afterwards, so ANSI escapes never count toward a
/// width.
struct Table {
    headers: Vec<&'static str>,
    aligns: Vec<Align>,
    rows: Vec<Vec<Cell>>,
    /// Single-space the final column. It carries a qualifier that
    /// belongs against its number (`-1.80 pp`), not a column of its
    /// own separated by a full gutter.
    tight_last: bool,
}

const GUTTER: &str = "  ";
const INDENT: &str = "  ";

impl Table {
    fn new(headers: Vec<&'static str>, aligns: Vec<Align>) -> Self {
        let tight_last = headers.len() > 2;
        Table {
            headers,
            aligns,
            rows: Vec::new(),
            tight_last,
        }
    }

    fn push(&mut self, row: Vec<Cell>) {
        self.rows.push(row);
    }

    fn widths(&self) -> Vec<usize> {
        let mut w: Vec<usize> =
            self.headers.iter().map(|h| h.chars().count()).collect();
        for row in &self.rows {
            for (i, c) in row.iter().enumerate() {
                if i < w.len() {
                    w[i] = w[i].max(c.text.chars().count());
                }
            }
        }
        w
    }

    /// The last column is never padded, so no line carries trailing
    /// whitespace inside a colour span.
    fn line(&self, cells: &[Cell], w: &[usize]) -> String {
        let mut out = String::from(INDENT);
        let n = cells.len();
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                out.push_str(if self.tight_last && i + 1 == n {
                    " "
                } else {
                    GUTTER
                });
            }
            // Right-aligned cells always pad: padding goes on the
            // left, so it can never leave trailing whitespace inside
            // a colour span. Left-aligned cells skip it in the final
            // column, where it would.
            let pad = if self.aligns[i] == Align::Right {
                format!("{:>width$}", c.text, width = w[i])
            } else if i + 1 == n {
                c.text.clone()
            } else {
                format!("{:<width$}", c.text, width = w[i])
            };
            out.push_str(&paint(pad, c.tint));
        }
        out.trim_end().to_string()
    }

    fn render(&self, title: &str) {
        println!("\n{}", title.bold());
        let w = self.widths();
        let head: Vec<Cell> =
            self.headers.iter().map(|h| Cell::new(*h)).collect();
        println!("{}", self.line(&head, &w).dimmed());
        for row in &self.rows {
            println!("{}", self.line(row, &w));
        }
    }
}

/// Keep a long hierarchy path from stretching the block column past
/// a readable line. Only the depth-2 tail of leaf registers hits this.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}\u{2026}")
}

const BLOCK_MAX: usize = 44;

// ---------------------------------------------------------------
// json
// ---------------------------------------------------------------

#[derive(Serialize)]
struct JsonRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<&'static str>,
    label: String,
    kind: Kind,
    before: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta: Option<f64>,
    /// Relative change for counts; percentage-point change is just
    /// `delta` and this stays null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pct_change: Option<f64>,
}

#[derive(Serialize)]
struct JsonFile {
    path: String,
    endpoints: usize,
    skipped_rows: usize,
    max_slack: f64,
    groups: Vec<JsonGroup>,
}

#[derive(Serialize)]
struct JsonGroup {
    group: String,
    endpoints: usize,
}

#[derive(Serialize)]
struct JsonOut {
    mode: &'static str,
    before: JsonFile,
    #[serde(skip_serializing_if = "Option::is_none")]
    after: Option<JsonFile>,
    summary: Vec<JsonRow>,
    bands: Vec<JsonRow>,
    blocks: Vec<JsonRow>,
}

fn json_file(a: &Analysis) -> JsonFile {
    JsonFile {
        path: a.file.to_string(),
        endpoints: a.rows.len(),
        skipped_rows: a.skipped,
        max_slack: a.max_slack(),
        groups: a
            .groups
            .iter()
            .map(|(g, n)| JsonGroup {
                group: g.clone(),
                endpoints: *n,
            })
            .collect(),
    }
}

fn json_row(
    key: Option<&'static str>,
    label: String,
    kind: Kind,
    before: f64,
    after: Option<f64>,
) -> JsonRow {
    let delta = after.map(|a| a - before);
    let pct_change = match (kind, delta) {
        (Kind::Count, Some(d)) if before != 0.0 => Some(100.0 * d / before),
        _ => None,
    };
    JsonRow {
        key,
        label,
        kind,
        before,
        after,
        delta,
        pct_change,
    }
}

// ---------------------------------------------------------------
// entry point
// ---------------------------------------------------------------

pub struct TimingArgs<'a> {
    pub files: &'a [Utf8PathBuf],
    pub json: bool,
    pub group: Option<&'a str>,
    pub block_depth: usize,
    pub top: usize,
    pub total_endpoints: &'a [u64],
}

pub fn run_timing(args: TimingArgs<'_>) -> Result<(), AnalyzeError> {
    if !args.total_endpoints.is_empty()
        && args.total_endpoints.len() != args.files.len()
    {
        return Err(AnalyzeError::TotalsArity {
            given: args.total_endpoints.len(),
            files: args.files.len(),
        });
    }

    let first = args.files.first().ok_or(AnalyzeError::NoFiles)?;
    let before = Analysis::load(first, args.group)?;
    let after = match args.files.get(1) {
        Some(p) => Some(Analysis::load(p, args.group)?),
        None => None,
    };
    let tot = |i: usize| args.total_endpoints.get(i).copied();

    let mb = metrics(&before, tot(0));
    let ma = after.as_ref().map(|a| metrics(a, tot(1)));

    // Band edges must be shared, or the two runs get bucketed
    // differently and the delta column is meaningless.
    let max_slack = before
        .max_slack()
        .max(after.as_ref().map(|a| a.max_slack()).unwrap_or(f64::MIN));
    let edges = band_edges(max_slack);
    let cb = band_counts(&before, &edges);
    let ca = after.as_ref().map(|a| band_counts(a, &edges));

    let bb = block_counts(&before, args.block_depth);
    let ba = after.as_ref().map(|a| block_counts(a, args.block_depth));
    let mut keys: Vec<String> = bb.keys().cloned().collect();
    if let Some(m) = &ba {
        for k in m.keys() {
            if !bb.contains_key(k) {
                keys.push(k.clone());
            }
        }
    }
    let total_of = |k: &String| {
        bb.get(k).copied().unwrap_or(0.0)
            + ba.as_ref().and_then(|m| m.get(k)).copied().unwrap_or(0.0)
    };
    keys.sort_by(|x, y| {
        total_of(y)
            .partial_cmp(&total_of(x))
            .unwrap()
            .then(x.cmp(y))
    });

    if args.json {
        let mut summary = Vec::new();
        for (i, m) in mb.iter().enumerate() {
            let av = ma.as_ref().map(|v| v[i].value);
            summary.push(json_row(
                Some(m.key),
                m.label.clone(),
                m.kind,
                m.value,
                av,
            ));
        }
        let mut bands = Vec::new();
        for (i, &(lo, hi)) in edges.iter().enumerate() {
            bands.push(json_row(
                None,
                band_label(lo, hi, i + 1 == edges.len()),
                Kind::Count,
                cb[i],
                ca.as_ref().map(|v| v[i]),
            ));
        }
        let mut blocks = Vec::new();
        for k in &keys {
            blocks.push(json_row(
                None,
                k.clone(),
                Kind::Count,
                bb.get(k).copied().unwrap_or(0.0),
                ba.as_ref().map(|m| m.get(k).copied().unwrap_or(0.0)),
            ));
        }
        let out = JsonOut {
            mode: if after.is_some() { "compare" } else { "single" },
            before: json_file(&before),
            after: after.as_ref().map(json_file),
            summary,
            bands,
            blocks,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    render(
        &before,
        after.as_ref(),
        &mb,
        ma.as_deref(),
        &edges,
        &cb,
        ca.as_deref(),
        &bb,
        ba.as_ref(),
        &keys,
        args.top,
        args.group,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render(
    before: &Analysis,
    after: Option<&Analysis>,
    mb: &[Metric],
    ma: Option<&[Metric]>,
    edges: &[(f64, f64)],
    cb: &[f64],
    ca: Option<&[f64]>,
    bb: &HashMap<String, f64>,
    ba: Option<&HashMap<String, f64>>,
    keys: &[String],
    top: usize,
    group: Option<&str>,
) {
    let cmp = after.is_some();

    // Provenance first: which files, how many endpoints, and any rows
    // that failed to parse. A truncated CSV silently shrinks every
    // count below, so it cannot be a footnote.
    println!("{}", "timing analysis".bold());
    let note = |label: &str, a: &Analysis| {
        println!(
            "{INDENT}{label:<8}{GUTTER}{}  ({} endpoints, slack < {:.3} ns)",
            a.file,
            commas(a.rows.len() as f64),
            a.max_slack()
        );
        if a.skipped > 0 {
            println!(
                "{INDENT}{:<8}{GUTTER}{} unparseable row(s) in {} — writer \
                 likely did not close the file",
                "warning".yellow(),
                a.skipped,
                a.file.file_name().unwrap_or_default()
            );
        }
    };
    if cmp {
        note("before", before);
        note("after", after.unwrap());
    } else {
        note("file", before);
    }
    if let Some(g) = group {
        println!("{INDENT}{:<8}{GUTTER}path group `{g}`", "filter");
    } else {
        // Groups from both runs, so a group that appears only in the
        // second file is still called out.
        let mut merged: HashMap<&str, usize> = HashMap::new();
        for a in [Some(before), after].into_iter().flatten() {
            for (g, n) in &a.groups {
                *merged.entry(g.as_str()).or_default() += n;
            }
        }
        if merged.len() > 1 {
            let mut gs: Vec<(&str, usize)> = merged.into_iter().collect();
            gs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            let rest: Vec<String> = gs
                .iter()
                .skip(1)
                .map(|(g, n)| format!("{g} ({n})"))
                .collect();
            println!(
                "{INDENT}{:<8}{GUTTER}{} path groups mixed; --group \
                 restricts to one",
                "note",
                gs.len()
            );
            println!(
                "{INDENT}{:<8}{GUTTER}{} ({}), {}",
                "",
                gs[0].0,
                commas(gs[0].1 as f64),
                rest.join(", ")
            );
        }
    }

    let count_cols = || {
        (
            vec!["", "before", "after", "delta", ""],
            vec![
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Left,
            ],
        )
    };
    let single_cols = |value: &'static str| {
        (vec!["", value], vec![Align::Left, Align::Right])
    };

    // ---- summary ----
    let (mut h, al) = if cmp {
        count_cols()
    } else {
        single_cols("value")
    };
    h[0] = "metric";
    let mut t = Table::new(h, al);
    for (i, m) in mb.iter().enumerate() {
        match ma {
            Some(v) => {
                let a = v[i].value;
                let d = a - m.value;
                let tn = tint_of(d, m.lower_is_better);
                let (mag, qual) = delta_parts(m.kind, m.value, a);
                t.push(vec![
                    Cell::new(m.label.clone()),
                    Cell::new(fmt_value(m.kind, m.value)),
                    Cell::new(fmt_value(m.kind, a)),
                    Cell::tinted(mag, tn),
                    Cell::tinted(qual, tn),
                ]);
            }
            None => t.push(vec![
                Cell::new(m.label.clone()),
                Cell::new(fmt_value(m.kind, m.value)),
            ]),
        }
    }
    t.render("summary");

    // ---- bands ----
    let (mut h, al) = if cmp {
        count_cols()
    } else {
        single_cols("endpoints")
    };
    h[0] = "band";
    let mut t = Table::new(h, al);
    let mut push_count = |label: Cell, b: f64, a: Option<f64>| match a {
        Some(a) => {
            let tn = tint_of(a - b, Some(true));
            let (mag, qual) = delta_parts(Kind::Count, b, a);
            t.push(vec![
                label,
                Cell::new(commas(b)),
                Cell::new(commas(a)),
                Cell::tinted(mag, tn),
                Cell::tinted(qual, tn),
            ]);
        }
        None => t.push(vec![label, Cell::new(commas(b))]),
    };
    for (i, &(lo, hi)) in edges.iter().enumerate() {
        let label = band_label(lo, hi, i + 1 == edges.len());
        push_count(Cell::new(label), cb[i], ca.map(|v| v[i]));
    }
    push_count(
        Cell::tinted("TOTAL", Tint::Dim),
        cb.iter().sum(),
        ca.map(|v| v.iter().sum()),
    );
    t.render("slack bands");

    // ---- blocks ----
    let (mut h, al) = if cmp {
        count_cols()
    } else {
        single_cols("endpoints")
    };
    h[0] = "block";
    let mut t = Table::new(h, al);
    for k in keys.iter().take(top) {
        let b = bb.get(k).copied().unwrap_or(0.0);
        let label = Cell::new(ellipsize(k, BLOCK_MAX));
        match ba {
            Some(m) => {
                let a = m.get(k).copied().unwrap_or(0.0);
                let tn = tint_of(a - b, Some(true));
                let (mag, qual) = delta_parts(Kind::Count, b, a);
                t.push(vec![
                    label,
                    Cell::new(commas(b)),
                    Cell::new(commas(a)),
                    Cell::tinted(mag, tn),
                    Cell::tinted(qual, tn),
                ]);
            }
            None => t.push(vec![label, Cell::new(commas(b))]),
        }
    }
    let title = if keys.len() > top {
        format!("endpoint blocks (top {top} of {})", keys.len())
    } else {
        "endpoint blocks".to_string()
    };
    t.render(&title);
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_split() {
        assert_eq!(endpoint_of("{a/b/C --> x/y/D}"), "x/y/D");
        assert_eq!(endpoint_of("a/b/C --> x/y/D"), "x/y/D");
        // No arrow: the whole label is the best guess available.
        assert_eq!(endpoint_of("{x/y/D}"), "x/y/D");
    }

    #[test]
    fn block_depth() {
        assert_eq!(block_of("a/b/c/d", 2), "a/b");
        assert_eq!(block_of("a", 2), "a");
    }

    #[test]
    fn row_parses_right_to_left() {
        let line = "{p/q/R[1]/C --> p/s/T[2]/D},0.000,4,2.198,0.349,\
                    1.849,84.1,-0.140,0.084,clkout1_primitive_1";
        let r = parse_row(line, 2).expect("parses");
        assert_eq!(r.endpoint, "p/s/T[2]/D");
        assert_eq!(r.levels, 4);
        assert_eq!(r.group, "clkout1_primitive_1");
        assert!((r.slack - 0.0).abs() < 1e-9);
        assert!((r.pct_net - 84.1).abs() < 1e-9);
    }

    #[test]
    fn truncated_row_is_skipped_not_guessed() {
        assert!(parse_row("{p/q/R --> p/s/T", 2).is_none());
    }

    #[test]
    fn commas_group_thousands() {
        assert_eq!(commas(25665.0), "25,665");
        assert_eq!(commas(117.0), "117");
        assert_eq!(commas(-9539.0), "-9,539");
        assert_eq!(commas(0.0), "0");
    }

    #[test]
    fn ellipsize_only_past_the_cap() {
        assert_eq!(ellipsize("abc", 5), "abc");
        assert_eq!(ellipsize("abcdefg", 5), "abcd\u{2026}");
    }

    #[test]
    fn columns_size_to_content_not_constants() {
        let mut t = Table::new(vec!["a", "b"], vec![Align::Left, Align::Right]);
        t.push(vec![Cell::new("xx"), Cell::new("1")]);
        t.push(vec![Cell::new("yyyy"), Cell::new("22222")]);
        let w = t.widths();
        assert_eq!(w, vec![4, 5]);
        // Right-aligned final column still pads; no trailing space.
        let line = t.line(&t.rows[0], &w);
        assert_eq!(line, "  xx        1");
        assert!(!line.ends_with(' '));
    }

    fn fixture(shares: &[f64]) -> Analysis {
        Analysis {
            file: Utf8PathBuf::from("t.csv"),
            rows: shares
                .iter()
                .map(|&p| Row {
                    endpoint: "a/b/C".into(),
                    slack: 0.1,
                    levels: 4,
                    logic: 0.4,
                    net: 1.6,
                    pct_net: p,
                    group: "g".into(),
                })
                .collect(),
            skipped: 0,
            groups: vec![("g".into(), shares.len())],
        }
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        let a = fixture(&v);
        assert_eq!(a.percentile(|r| r.pct_net, 10.0), 11.0);
        assert_eq!(a.percentile(|r| r.pct_net, 90.0), 91.0);
        // Never indexes past the end at p100.
        assert_eq!(a.percentile(|r| r.pct_net, 100.0), 100.0);
    }

    #[test]
    fn net_share_reported_as_a_distribution() {
        // The rows that replaced the >75% / <50% threshold pair:
        // three points on one distribution, no unnamed band between.
        let a = fixture(&[70.0, 74.0, 79.0, 80.0, 85.0]);
        let m = metrics(&a, None);
        let keys: Vec<&str> = m.iter().map(|x| x.key).collect();
        assert!(keys.contains(&"net_share_p10"));
        assert!(keys.contains(&"median_net_share"));
        assert!(keys.contains(&"net_share_p90"));
        assert!(!keys.contains(&"route_limited"));
        assert!(!keys.contains(&"logic_limited"));
        // All three are descriptive, never coloured as good or bad.
        for k in ["net_share_p10", "median_net_share", "net_share_p90"] {
            let row = m.iter().find(|x| x.key == k).unwrap();
            assert_eq!(row.lower_is_better, None);
        }
    }

    #[test]
    fn count_delta_splits_magnitude_from_qualifier() {
        assert_eq!(
            delta_parts(Kind::Count, 25665.0, 16126.0),
            ("-9,539".to_string(), "(-37%)".to_string())
        );
        assert_eq!(
            delta_parts(Kind::Count, 0.0, 373.0),
            ("+373".to_string(), "(new)".to_string())
        );
        // Percentages move in points, never percent-of-percent.
        assert_eq!(
            delta_parts(Kind::Pct, 84.23, 74.77),
            ("-9.46".to_string(), "pp".to_string())
        );
    }

    #[test]
    fn overflow_band_added_only_when_needed() {
        assert_eq!(band_edges(0.200).len(), BANDS.len());
        assert_eq!(band_edges(0.500).len(), BANDS.len() + 1);
    }
}
