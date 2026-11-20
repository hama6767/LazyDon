use crate::app::{App, Mode, ViewItem};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

/// Draw the interface from the current application state.
pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);

    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " LazyDon ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(&app.account, Style::default().fg(Color::Cyan)),
        Span::raw("  ·  "),
        Span::styled(&app.instance, Style::default().fg(Color::DarkGray)),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    frame.render_widget(header, rows[0]);

    // Stack the panels on narrow screens so that details remain accessible.
    if rows[1].width >= 80 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(47), Constraint::Percentage(53)])
            .split(rows[1]);
        draw_timeline(frame, app, columns[0]);
        draw_detail(frame, app, columns[1]);
    } else if rows[1].height >= 16 {
        let panels = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(rows[1]);
        draw_timeline(frame, app, panels[0]);
        draw_detail(frame, app, panels[1]);
    } else {
        draw_timeline(frame, app, rows[1]);
    }

    let footer = Paragraph::new(vec![
        Line::from(Span::styled(
            &app.status,
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            "j/k move  h/l tab  a add  c compose  f favourite  b boost  ? help  q quit",
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    frame.render_widget(footer, rows[2]);

    match app.mode {
        Mode::Normal => {}
        Mode::Help => draw_popup(frame, " Help ", help_text(), 76, 75),
        Mode::AddTab => draw_popup(
            frame,
            " Add timeline ",
            vec![
                Line::from("l: local timeline"),
                Line::from("f: federated timeline"),
                Line::from("Esc: cancel"),
            ],
            58,
            30,
        ),
        Mode::Compose => {
            let mut lines = vec![
                Line::from("Write a post. Ctrl-S: send · Esc: keep draft"),
                Line::from("Ctrl-V: attach image from clipboard"),
                Line::from(Span::styled(
                    app.status.as_str(),
                    Style::default().fg(Color::Yellow),
                )),
                Line::from(""),
            ];
            lines.extend(app.input.split('\n').map(Line::from));
            draw_popup(frame, " New post ", lines, 80, 65);
        }
    }
}

fn draw_timeline(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(tab) = app.tabs.get(app.active) else {
        frame.render_widget(
            Paragraph::new("No timelines").block(panel(" Timelines ", true)),
            area,
        );
        return;
    };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(area);
    let mut tab_spans = Vec::new();
    for (i, tab) in app.tabs.iter().enumerate() {
        let style = if i == app.active {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        tab_spans.push(Span::styled(format!(" {} ", tab.title), style));
        tab_spans.push(Span::raw(" "));
    }
    frame.render_widget(
        Paragraph::new(Line::from(tab_spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Timelines ")
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        sections[0],
    );

    let items: Vec<ListItem<'_>> = tab.items.iter().map(item_row).collect();
    let title = format!(" {} · {} ", tab.title, tab.items.len());
    let list = List::new(items)
        .block(panel(&title, true))
        .highlight_style(Style::default().bg(Color::Rgb(34, 48, 57)))
        .highlight_symbol("▌");
    let mut state = ListState::default();
    if !tab.items.is_empty() {
        state.select(Some(tab.selected.min(tab.items.len() - 1)));
    }
    frame.render_stateful_widget(list, sections[1], &mut state);
    if tab.items.is_empty() {
        let inner = panel("", true).inner(sections[1]);
        frame.render_widget(
            Paragraph::new("No posts yet. Press r to refresh.")
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true }),
            inner,
        );
    }
}

fn item_row(item: &ViewItem) -> ListItem<'_> {
    let color = account_color(&item.acct);
    let context = if item.context.is_empty() {
        "".to_string()
    } else {
        format!(" · {}", item.context)
    };
    let media = if item.media.is_empty() {
        ""
    } else {
        "  [image]"
    };
    let body = item
        .content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(2)
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ");
    let body = truncate(&body, 105);
    ListItem::new(vec![
        Line::from(vec![
            Span::styled("● ", Style::default().fg(color)),
            Span::styled(
                item.author.as_str(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" @{}", item.acct),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(vec![
            Span::styled("  ├─ ", Style::default().fg(Color::DarkGray)),
            Span::raw(body),
        ]),
        Line::from(Span::styled(
            format!("  └─ {}{}{}", item.timestamp, context, media),
            Style::default().fg(Color::DarkGray),
        )),
    ])
}

fn draw_detail(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some(item) = app
        .tabs
        .get(app.active)
        .and_then(|tab| tab.items.get(tab.selected))
    else {
        frame.render_widget(
            Paragraph::new("Select a post to see its details.")
                .style(Style::default().fg(Color::DarkGray))
                .block(panel(" Post ", false)),
            area,
        );
        return;
    };
    let color = account_color(&item.acct);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("● ", Style::default().fg(color)),
            Span::styled(
                item.author.as_str(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" @{}", item.acct),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(Span::styled(
            format!("{}  {}", item.timestamp, item.context),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
    ];
    lines.extend(item.content.lines().map(|line| Line::from(line.to_owned())));
    if !item.media.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("└─ Media ({})", item.media.len()),
            Style::default().fg(Color::Cyan),
        )));
        if app.preview.is_empty() {
            lines.push(Line::from(Span::styled(
                "   Loading preview…",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            lines.extend(app.preview.iter().map(|line| Line::from(line.as_str())));
        }
    }
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(panel(" Post ", false))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn panel(title: &str, active: bool) -> Block<'_> {
    Block::default()
        .title(title.to_string())
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if active { Color::Cyan } else { Color::DarkGray }))
}

fn draw_popup(frame: &mut Frame<'_>, title: &str, lines: Vec<Line<'_>>, width: u16, height: u16) {
    let area = centered_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(title, true))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height) / 2),
            Constraint::Percentage(height),
            Constraint::Percentage((100 - height) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width) / 2),
            Constraint::Percentage(width),
            Constraint::Percentage((100 - width) / 2),
        ])
        .split(vertical[1])[1]
}

