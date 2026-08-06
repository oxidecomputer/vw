// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Fuzzy symbol picker — Ctrl-T opens a centered modal listing every
//! known procedure / type / enum / variant across the session and
//! the in-flight input. Typing filters; ↑/↓ navigates; Enter inserts
//! the symbol's qualified name at the cursor in the input editor.
//!
//! The matcher is [`nucleo_matcher`] — the same engine Helix and
//! several other Rust TUIs use. Each candidate is scored twice:
//! once against its `name` (high weight) and once against its
//! `doc_summary` (low weight). The final score takes the max of
//! the two, so a query that hits ONLY a doc-comment phrase still
//! surfaces the symbol but ranks below any name match.
//!
//! Tab inside the picker switches to the **library view** —
//! `(library, symbol-count)` rows. Enter on a library row filters
//! the symbol list to that library.

use std::sync::Arc;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32String};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, ListState, Paragraph,
};
use ratatui::Frame;

use crate::symbol_index::{LibraryInfo, LibraryRef, SymbolIndex, SymbolKind};

/// Boost factor applied to name-match scores so name hits always
/// outrank doc-only hits. Picked by trial — a value of 3 keeps a
/// short doc match (e.g. one word) below any name fuzzy hit.
const NAME_WEIGHT: u32 = 3;

/// Symbol-picker overlay state.
#[derive(Debug)]
pub struct SymbolPicker {
    /// Snapshot of the symbol index when the picker was opened.
    /// We don't update mid-search — the index may not change in
    /// practice (no commits land while a popup is open), and a
    /// stable index avoids selection-index churn while typing.
    pub index: Arc<SymbolIndex>,
    /// Live query string the user is typing.
    pub query: String,
    /// Scored result indices into `index.all()` in display order.
    pub results: Vec<Scored>,
    pub selected: usize,
    /// When `Some(name)`, the result list is filtered to that
    /// library. Set by accepting a row in the libraries sub-view.
    pub library_filter: Option<String>,
    /// Toggle between symbol list and library list.
    pub view: PickerView,
    /// Cached library list (computed once on open).
    pub libraries: Vec<LibraryInfo>,
    /// Selected library row (only used in `Libraries` view).
    pub selected_library: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerView {
    Symbols,
    Libraries,
}

#[derive(Clone, Copy, Debug)]
pub struct Scored {
    pub sym_idx: usize,
    pub score: u32,
}

impl SymbolPicker {
    pub fn new(index: Arc<SymbolIndex>) -> Self {
        let libraries = index.libraries();
        let mut p = Self {
            index,
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            library_filter: None,
            view: PickerView::Symbols,
            libraries,
            selected_library: 0,
        };
        p.recompute();
        p
    }

    pub fn move_up(&mut self) {
        match self.view {
            PickerView::Symbols => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            PickerView::Libraries => {
                if self.selected_library > 0 {
                    self.selected_library -= 1;
                }
            }
        }
    }

    pub fn move_down(&mut self) {
        match self.view {
            PickerView::Symbols => {
                if self.selected + 1 < self.results.len() {
                    self.selected += 1;
                }
            }
            PickerView::Libraries => {
                if self.selected_library + 1 < self.libraries.len() {
                    self.selected_library += 1;
                }
            }
        }
    }

    pub fn push_char(&mut self, c: char) {
        self.query.push(c);
        self.recompute();
    }

    pub fn pop_char(&mut self) {
        self.query.pop();
        self.recompute();
    }

    pub fn toggle_view(&mut self) {
        self.view = match self.view {
            PickerView::Symbols => PickerView::Libraries,
            PickerView::Libraries => PickerView::Symbols,
        };
    }

    /// Apply a library filter — sets `library_filter` and switches
    /// back to the symbols view. Used when Enter is pressed on a
    /// library row.
    pub fn apply_library_filter(&mut self) {
        if let Some(lib) = self.libraries.get(self.selected_library) {
            self.library_filter = Some(lib.library.display());
            self.view = PickerView::Symbols;
            self.selected = 0;
            self.recompute();
        }
    }

    /// Currently-selected symbol, if any (Symbols view only).
    pub fn current_symbol(&self) -> Option<&crate::symbol_index::Symbol> {
        let idx = self.results.get(self.selected)?.sym_idx;
        self.index.all().get(idx)
    }

    /// Re-run the matcher with the current query and library
    /// filter. Called from `new` / `push_char` / `pop_char` /
    /// `apply_library_filter`. O(N) over the index.
    fn recompute(&mut self) {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let all = self.index.all();
        // Indices that survive the library filter (if any).
        let candidate_indices: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, s)| match &self.library_filter {
                Some(lib) => s.library.display() == *lib,
                None => true,
            })
            .map(|(i, _)| i)
            .collect();

