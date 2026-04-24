// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Plot generation from Xyce `.prn` output.
//!
//! Reads `@plot` directives from SPICE netlists and generates PNGs.

use plotters::prelude::*;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::VwError;

const BG: RGBColor = RGBColor(24, 24, 32);
const GRID: RGBColor = RGBColor(60, 60, 72);
const GRID_FINE: RGBColor = RGBColor(40, 40, 50);
const TEXT: RGBColor = RGBColor(200, 200, 210);
const EYE_COLOR: RGBColor = RGBColor(0, 255, 128);

const TRACE_COLORS: [RGBColor; 6] = [
    RGBColor(80, 200, 255),
    RGBColor(255, 100, 120),
    RGBColor(120, 255, 120),
    RGBColor(255, 200, 80),
    RGBColor(200, 120, 255),
    RGBColor(255, 160, 200),
];

/// Parsed Xyce `.prn` data with named columns.
pub struct PrnData {
    pub time: Vec<f64>,
    pub columns: HashMap<String, Vec<f64>>,
    pub column_order: Vec<String>,
}

/// A plot directive parsed from a SPICE netlist comment.
pub struct PlotDirective {
    pub plot_type: PlotType,
    pub signals: Vec<String>,
    pub ui: Option<f64>,
    pub label: Option<String>,
}

pub enum PlotType {
    Timeseries,
    Eye,
}

/// Parse a Xyce `.prn` file into named columns.
pub fn parse_prn(path: &Path) -> Result<PrnData, VwError> {
    let content =
        fs::read_to_string(path).map_err(|e| VwError::Simulation {
            message: format!(
                "Failed to read .prn file {}: {e}",
                path.display()
            ),
        })?;

    let mut lines = content.lines();

    // Parse header line to get column names
    let header = lines.next().ok_or_else(|| VwError::Simulation {
        message: "Empty .prn file".to_string(),
    })?;
    let col_names: Vec<String> =
        header.split_whitespace().map(String::from).collect();

    // First column is typically the index, second is TIME
    let mut time = Vec::new();
    let mut columns: HashMap<String, Vec<f64>> = HashMap::new();
    let mut column_order = Vec::new();

    // Skip first column (index) and second (time), rest are signals
    for name in col_names.iter().skip(2) {
        let normalized = name.to_uppercase();
        columns.insert(normalized.clone(), Vec::new());
        column_order.push(normalized);
    }

    for line in lines {
        if line.starts_with("End") || line.trim().is_empty() {
            break;
        }
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 3 {
            continue;
        }

        let t: f64 = cols[1].parse().unwrap_or(0.0);
        time.push(t);

        for (i, name) in column_order.iter().enumerate() {
            let val: f64 =
                cols.get(i + 2).and_then(|s| s.parse().ok()).unwrap_or(0.0);
            if let Some(col) = columns.get_mut(name) {
                col.push(val);
            }
        }
    }

    Ok(PrnData {
        time,
        columns,
        column_order,
    })
}

/// Parse `@plot` directives from a SPICE netlist.
///
/// Format: `* @plot <type> <signal> [<signal> ...] [key=value ...]`
pub fn parse_plot_directives(
    netlist_path: &Path,
) -> Result<Vec<PlotDirective>, VwError> {
    let content =
        fs::read_to_string(netlist_path).map_err(|e| VwError::Simulation {
            message: format!(
                "Failed to read netlist {}: {e}",
                netlist_path.display()
            ),
        })?;

    let mut directives = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('*') {
            continue;
        }
        let comment = trimmed.trim_start_matches('*').trim();
        if !comment.starts_with("@plot") {
            continue;
        }

        let rest = comment.strip_prefix("@plot").unwrap().trim();
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }

        let plot_type = match tokens[0].to_lowercase().as_str() {
            "timeseries" => PlotType::Timeseries,
            "eye" => PlotType::Eye,
            _ => continue,
        };

        let mut signals = Vec::new();
        let mut ui = None;
        let mut label = None;

        for token in &tokens[1..] {
            if let Some(val) = token.strip_prefix("ui=") {
                ui = parse_time_value(val);
            } else if let Some(val) = token.strip_prefix("label=") {
                label = Some(val.trim_matches('"').to_string());
            } else {
                signals.push(token.to_string());
            }
        }

        directives.push(PlotDirective {
            plot_type,
            signals,
            ui,
            label,
        });
    }

    Ok(directives)
}

