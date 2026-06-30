// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Overlay popups attached to the input editor — completion (slice 4),
//! signature help (slice 5), hover (slice 6).
//!
//! Each popup is a [`PopupState`] variant held on the App as
//! `Option<PopupState>`. The key handler in
//! [`crate::app::App::handle_terminal_event`] intercepts navigation
//! and dismissal keys when a popup is active, BEFORE the catch-all
//! editor handoff — so Up/Down/Enter/Esc go to the popup rather than
//! moving the text cursor.
//!
//! Popups render in [`crate::ui::draw`] as an additional pass on top
//! of the input editor; anchor coordinates are derived from the
//! cursor's screen position so they appear next to where the user is
//! typing.
//!
//! Coexistence: only one popup is active at a time. Completion
//! takes precedence over signature help; both dismiss when hover
//! opens; any popup dismisses on Esc.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph,
};
use ratatui::Frame;
use vw_htcl::complete::{Completion, CompletionKind};

/// One of the popup kinds the input editor can show.
#[derive(Debug)]
pub enum PopupState {
    Completion(CompletionPopup),
    SignatureHelp(SignatureHelpPopup),
    Hover(HoverPopup),
    Help(HelpPopup),
    /// Fuzzy symbol picker (Ctrl-T). The picker owns its own
    /// matcher state and rendering; this enum just multiplexes
    /// into [`crate::symbol_search`].
    SymbolSearch(crate::symbol_search::SymbolPicker),
}

/// Keybinding cheat-sheet shown by Ctrl-H. Lists every key chord the
/// REPL responds to, with a short description, so users don't have
/// to read the source to discover features. Any key dismisses.
#[derive(Debug)]
pub struct HelpPopup;

/// Per-keystroke signature help — shows the proc's args + active
/// parameter while the user is typing arg values. Owns its strings
/// so we can store it on the App across frames without borrowing
/// from any short-lived `vw_htcl` document. Recomputed on every
/// buffer-mutating keystroke.
#[derive(Clone, Debug)]
pub struct SignatureHelpPopup {
    pub proc_name: String,
    pub args: Vec<SigHelpArg>,
    pub return_type: Option<String>,
    pub doc_brief: Option<String>,
    /// Index into `args` of the parameter under the cursor.
    pub active: Option<usize>,
    /// Cursor cell where the popup should anchor (above the cursor
    /// — the renderer flips to below if there's no room above).
    pub anchor: (u16, u16),
    /// First arg index to show — wide-signature procs
    /// (`create_cpm5_*` has 50+ args) overflow the popup's vertical
    /// budget and get truncated. Ctrl-↑ / Ctrl-↓ adjusts this so
    /// the user can scroll through the full list. Preserved across
    /// `refresh_signature_help` rebuilds (App reads the old popup's
    /// value before overwriting) so manual scrolling sticks while
    /// the user is typing.
    pub scroll_offset: usize,
}

#[derive(Clone, Debug)]
pub struct SigHelpArg {
    pub name: String,
    pub type_str: Option<String>,
    /// Pre-formatted `@default(...)` value when the arg declares
    /// one. Already truncated by the caller (see
    /// `crate::app::format_default_value`) to a sensible width so
    /// the popup doesn't blow wide on multi-KB paired-dict
    /// defaults from generated IP wrappers.
    pub default_str: Option<String>,
}

/// Hover popup — Ctrl-K opens it, any keystroke dismisses. Shows the
/// proc/var the cursor sits on, with full doc-comment body when
/// available. Distinct from signature help: hover is explicit + shows
/// the FULL docs; sig help is auto + shows a one-line brief.
#[derive(Clone, Debug)]
pub struct HoverPopup {
    /// First line — usually the proc signature or `$variable: type`.
    pub title: String,
    /// Full doc body, reflowed. May be empty.
    pub body: String,
    pub anchor: (u16, u16),
}