        if self.query.is_empty() {
            // No query: show all candidates sorted alphabetically,
            // capped at 200 so the popup is bounded.
            let mut sorted = candidate_indices.clone();
            sorted.sort_by(|a, b| all[*a].name.cmp(&all[*b].name));
            sorted.truncate(200);
            self.results = sorted
                .into_iter()
                .map(|sym_idx| Scored { sym_idx, score: 0 })
                .collect();
            self.selected =
                self.selected.min(self.results.len().saturating_sub(1));
            return;
        }

        let pattern = Pattern::parse(
            &self.query,
            CaseMatching::Smart,
            Normalization::Smart,
        );
        let mut scored: Vec<Scored> = candidate_indices
            .iter()
            .filter_map(|&sym_idx| {
                let sym = &all[sym_idx];
                let name_haystack = Utf32String::from(sym.name.as_str());
                let doc_haystack = Utf32String::from(sym.doc_summary.as_str());
                let name_score =
                    pattern.score(name_haystack.slice(..), &mut matcher);
                let doc_score =
                    pattern.score(doc_haystack.slice(..), &mut matcher);
                let combined = match (name_score, doc_score) {
                    (None, None) => 0,
                    (Some(n), None) => n * NAME_WEIGHT,
                    (None, Some(d)) => d,
                    (Some(n), Some(d)) => (n * NAME_WEIGHT).max(d),
                };
                if combined == 0 {
                    None
                } else {
                    Some(Scored {
                        sym_idx,
                        score: combined,
                    })
                }
            })
            .collect();
        scored.sort_by_key(|b| std::cmp::Reverse(b.score));
        scored.truncate(200);
        self.results = scored;
        if self.selected >= self.results.len() {
            self.selected = self.results.len().saturating_sub(1);
        }
    }
}

/// Render the picker as a centered modal. Sized at 70% width × 75%
/// height of the frame.
pub fn draw_symbol_picker(f: &mut Frame, picker: &SymbolPicker) {
    let frame = f.area();
    let width = (frame.width as f32 * 0.7) as u16;
    let height = (frame.height as f32 * 0.75) as u16;
    let x = frame.x + (frame.width.saturating_sub(width)) / 2;
    let y = frame.y + (frame.height.saturating_sub(height)) / 2;
    let area = Rect {
        x,
        y,
        width,
        height,
    };
    f.render_widget(Clear, area);

    // Top: query line (1 row). Middle: list (fills). Bottom: hint
    // (1 row). Use a manual vertical split since this is a small
    // fixed layout.
    let inner_top = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: 1,
    };
    let inner_hint = Rect {
        x: area.x + 1,
        y: area.y + area.height.saturating_sub(2),
        width: area.width.saturating_sub(2),
        height: 1,
    };
    let inner_list = Rect {
        x: area.x + 1,
        y: area.y + 2,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(4),
    };

    let title = match picker.view {
        PickerView::Symbols => " symbol search ",
        PickerView::Libraries => " libraries — Enter to filter ",
    };
    f.render_widget(Block::default().borders(Borders::ALL).title(title), area);

    let prompt_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);
    let prompt_prefix = match picker.view {
        PickerView::Symbols => "› ",
        PickerView::Libraries => "» ",
    };
    let prompt_text = match (picker.view, picker.library_filter.as_deref()) {
        (PickerView::Symbols, Some(lib)) => {
            format!("{prompt_prefix}[{lib}] {}", picker.query)
        }
        (PickerView::Symbols, None) => {
            format!("{prompt_prefix}{}", picker.query)
        }
        (PickerView::Libraries, _) => "(use ↑/↓; Enter to filter)".to_string(),
    };
    f.render_widget(
        Paragraph::new(Span::styled(prompt_text, prompt_style)),
        inner_top,
    );

    match picker.view {
        PickerView::Symbols => render_symbols(f, picker, inner_list),
        PickerView::Libraries => render_libraries(f, picker, inner_list),
    }

    let hint = match picker.view {
        PickerView::Symbols => "Tab: libraries · Enter: insert · Esc: dismiss",
        PickerView::Libraries => "Tab: symbols · Enter: filter · Esc: dismiss",
    };
    f.render_widget(Paragraph::new(Span::styled(hint, dim)), inner_hint);
}