/// Pick a time unit and scale factor appropriate for the data range.
/// Returns (scale, unit_label) where scaled_time = time_seconds * scale.
fn auto_time_unit(t_min: f64, t_max: f64) -> (f64, &'static str) {
    let span = t_max - t_min;
    if span >= 1e-3 {
        (1e3, "ms")
    } else if span >= 1e-6 {
        (1e6, "us")
    } else if span >= 1e-9 {
        (1e9, "ns")
    } else {
        (1e12, "ps")
    }
}

/// Parse a time value like "10ns", "37.647ps", or "10e-9".
fn parse_time_value(s: &str) -> Option<f64> {
    if let Some(v) = s.strip_suffix("ps") {
        v.parse::<f64>().ok().map(|v| v * 1e-12)
    } else if let Some(v) = s.strip_suffix("ns") {
        v.parse::<f64>().ok().map(|v| v * 1e-9)
    } else if let Some(v) = s.strip_suffix("us") {
        v.parse::<f64>().ok().map(|v| v * 1e-6)
    } else if let Some(v) = s.strip_suffix("ms") {
        v.parse::<f64>().ok().map(|v| v * 1e-3)
    } else {
        s.parse::<f64>().ok()
    }
}

/// Generate all plots from `@plot` directives in the netlist.
pub fn generate_plots(
    netlist_path: &Path,
    prn_path: &Path,
    output_dir: &Path,
) -> Result<(), VwError> {
    let data = parse_prn(prn_path)?;
    let directives = parse_plot_directives(netlist_path)?;

    if directives.is_empty() {
        return Ok(());
    }

    for (i, directive) in directives.iter().enumerate() {
        match directive.plot_type {
            PlotType::Timeseries => {
                let filename = if directives.len() == 1 {
                    "timeseries.png".to_string()
                } else {
                    format!("timeseries_{i}.png")
                };
                let out_path = output_dir.join(&filename);
                generate_timeseries(
                    &data,
                    &directive.signals,
                    directive.label.as_deref(),
                    out_path.to_str().unwrap(),
                )?;
            }
            PlotType::Eye => {
                let filename = if directives.len() == 1 {
                    "eye.png".to_string()
                } else {
                    format!("eye_{i}.png")
                };
                let out_path = output_dir.join(&filename);
                let ui = directive.ui.unwrap_or(10e-9);
                generate_eye(
                    &data,
                    &directive.signals,
                    ui,
                    directive.label.as_deref(),
                    out_path.to_str().unwrap(),
                )?;
            }
        }
    }

    Ok(())
}

/// Evaluate a signal expression from the .prn data.
/// Handles simple expressions like "V(OUT_P)" or "V(OUT_P)-V(OUT_N)".
fn eval_signal(data: &PrnData, expr: &str) -> Option<Vec<f64>> {
    let expr_upper = expr.to_uppercase();

    // Direct column lookup
    if let Some(col) = data.columns.get(&expr_upper) {
        return Some(col.clone());
    }

    // Handle difference expression like "V(A)-V(B)"
    if let Some(pos) = expr_upper.find('-') {
        let left = &expr_upper[..pos];
        let right = &expr_upper[pos + 1..];
        if let (Some(l), Some(r)) =
            (data.columns.get(left), data.columns.get(right))
        {
            return Some(l.iter().zip(r).map(|(a, b)| a - b).collect());
        }
    }

    None
}