/// Render the hover popup at the cursor anchor. Sized to fit the
/// content, capped to ~70% of the frame so a long doc doesn't
/// swallow the whole screen.
pub fn draw_hover_popup(f: &mut Frame, popup: &HoverPopup, frame_area: Rect) {
    let max_width = ((frame_area.width as usize) * 7 / 10).max(40);
    let max_height = ((frame_area.height as usize) * 6 / 10).max(8);

    // Wrap body lines to fit max_width-2 (border padding).
    let inner_w = max_width.saturating_sub(2);
    let mut body_lines: Vec<String> = Vec::new();
    for paragraph in popup.body.split('\n') {
        if paragraph.is_empty() {
            body_lines.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if line.chars().count() + word.chars().count() + 1 > inner_w {
                body_lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            body_lines.push(line);
        }
    }
    let total_lines = 1 + body_lines.len(); // title + body
    let height = (total_lines + 2).min(max_height) as u16;
    let width = max_width as u16;

    let (anchor_x, anchor_y) = popup.anchor;
    let y = if anchor_y + height < frame_area.y + frame_area.height {
        (anchor_y + 1)
            .min(frame_area.y + frame_area.height.saturating_sub(height))
    } else if anchor_y >= frame_area.y + height {
        anchor_y.saturating_sub(height)
    } else {
        frame_area.y
    };
    let x = anchor_x.min(frame_area.x + frame_area.width.saturating_sub(width));
    let area = Rect {
        x,
        y,
        width: width.min(frame_area.width),
        height,
    };
    f.render_widget(Clear, area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        popup.title.clone(),
        Style::default()
            .fg(Color::Rgb(230, 200, 120))
            .add_modifier(Modifier::BOLD),
    )));
    for body in body_lines {
        lines.push(Line::from(Span::styled(
            body,
            Style::default().fg(Color::Gray),
        )));
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" hover — any key to dismiss "),
    );
    f.render_widget(para, area);
}

/// Render the signature help popup as an overlay anchored just above
/// the cursor cell. If there's not enough vertical room above, the
/// renderer flips it below.
pub fn draw_signature_help_popup(
    f: &mut Frame,
    popup: &SignatureHelpPopup,
    frame_area: Rect,
) {
    // Multi-line layout: proc name on line 1, then one indented
    // arg per line, then `→ return_type` (when annotated), then an
    // optional doc-brief line.
    //
    // Single-line layouts blew past the popup width on procs with
    // many args — `create_versal_cips` has ~20 — and ratatui
    // truncates beyond the box. One-arg-per-line keeps each arg
    // readable and scales naturally. The active-arg highlighting
    // (bold + underline on the arg's name) still works per-line.
    let name_style_keyword = Style::default()
        .fg(Color::Rgb(230, 200, 120))
        .add_modifier(Modifier::BOLD);
    let arg_name_style_default = Style::default().fg(Color::Gray);
    let arg_name_style_active = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let type_style = Style::default().fg(Color::Rgb(100, 200, 200));
    let dim = Style::default().add_modifier(Modifier::DIM);
    let default_style = Style::default()
        .fg(Color::Rgb(180, 200, 130))
        .add_modifier(Modifier::DIM);

    // Build the proc name line (always visible — never scrolled
    // off) and the body lines (arg lines + return + doc) as
    // separate vectors so we can apply scroll_offset to the body
    // independently of the title.
    let name_line =
        Line::from(Span::styled(popup.proc_name.clone(), name_style_keyword));
    let mut body_lines: Vec<Line<'static>> = Vec::new();
    for (i, arg) in popup.args.iter().enumerate() {
        let active = popup.active == Some(i);
        let mut spans: Vec<Span<'static>> = vec![Span::raw("  ")];
        let name_style = if active {
            arg_name_style_active
        } else {
            arg_name_style_default
        };
        spans.push(Span::styled(format!("-{}", arg.name), name_style));
        if let Some(ty) = arg.type_str.as_deref() {
            spans.push(Span::styled(": ".to_string(), dim));
            spans.push(Span::styled(ty.to_string(), type_style));
        }
        if let Some(default) = arg.default_str.as_deref() {
            spans.push(Span::styled(" = ".to_string(), dim));
            spans.push(Span::styled(default.to_string(), default_style));
        }
        body_lines.push(Line::from(spans));
    }
    if let Some(ret) = popup.return_type.as_deref() {
        body_lines.push(Line::from(vec![
            Span::styled("  → ".to_string(), dim),
            Span::styled(ret.to_string(), type_style),
        ]));
    }
    if let Some(brief) = popup.doc_brief.as_deref() {
        if !brief.is_empty() {
            body_lines.push(Line::from(""));
            body_lines.push(Line::from(Span::styled(
                brief.to_string(),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    // Body budget: total height budget minus borders, the proc
    // name row, and (when scrolled / overflowing) one row for the
    // "(N more · ctrl-↑/↓ to scroll)" footer hint.
    let max_height_rows = ((frame_area.height as usize) * 7 / 10).max(4);
    let chrome_rows = 2 /* borders */ + 1 /* proc name */;
    let max_body_rows = max_height_rows.saturating_sub(chrome_rows);

    // Apply scroll_offset, clamping so we never scroll past a point
    // where the visible window would underfill: the user can scroll
    // until the LAST body line is at the bottom of the window.
    let total_body = body_lines.len();
    let needs_scroll_hint = total_body > max_body_rows;
    let visible_body_rows = if needs_scroll_hint {
        max_body_rows.saturating_sub(1) // reserve one row for the hint
    } else {
        max_body_rows
    };
    let max_offset = total_body.saturating_sub(visible_body_rows);
    let scroll_offset = popup.scroll_offset.min(max_offset);
    let body_end = (scroll_offset + visible_body_rows).min(total_body);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(
        1 + (body_end - scroll_offset) + needs_scroll_hint as usize,
    );
    lines.push(name_line);
    lines.extend(body_lines[scroll_offset..body_end].iter().cloned());
    if needs_scroll_hint {
        let visible_lo = scroll_offset + 1;
        let visible_hi = body_end;
        let hint = format!(
            "  … showing {visible_lo}-{visible_hi} of {total_body} \
             · shift-↑/shift-↓ to scroll",
        );
        lines.push(Line::from(Span::styled(
            hint,
            Style::default().add_modifier(Modifier::DIM),
        )));
    }

    // Width: longest line + borders, capped at 70% of frame so
    // wide types don't push the popup off-screen.
    let widest: usize = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
        })
        .max()
        .unwrap_or(20);
    let max_width = ((frame_area.width as usize) * 7 / 10).max(40);
    let width = ((widest + 2).clamp(20, max_width)) as u16;
    // Recompute height now that the line list is bounded.
    let height = ((lines.len() + 2) as u16)
        .min(frame_area.height.saturating_sub(1).max(3));

    let (anchor_x, anchor_y) = popup.anchor;
    // Anchor ABOVE cursor when room exists (don't fight the completion
    // popup which anchors below). Falls back to below, then clamps to
    // the frame bottom — never lets the rect extend past the buffer
    // (would panic inside ratatui's `render_widget`).
    let y_above = anchor_y.saturating_sub(height);
    let y_below = anchor_y.saturating_add(1);
    let y = if anchor_y >= frame_area.y + height {
        y_above
    } else {
        y_below
    };
    let max_y = frame_area
        .y
        .saturating_add(frame_area.height)
        .saturating_sub(height);
    let y = y.min(max_y).max(frame_area.y);
    let x = anchor_x.min(frame_area.x + frame_area.width.saturating_sub(width));
    let area = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, area);
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" signature "));
    f.render_widget(para, area);
}