fn help_text<'a>() -> Vec<Line<'a>> {
    [
        "j / k, ↓ / ↑   Move through posts",
        "g / G           First / last post",
        "h / l, ← / →   Switch timelines",
        "a               Add local or federated timeline",
        "c               Compose a post",
        "r               Refresh current timeline",
        "f / b           Toggle favourite / boost",
        "?               Toggle this help",
        "q               Quit",
        "",
        "Compose: Ctrl-S sends, Ctrl-V attaches clipboard image.",
        "Esc keeps the draft; press c to resume it.",
    ]
    .into_iter()
    .map(Line::from)
    .collect()
}

fn account_color(account: &str) -> Color {
    const COLORS: [Color; 8] = [
        Color::Cyan,
        Color::Green,
        Color::Yellow,
        Color::Magenta,
        Color::LightBlue,
        Color::LightGreen,
        Color::LightRed,
        Color::LightMagenta,
    ];
    // FNV-1a is deterministic across runs, unlike the default randomized hasher.
    let hash = account.bytes().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x100000001b3)
    });
    COLORS[hash as usize % COLORS.len()]
}

fn truncate(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let result: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        format!("{}…", result)
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn render(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..height {
            for x in 0..width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn default_tabs_and_selected_post_are_visible_on_a_normal_terminal() {
        let app = crate::app::test_app();
        let screen = render(&app, 120, 36);
        assert!(screen.contains("Home"));
        assert!(screen.contains("Notifications"));
        assert!(screen.contains("Alice"));
        assert!(screen.contains("Hello from Alice"));
        assert!(screen.contains("Media (1)"));
        assert!(screen.contains("[preview]"));
    }

    #[test]
    fn narrow_terminal_stacks_timeline_and_post_detail() {
        let app = crate::app::test_app();
        let screen = render(&app, 60, 30);
        assert!(screen.contains("Home"));
        assert!(screen.contains("Notifications"));
        assert!(screen.contains("Hello from Alice"));
        assert!(screen.contains("[preview]"));
    }

    #[test]
    fn compose_overlay_shows_multiline_draft_and_retention_hint() {
        let mut app = crate::app::test_app();
        app.mode = Mode::Compose;
        app.input = "First line\nSecond line".into();
        let screen = render(&app, 100, 36);
        assert!(screen.contains("New post"));
        assert!(screen.contains("First line"));
        assert!(screen.contains("Second line"));
        assert!(screen.contains("Esc: keep draft"));
        assert!(screen.contains("Ctrl-V"));
    }
}