fn generate_timeseries(
    data: &PrnData,
    signals: &[String],
    label: Option<&str>,
    out_path: &str,
) -> Result<(), VwError> {
    let root = BitMapBackend::new(out_path, (1600, 800)).into_drawing_area();
    root.fill(&BG).unwrap();

    if data.time.is_empty() {
        return Ok(());
    }

    let t_min = data.time[0];
    let t_max = *data.time.last().unwrap();
    let (t_scale, t_unit) = auto_time_unit(t_min, t_max);
    let t_min_scaled = t_min * t_scale;
    let t_max_scaled = t_max * t_scale;

    // Evaluate all signals and find voltage range
    let mut evaluated: Vec<(String, Vec<f64>)> = Vec::new();
    for sig in signals {
        if let Some(vals) = eval_signal(data, sig) {
            evaluated.push((sig.clone(), vals));
        }
    }

    if evaluated.is_empty() {
        return Ok(());
    }

    let v_min = evaluated
        .iter()
        .flat_map(|(_, v)| v.iter())
        .cloned()
        .fold(f64::INFINITY, f64::min);
    let v_max = evaluated
        .iter()
        .flat_map(|(_, v)| v.iter())
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);
    let margin = (v_max - v_min) * 0.1;

    let title = label.unwrap_or("Timeseries");

    let mut chart = ChartBuilder::on(&root)
        .caption(title, ("monospace", 28).into_font().color(&TEXT))
        .margin(20)
        .x_label_area_size(45)
        .y_label_area_size(70)
        .build_cartesian_2d(
            t_min_scaled..t_max_scaled,
            (v_min - margin)..(v_max + margin),
        )
        .unwrap();

    chart
        .configure_mesh()
        .bold_line_style(GRID.stroke_width(1))
        .light_line_style(GRID_FINE)
        .axis_style(GRID)
        .x_labels(12)
        .y_labels(10)
        .x_label_style(("monospace", 14).into_font().color(&TEXT))
        .y_label_style(("monospace", 14).into_font().color(&TEXT))
        .x_label_formatter(&|x| format!("{x:.1} {t_unit}"))
        .y_label_formatter(&|y| format!("{y:.2} V"))
        .x_desc("Time")
        .y_desc("Voltage")
        .axis_desc_style(("monospace", 16).into_font().color(&TEXT))
        .draw()
        .unwrap();

    for (i, (name, vals)) in evaluated.iter().enumerate() {
        let color = TRACE_COLORS[i % TRACE_COLORS.len()];
        let name_owned = name.clone();
        chart
            .draw_series(LineSeries::new(
                data.time.iter().zip(vals).map(|(&t, &v)| (t * t_scale, v)),
                color.stroke_width(2),
            ))
            .unwrap()
            .label(&name_owned)
            .legend(move |(x, y)| {
                PathElement::new(
                    vec![(x, y), (x + 15, y)],
                    color.stroke_width(2),
                )
            });
    }

    chart
        .configure_series_labels()
        .position(SeriesLabelPosition::UpperRight)
        .margin(10)
        .background_style(BG.mix(0.9))
        .border_style(GRID)
        .label_font(("monospace", 16).into_font().color(&TEXT))
        .draw()
        .unwrap();

    root.present().unwrap();
    println!("wrote {out_path}");
    Ok(())
}

fn generate_eye(
    data: &PrnData,
    signals: &[String],
    ui: f64,
    label: Option<&str>,
    out_path: &str,
) -> Result<(), VwError> {
    let root = BitMapBackend::new(out_path, (1000, 800)).into_drawing_area();
    root.fill(&BG).unwrap();

    if data.time.is_empty() || signals.is_empty() {
        return Ok(());
    }

    // Use first signal expression for the eye
    let v_diff = match eval_signal(data, &signals[0]) {
        Some(v) => v,
        None => return Ok(()),
    };

    let v_min = v_diff.iter().cloned().fold(f64::INFINITY, f64::min);
    let v_max = v_diff.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let margin = (v_max - v_min) * 0.1;

    let eye_width = 2.0 * ui;
    let (t_scale, t_unit) = auto_time_unit(0.0, eye_width);
    let title = label.unwrap_or("Eye Diagram");

    let mut chart = ChartBuilder::on(&root)
        .caption(title, ("monospace", 24).into_font().color(&TEXT))
        .margin(20)
        .x_label_area_size(45)
        .y_label_area_size(70)
        .build_cartesian_2d(
            0.0..(eye_width * t_scale),
            (v_min - margin)..(v_max + margin),
        )
        .unwrap();

    chart
        .configure_mesh()
        .bold_line_style(GRID.stroke_width(1))
        .light_line_style(GRID_FINE)
        .axis_style(GRID)
        .x_labels(8)
        .y_labels(10)
        .x_label_style(("monospace", 14).into_font().color(&TEXT))
        .y_label_style(("monospace", 14).into_font().color(&TEXT))
        .x_label_formatter(&|x| format!("{x:.1} {t_unit}"))
        .y_label_formatter(&|y| format!("{y:.2} V"))
        .x_desc("Time (modulo 2 UI)")
        .y_desc("V(diff)")
        .axis_desc_style(("monospace", 16).into_font().color(&TEXT))
        .draw()
        .unwrap();

    let trace_style = ShapeStyle {
        color: EYE_COLOR.mix(0.4),
        filled: false,
        stroke_width: 1,
    };

    let t_start = data.time[0];
    let mut slice_start = t_start;
    let t_end = *data.time.last().unwrap();
    while slice_start + eye_width <= t_end {
        let points: Vec<(f64, f64)> = data
            .time
            .iter()
            .zip(&v_diff)
            .filter(|(&t, _)| t >= slice_start && t <= slice_start + eye_width)
            .map(|(&t, &v)| ((t - slice_start) * t_scale, v))
            .collect();

        if points.len() >= 2 {
            chart
                .draw_series(LineSeries::new(points, trace_style))
                .unwrap();
        }

        slice_start += ui;
    }

    root.present().unwrap();
    println!("wrote {out_path}");
    Ok(())
}
