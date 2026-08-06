//! Interactive fuzzy picker for selecting an FPGA part from the
//! catalog that ships with the current Vivado install. Invoked by
//! `vw init` when no `--part` flag is passed and stdin is a tty.

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};
use std::io;
use vw_lib::parts::{matches_query, PartEntry, PartSeries};

/// Run the picker and return the selected part id, or `None` if the
/// user cancelled (Esc / Ctrl-C).
pub fn pick_part(parts: &[PartEntry]) -> io::Result<Option<String>> {
    if parts.is_empty() {
        return Ok(None);
    }

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = PickerState::new(parts);
    let result = loop {
        terminal.draw(|f| render(f, &mut state))?;
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        break Ok(None)
                    }
                    (KeyCode::Enter, _) => {
                        break Ok(state.selected_id().map(str::to_owned))
                    }
                    (KeyCode::Tab, _) => state.cycle_family_forward(),
                    (KeyCode::BackTab, _) => state.cycle_family_backward(),
                    (KeyCode::Backspace, _) => {
                        state.query.pop();
                        state.refilter();
                    }
                    (KeyCode::Char(c), m)
                        if !m.contains(KeyModifiers::CONTROL) =>
                    {
                        state.query.push(c);
                        state.refilter();
                    }
                    (KeyCode::Up, _) => state.select_prev(),
                    (KeyCode::Down, _) => state.select_next(),
                    (KeyCode::PageUp, _) => {
                        for _ in 0..10 {
                            state.select_prev();
                        }
                    }
                    (KeyCode::PageDown, _) => {
                        for _ in 0..10 {
                            state.select_next();
                        }
                    }
                    (KeyCode::Home, _) => state.select_first(),
                    (KeyCode::End, _) => state.select_last(),
                    _ => {}
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

struct PickerState<'a> {
    all: &'a [PartEntry],
    query: String,
    /// `None` = All families.
    family: Option<PartSeries>,
    filtered: Vec<usize>,
    list: ListState,
}

impl<'a> PickerState<'a> {
    fn new(all: &'a [PartEntry]) -> Self {
        let mut s = Self {
            all,
            query: String::new(),
            family: None,
            filtered: Vec::new(),
            list: ListState::default(),
        };
        s.refilter();
        s
    }

    fn refilter(&mut self) {
        self.filtered = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                self.family.is_none_or(|f| p.series == f)
                    && matches_query(&p.id, &self.query)
            })
            .map(|(i, _)| i)
            .collect();
        self.list.select((!self.filtered.is_empty()).then_some(0));
    }

    fn selected_id(&self) -> Option<&str> {
        let idx = self.list.selected()?;
        let src = *self.filtered.get(idx)?;
        Some(self.all[src].id.as_str())
    }

    fn select_next(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let cur = self.list.selected().unwrap_or(0);
        self.list
            .select(Some((cur + 1).min(self.filtered.len() - 1)));
    }

    fn select_prev(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let cur = self.list.selected().unwrap_or(0);
        self.list.select(Some(cur.saturating_sub(1)));
    }

    fn select_first(&mut self) {
        self.list.select((!self.filtered.is_empty()).then_some(0));
    }

    fn select_last(&mut self) {
        if !self.filtered.is_empty() {
            self.list.select(Some(self.filtered.len() - 1));
        }
    }

    fn family_label(&self) -> String {
        match self.family {
            None => "All".to_string(),
            Some(s) => s.label().to_string(),
        }
    }

    fn cycle_family_forward(&mut self) {
        let seq = family_cycle();
        let idx = seq.iter().position(|x| *x == self.family).unwrap_or(0);
        self.family = seq[(idx + 1) % seq.len()];
        self.refilter();
    }

    fn cycle_family_backward(&mut self) {
        let seq = family_cycle();
        let idx = seq.iter().position(|x| *x == self.family).unwrap_or(0);
        self.family = seq[(idx + seq.len() - 1) % seq.len()];
        self.refilter();
    }
}

/// Full family-chip cycle order: `All` first, then each series in
/// `PartSeries::all()` order.
fn family_cycle() -> Vec<Option<PartSeries>> {
    let mut v = vec![None];
    v.extend(PartSeries::all().into_iter().map(Some));
    v
}

fn render(f: &mut ratatui::Frame<'_>, state: &mut PickerState<'_>) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // search box
            Constraint::Min(1),    // results
            Constraint::Length(1), // help
        ])
        .split(area);

    render_search(f, chunks[0], state);
    render_results(f, chunks[1], state);
    render_help(f, chunks[2]);
}

fn render_search(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &PickerState<'_>,
) {
    let family = state.family_label();
    let chip = format!(" [Family: {family}] ");
    let query_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(&state.query),
        Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(vec![
            Span::raw(" Search parts "),
            Span::styled(
                format!("({} matches)", state.filtered.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ]))
        .title_alignment(ratatui::layout::Alignment::Left)
        .title_bottom(
            Line::from(chip).alignment(ratatui::layout::Alignment::Right),
        );
    f.render_widget(Paragraph::new(query_line).block(block), area);
}

fn render_results(
    f: &mut ratatui::Frame<'_>,
    area: Rect,
    state: &mut PickerState<'_>,
) {
    if state.filtered.is_empty() {
        let msg = if state.all.is_empty() {
            "No parts found — Vivado install not detected."
        } else {
            "No matches. Showing only families installed with Vivado; pass --part <literal> for others."
        };
        let block = Block::default().borders(Borders::ALL);
        f.render_widget(
            Paragraph::new(msg)
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    }
    let items: Vec<ListItem> = state
        .filtered
        .iter()
        .map(|&i| {
            let p = &state.all[i];
            let series = Span::styled(
                format!("  {:<11}", p.series.label()),
                Style::default().fg(Color::DarkGray),
            );
            ListItem::new(Line::from(vec![Span::raw(&p.id), series]))
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL))
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut state.list);
}

fn render_help(f: &mut ratatui::Frame<'_>, area: Rect) {
    let help = Line::from(vec![
        Span::styled(" Tab", Style::default().fg(Color::Yellow)),
        Span::raw(": family  "),
        Span::styled("↑↓/PgUp/PgDn", Style::default().fg(Color::Yellow)),
        Span::raw(": navigate  "),
        Span::styled("Enter", Style::default().fg(Color::Yellow)),
        Span::raw(": select  "),
        Span::styled("Esc", Style::default().fg(Color::Yellow)),
        Span::raw(": cancel"),
    ]);
    f.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}