/// One row in the cheat-sheet. `keys` is shown left-aligned in its
/// own column; `description` fills the rest of the row.
struct HelpRow {
    keys: &'static str,
    description: &'static str,
}

/// The full keybinding + meta-command catalog. Keep in sync with
/// `crate::app::App::handle_terminal_event` (key chords) and
/// `crate::app::META_COMMANDS` (the `:foo` REPL commands) — every
/// recognized chord and meta-command should appear here so the
/// help is authoritative.
///
/// Rows are grouped with blank-spacer rows between sections; the
/// renderer treats `("", "")` as a section separator.
const HELP_ROWS: &[HelpRow] = &[
    // --- popups + completion ---
    HelpRow {
        keys: "Ctrl-H",
        description: "show this help",
    },
    HelpRow {
        keys: "Tab",
        description: "open completion popup (procs, flags, :commands)",
    },
    HelpRow {
        keys: "↑ / ↓ / Enter",
        description: "navigate / accept inside any popup",
    },
    HelpRow {
        keys: "Esc",
        description: "dismiss popup or reverse search",
    },
    HelpRow {
        keys: "",
        description: "",
    },
    // --- input / submit ---
    HelpRow {
        keys: "Enter",
        description: "submit input (newline if parse incomplete)",
    },
    HelpRow {
        keys: "Shift+Enter, Alt+Enter",
        description: "insert literal newline",
    },
    HelpRow {
        keys: "Ctrl-P / Ctrl-N",
        description: "prev / next in input history",
    },
    HelpRow {
        keys: "Ctrl-R",
        description: "reverse-search history",
    },
    HelpRow {
        keys: "Ctrl-C",
        description: "clear current input",
    },
    HelpRow {
        keys: "",
        description: "",
    },
    // --- scrollback ---
    HelpRow {
        keys: "Ctrl-K, PageUp",
        description: "scroll scrollback up",
    },
    HelpRow {
        keys: "Ctrl-J, PageDown",
        description: "scroll scrollback down",
    },
    HelpRow {
        keys: "End, Ctrl-G",
        description: "jump to bottom (resume tail-follow)",
    },
    HelpRow {
        keys: "Mouse wheel",
        description: "scroll scrollback (mouse capture must be on)",
    },
    HelpRow {
        keys: "Mouse drag",
        description: "select for clipboard copy (auto-scrolls past edges)",
    },
    HelpRow {
        keys: "F2",
        description: "toggle mouse capture (mouse-app vs terminal-native)",
    },
    HelpRow {
        keys: "",
        description: "",
    },
    // --- session / exit ---
    HelpRow {
        keys: "Ctrl-D",
        description: "exit REPL",
    },
    HelpRow {
        keys: ":quit / :q / :exit",
        description: "exit REPL via meta-command",
    },
    HelpRow {
        keys: ":load <path>",
        description: "evaluate the contents of a file in this session",
    },
    HelpRow {
        keys: ":libs",
        description: "list loaded libraries and their symbol counts",
    },
    HelpRow {
        keys: ":restart",
        description:
            "restart the Vivado worker (stubbed — not yet implemented)",
    },
    HelpRow {
        keys: "",
        description: "",
    },
    HelpRow {
        keys: "(auto)",
        description: "signature help shows while you type a call's args",
    },
    HelpRow {
        keys: "Shift-↑ / Shift-↓",
        description: "scroll the signature-help popup (long arg lists)",
    },
    HelpRow {
        keys: "Ctrl-Y",
        description: "hover docs under cursor",
    },
    HelpRow {
        keys: "Ctrl-S",
        description: "fuzzy symbol search (Tab toggles libraries view)",
    },
];