fn render_symbols(f: &mut Frame, picker: &SymbolPicker, area: Rect) {
    let all = picker.index.all();
    let items: Vec<ListItem> = picker
        .results
        .iter()
        .filter_map(|scored| all.get(scored.sym_idx))
        .map(|sym| {
            let icon = match sym.kind {
                SymbolKind::Proc => "·",
                SymbolKind::Type => "≡",
                SymbolKind::EnumDecl => "◆",
                SymbolKind::EnumVariant => "◇",
                SymbolKind::Variable => "$",
            };
            let icon_style = match sym.kind {
                SymbolKind::Proc => {
                    Style::default().fg(Color::Rgb(230, 200, 120))
                }
                SymbolKind::Type
                | SymbolKind::EnumDecl
                | SymbolKind::EnumVariant => {
                    Style::default().fg(Color::Rgb(100, 200, 200))
                }
                SymbolKind::Variable => {
                    Style::default().fg(Color::Rgb(130, 200, 230))
                }
            };
            let lib_style = Style::default().add_modifier(Modifier::DIM);
            let name_style = Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD);
            let doc_style = Style::default().fg(Color::Gray);

            let mut spans = vec![
                Span::styled(format!("{icon} "), icon_style),
                Span::styled(format!("{} ", sym.library.display()), lib_style),
                Span::styled(":: ", lib_style),
                Span::styled(sym.name.clone(), name_style),
            ];
            if !sym.doc_summary.is_empty() {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(sym.doc_summary.clone(), doc_style));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut state = ListState::default();
    if !picker.results.is_empty() {
        state.select(Some(picker.selected));
    }
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(Color::Rgb(40, 40, 60))
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(list, area, &mut state);
}

fn render_libraries(f: &mut Frame, picker: &SymbolPicker, area: Rect) {
    let items: Vec<ListItem> = picker
        .libraries
        .iter()
        .map(|info| {
            let name = info.library.display();
            let path_str = match &info.library {
                LibraryRef::Entry => "<entry>".to_string(),
                LibraryRef::Import { path, .. } => path.display().to_string(),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {:5} ", info.symbol_count),
                    Style::default()
                        .fg(Color::Rgb(230, 200, 120))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    name,
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    path_str,
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    if !picker.libraries.is_empty() {
        state.select(Some(picker.selected_library));
    }
    let list = List::new(items).highlight_style(
        Style::default()
            .bg(Color::Rgb(40, 40, 60))
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(list, area, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::session::Session;
    use crate::symbol_index::SymbolIndex;

    fn build_index_from_src(src: &str) -> Arc<SymbolIndex> {
        let session = Session::default();
        let parsed = vw_htcl::parse(src);
        Arc::new(SymbolIndex::build(&session, None, Some(&parsed.document)))
    }

    #[test]
    fn empty_query_shows_all_alphabetic() {
        let idx = build_index_from_src(
            "proc foo {} unit {}\nproc bar {} unit {}\nproc baz {} unit {}",
        );
        let picker = SymbolPicker::new(idx);
        let names: Vec<&str> = picker
            .results
            .iter()
            .map(|s| picker.index.all()[s.sym_idx].name.as_str())
            .collect();
        assert_eq!(names, vec!["bar", "baz", "foo"]);
    }

    #[test]
    fn query_filters_results() {
        let idx = build_index_from_src(
            "proc foo {} unit {}\nproc bar {} unit {}\nproc baz {} unit {}",
        );
        let mut picker = SymbolPicker::new(idx);
        picker.push_char('b');
        picker.push_char('a');
        let names: Vec<&str> = picker
            .results
            .iter()
            .map(|s| picker.index.all()[s.sym_idx].name.as_str())
            .collect();
        assert!(names.contains(&"bar"));
        assert!(names.contains(&"baz"));
        assert!(!names.contains(&"foo"));
    }

    #[test]
    fn name_match_outranks_doc_only() {
        // A proc whose name doesn't match but whose docs mention the
        // query word should rank BELOW a proc whose name matches.
        let src = "\
## a wonderful procedure that does foobar things
proc unrelated {} unit {}
proc foobar_proc {} unit {}";
        let idx = build_index_from_src(src);
        let mut picker = SymbolPicker::new(idx);
        for c in "foobar".chars() {
            picker.push_char(c);
        }
        let names: Vec<&str> = picker
            .results
            .iter()
            .map(|s| picker.index.all()[s.sym_idx].name.as_str())
            .collect();
        // foobar_proc must come first
        assert_eq!(names.first().copied(), Some("foobar_proc"), "{names:?}");
    }

    #[test]
    fn library_view_toggles() {
        let idx = build_index_from_src("proc foo {} unit {}");
        let mut picker = SymbolPicker::new(idx);
        assert_eq!(picker.view, PickerView::Symbols);
        picker.toggle_view();
        assert_eq!(picker.view, PickerView::Libraries);
        picker.toggle_view();
        assert_eq!(picker.view, PickerView::Symbols);
    }
}