/// Render the help modal centered in the frame. Width fits the
/// longest row + small padding; height fits all rows + borders.
pub fn draw_help_popup(f: &mut Frame, frame_area: Rect) {
    let key_col_w: usize = HELP_ROWS
        .iter()
        .map(|r| r.keys.chars().count())
        .max()
        .unwrap_or(8);
    let desc_col_w: usize = HELP_ROWS
        .iter()
        .map(|r| r.description.chars().count())
        .max()
        .unwrap_or(20);
    // +4 = " │ " separator + outer 1-cell padding on each side.
    let width = ((key_col_w + desc_col_w + 6).clamp(40, 80)) as u16;
    let height = (HELP_ROWS.len() + 2) as u16; // +2 borders
    let x = frame_area.x + frame_area.width.saturating_sub(width) / 2;
    let y = frame_area.y + frame_area.height.saturating_sub(height) / 2;
    let area = Rect {
        x,
        y,
        width: width.min(frame_area.width),
        height: height.min(frame_area.height),
    };
    f.render_widget(Clear, area);

    let key_style = Style::default()
        .fg(Color::Rgb(180, 130, 220))
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::Gray);
    let sep_style = Style::default().add_modifier(Modifier::DIM);

    let lines: Vec<Line<'static>> = HELP_ROWS
        .iter()
        .map(|row| {
            if row.keys.is_empty() && row.description.is_empty() {
                return Line::from("");
            }
            let pad = key_col_w.saturating_sub(row.keys.chars().count());
            Line::from(vec![
                Span::styled(format!(" {}", row.keys), key_style),
                Span::raw(" ".repeat(pad)),
                Span::styled("  │  ", sep_style),
                Span::styled(row.description.to_string(), desc_style),
            ])
        })
        .collect();

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" help — press any key to dismiss "),
    );
    f.render_widget(para, area);
}

/// Completion popup state. Owns the list of items the call to
/// `vw_htcl::complete_at` produced, plus the currently-selected
/// index and the cursor offset at which the popup was anchored
/// (used by Enter to know what range to replace).
#[derive(Debug)]
pub struct CompletionPopup {
    pub items: Vec<Completion>,
    pub selected: usize,
    /// Screen-anchor cell — where the popup's top-left should appear
    /// relative to the frame. Set by the caller (which knows the
    /// terminal cursor position) and used by the renderer.
    pub anchor: (u16, u16),
}

impl CompletionPopup {
    pub fn new(items: Vec<Completion>, anchor: (u16, u16)) -> Option<Self> {
        if items.is_empty() {
            None
        } else {
            Some(Self {
                items,
                selected: 0,
                anchor,
            })
        }
    }

    pub fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    /// The item the user would accept by pressing Enter.
    pub fn current(&self) -> Option<&Completion> {
        self.items.get(self.selected)
    }
}

/// Render a completion popup as an overlay. Sized to fit a sensible
/// row count from the available frame height (10 rows default, less
/// when near the bottom edge). Anchored just below-and-right of the
/// cursor position the popup was created at; clamped to stay inside
/// the frame.
pub fn draw_completion_popup(
    f: &mut Frame,
    popup: &CompletionPopup,
    frame_area: Rect,
) {
    if popup.items.is_empty() {
        return;
    }
    let max_rows = popup.items.len().min(10) as u16;
    // Width: longest label + space + longest detail + padding, capped
    // at 60 cells.
    let max_label = popup
        .items
        .iter()
        .map(|c| c.label.chars().count())
        .max()
        .unwrap_or(8);
    let max_detail = popup
        .items
        .iter()
        .map(|c| c.detail.as_deref().unwrap_or("").chars().count())
        .max()
        .unwrap_or(0);
    let width = (max_label + max_detail + 4).clamp(20, 60) as u16;
    let height = max_rows + 2; // +2 for borders

    let (anchor_x, anchor_y) = popup.anchor;
    let x = anchor_x.min(frame_area.x + frame_area.width.saturating_sub(width));
    let y = (anchor_y + 1)
        .min(frame_area.y + frame_area.height.saturating_sub(height));
    let area = Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, area);

    let items: Vec<ListItem> = popup
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let kind_glyph = match item.kind {
                CompletionKind::Proc => "·",
                CompletionKind::Flag => "-",
                CompletionKind::EnumValue => "=",
            };
            let kind_style = match item.kind {
                CompletionKind::Proc => {
                    Style::default().fg(Color::Rgb(230, 200, 120))
                }
                CompletionKind::Flag => {
                    Style::default().fg(Color::Rgb(180, 130, 220))
                }
                CompletionKind::EnumValue => {
                    Style::default().fg(Color::Rgb(100, 200, 200))
                }
            };
            let detail = item
                .detail
                .as_deref()
                .map(|d| format!("  {d}"))
                .unwrap_or_default();
            let mut spans = vec![
                Span::styled(format!("{kind_glyph} "), kind_style),
                Span::styled(
                    item.label.clone(),
                    if i == popup.selected {
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .fg(Color::White)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
            ];
            if !detail.is_empty() {
                spans.push(Span::styled(
                    detail,
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
            let style = if i == popup.selected {
                Style::default().bg(Color::Rgb(40, 40, 60))
            } else {
                Style::default()
            };
            ListItem::new(Line::from(spans)).style(style)
        })
        .collect();

    let mut list_state = ListState::default();
    list_state.select(Some(popup.selected));

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" completions "),
        )
        .highlight_style(Style::default().bg(Color::Rgb(40, 40, 60)));
    f.render_stateful_widget(list, area, &mut list_state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use vw_htcl::span::Span as HtclSpan;

    fn item(label: &str, kind: CompletionKind) -> Completion {
        Completion {
            label: label.to_string(),
            kind,
            detail: None,
            documentation: None,
            replace: HtclSpan { start: 0, end: 0 },
        }
    }

    #[test]
    fn new_returns_none_on_empty() {
        assert!(CompletionPopup::new(Vec::new(), (0, 0)).is_none());
    }

    #[test]
    fn move_up_down_clamps() {
        let items = vec![
            item("a", CompletionKind::Proc),
            item("b", CompletionKind::Proc),
        ];
        let mut p = CompletionPopup::new(items, (0, 0)).unwrap();
        assert_eq!(p.selected, 0);
        p.move_up();
        assert_eq!(p.selected, 0); // can't go below 0
        p.move_down();
        assert_eq!(p.selected, 1);
        p.move_down();
        assert_eq!(p.selected, 1); // can't go past last
        p.move_up();
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn current_returns_selected_item() {
        let items = vec![
            item("a", CompletionKind::Proc),
            item("b", CompletionKind::Flag),
        ];
        let mut p = CompletionPopup::new(items, (0, 0)).unwrap();
        assert_eq!(p.current().unwrap().label, "a");
        p.move_down();
        assert_eq!(p.current().unwrap().label, "b");
    }
}
