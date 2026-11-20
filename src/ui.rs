use crate::app::{App, Mode, ViewItem};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthChar;

/// Draw the interface from the current application state.
pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
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

    // Keep notifications visible while preserving room for the selected post.
    // On narrower terminals the same three panels stack vertically.
    if rows[1].width >= 105 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(42),
                Constraint::Percentage(30),
                Constraint::Percentage(28),
            ])
            .split(rows[1]);
        draw_timeline(frame, app, columns[0]);
        draw_detail(frame, app, columns[1]);
        draw_notifications(frame, app, columns[2]);
    } else {
        let panels = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(40),
                Constraint::Percentage(32),
                Constraint::Percentage(28),
            ])
            .split(rows[1]);
        draw_timeline(frame, app, panels[0]);
        draw_detail(frame, app, panels[1]);
        draw_notifications(frame, app, panels[2]);
    }

    let footer = Paragraph::new(vec![
        Line::from(Span::styled(
            &app.status,
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            "j/k move  h/l tab  Enter details  a add  c compose  f favourite  b boost  ? help  q quit",
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
        Mode::Compose => draw_compose(frame, app),
        Mode::Detail => draw_detail_popup(frame, app),
    }
}

fn draw_timeline(frame: &mut Frame<'_>, app: &App, area: Rect) {
    // Notifications have their own persistent panel. When it has focus, keep
    // the Home timeline alongside it instead of showing the same list twice.
    let timeline_index = app
        .tabs
        .get(app.active)
        .filter(|tab| tab.title != "Notifications")
        .map(|_| app.active)
        .or_else(|| app.tabs.iter().position(|tab| tab.title == "Home"));
    let Some(tab) = timeline_index.and_then(|index| app.tabs.get(index)) else {
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

    let width = sections[1].width.saturating_sub(3) as usize;
    let items: Vec<ListItem<'_>> = tab.items.iter().map(|item| item_row(item, width)).collect();
    let title = format!(" {} · {} ", tab.title, tab.items.len());
    let timeline_focused = timeline_index == Some(app.active);
    let list = List::new(items)
        .block(panel(&title, timeline_focused))
        .highlight_style(Style::default().bg(Color::Rgb(34, 48, 57)))
        .highlight_symbol(if timeline_focused { "▌" } else { " " });
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

fn draw_notifications(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let Some((index, tab)) = app
        .tabs
        .iter()
        .enumerate()
        .find(|(_, tab)| tab.title == "Notifications")
    else {
        frame.render_widget(
            Paragraph::new("Notifications unavailable").block(panel(" Notifications ", false)),
            area,
        );
        return;
    };
    let focused = index == app.active;
    let title = format!(" Notifications · {} ", tab.items.len());
    let width = area.width.saturating_sub(3) as usize;
    let list = List::new(
        tab.items
            .iter()
            .map(|item| notification_row(item, width))
            .collect::<Vec<_>>(),
    )
    .block(panel(&title, focused))
    .highlight_style(Style::default().bg(Color::Rgb(34, 48, 57)))
    .highlight_symbol(if focused { "▌" } else { " " });
    let mut state = ListState::default();
    if !tab.items.is_empty() {
        state.select(Some(tab.selected.min(tab.items.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
    if tab.items.is_empty() {
        let inner = panel("", focused).inner(area);
        frame.render_widget(
            Paragraph::new("No notifications yet.")
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: true }),
            inner,
        );
    }
}

// Wrap at display-cell boundaries rather than bytes or words. This preserves
// Japanese text and existing newlines and works even in a very narrow pane.
fn wrapped_lines(text: &str, width: usize, limit: usize) -> Vec<String> {
    let width = width.max(2); // enough room for a full-width character
    let mut out = vec![String::new()];
    let mut col = 0;
    let mut chars = text.chars();
    for ch in chars.by_ref().take(limit) {
        if ch == '\n' {
            out.push(String::new());
            col = 0;
            continue;
        }
        if ch.is_control() {
            continue;
        }
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + w > width && col > 0 {
            out.push(String::new());
            col = 0;
        }
        out.last_mut().unwrap().push(ch);
        col += w;
    }
    if chars.next().is_some() {
        if col + 1 > width {
            out.push(String::new());
        }
        out.last_mut().unwrap().push('…');
    }
    out
}

fn body_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    wrapped_lines(text, width.saturating_sub(5), 140)
        .into_iter()
        .map(|part| {
            Line::from(vec![
                Span::styled("  │  ", Style::default().fg(Color::DarkGray)),
                Span::styled(part, Style::default().fg(Color::White)),
            ])
        })
        .collect()
}

fn notification_row(item: &ViewItem, width: usize) -> ListItem<'_> {
    let event = if item.context == "notification" {
        item.content.as_str()
    } else {
        item.context.split(" ♥").next().unwrap_or(&item.context)
    };
    let mut lines = vec![Line::from(Span::styled(
        event.to_owned(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))];
    lines.extend(body_lines(&item.content, width));
    lines.push(Line::from(Span::styled(
        format!("  └─ @{} · {}", item.acct, item.timestamp),
        Style::default().fg(Color::DarkGray),
    )));
    ListItem::new(lines)
}

fn item_row(item: &ViewItem, width: usize) -> ListItem<'_> {
    let color = account_color(&item.acct);
    let context = if item.context.is_empty() {
        String::new()
    } else {
        format!(" · {}", item.context)
    };
    let media = if item.media.is_empty() {
        ""
    } else {
        "  [image]"
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("● ", Style::default().fg(color)),
        Span::styled(
            item.author.as_str(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" @{}", item.acct),
            Style::default().fg(Color::DarkGray),
        ),
    ])];
    lines.extend(body_lines(&item.content, width));
    lines.push(Line::from(Span::styled(
        format!("  └─ {}{}{}", item.timestamp, context, media),
        Style::default().fg(Color::DarkGray),
    )));
    ListItem::new(lines)
}

fn draw_detail(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
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
    lines.extend(item.content.lines().map(|line| {
        Line::from(Span::styled(
            line.to_owned(),
            Style::default().fg(Color::White),
        ))
    }));
    let block = panel(" Post ", false);
    let inner = block.inner(area);
    if let Some(preview) = &item.link_preview {
        lines.extend(
            wrapped_lines(preview, inner.width.max(2) as usize, 200)
                .into_iter()
                .take(2)
                .map(|line| Line::from(Span::styled(line, Style::default().fg(Color::Cyan)))),
        );
    }
    frame.render_widget(block, area);
    if item.media.is_empty() {
        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
            inner,
        );
    } else {
        // Draw image rows in a separate, non-wrapping paragraph: wrapping a
        // terminal image breaks its geometry when the pane is narrow.
        let width = inner.width.max(1) as usize;
        let text_height: usize = lines
            .iter()
            .map(|line| (line.width().max(1) + width - 1) / width)
            .sum();
        let body_height = text_height.min((inner.height as usize / 2).max(1)) as u16;
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(body_height),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(inner);
        frame.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }),
            sections[0],
        );
        frame.render_widget(
            Paragraph::new(format!("└─ Media ({})", item.media.len()))
                .style(Style::default().fg(Color::Cyan)),
            sections[1],
        );
        // Avoid emitting image graphics under a modal, where they could obscure it.
        if app.mode == Mode::Normal && sections[2].width > 0 && sections[2].height > 0 {
            if let Some(preview) = &mut app.native_preview {
                preview.render(frame, sections[2]);
            } else {
                frame.render_widget(
                    Paragraph::new("Image preview unavailable")
                        .style(Style::default().fg(Color::DarkGray)),
                    sections[2],
                );
            }
        }
    }
}

fn draw_compose(frame: &mut Frame<'_>, app: &App) {
    let area = centered_rect(80, 75, frame.area());
    frame.render_widget(Clear, area);
    let block = panel(" New post ", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(inner);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Posting as ", Style::default().fg(Color::DarkGray)),
            Span::styled(&app.account, Style::default().fg(Color::Cyan)),
        ])),
        sections[0],
    );
    let width = sections[1].width.saturating_sub(2).max(2) as usize;
    let (mut draft_lines, cursor_row, cursor_col) =
        draft_layout(&app.input, app.input_cursor, width);
    if app.input.is_empty() {
        draft_lines[0] = "Write your post here…".into();
    }
    let visible = sections[1].height.saturating_sub(2) as usize;
    let scroll = cursor_row
        .saturating_add(1)
        .saturating_sub(visible)
        .min(u16::MAX as usize) as u16;
    let body = Text::from(draft_lines.into_iter().map(Line::from).collect::<Vec<_>>());
    frame.render_widget(
        Paragraph::new(body)
            .block(panel(" Body · editing ", true))
            .style(
                Style::default()
                    .fg(if app.input.is_empty() {
                        Color::DarkGray
                    } else {
                        Color::White
                    })
                    .bg(Color::Rgb(25, 31, 39)),
            )
            .scroll((scroll, 0)),
        sections[1],
    );
    if visible > 0
        && sections[1].width > 2
        && cursor_row >= scroll as usize
        && cursor_row - (scroll as usize) < visible
    {
        frame.set_cursor_position((
            sections[1].x + 1 + cursor_col as u16,
            sections[1].y + 1 + (cursor_row - scroll as usize) as u16,
        ));
    }
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Attachments  ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("{} / 4 images", app.attachment_count())),
                Span::styled(
                    "    ·    Characters  ",
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(app.input.chars().count().to_string()),
            ]),
            Line::from(Span::styled(
                &app.status,
                Style::default().fg(Color::Yellow),
            )),
        ]),
        sections[2],
    );
    frame.render_widget(
        Paragraph::new("Ctrl-S: send  ·  Ctrl-V: attach image  ·  Esc: keep draft")
            .style(Style::default().fg(Color::Cyan))
            .wrap(Wrap { trim: true }),
        sections[3],
    );
}

// Return explicit wrapped lines and the caret's terminal-cell position. Showing
// the real cursor helps IMEs place their preedit/candidate window correctly.
fn draft_layout(text: &str, cursor: usize, width: usize) -> (Vec<String>, usize, usize) {
    let width = width.max(2);
    let cursor = (0..=cursor.min(text.len()))
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    let mut lines = vec![String::new()];
    let (mut row, mut col) = (0, 0);
    let mut caret = None;
    for (offset, ch) in text.char_indices() {
        if offset == cursor {
            let next_width = if ch == '\n' {
                0
            } else {
                UnicodeWidthChar::width(ch).unwrap_or(0)
            };
            caret = Some(if col == width || (col > 0 && col + next_width > width) {
                (row + 1, 0)
            } else {
                (row, col)
            });
        }
        if ch == '\n' {
            lines.push(String::new());
            row += 1;
            col = 0;
        } else if !ch.is_control() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if col + w > width && col > 0 {
                lines.push(String::new());
                row += 1;
                col = 0;
            }
            lines.last_mut().unwrap().push(ch);
            col += w;
        }
    }
    let caret = caret.unwrap_or(if col == width {
        (row + 1, 0)
    } else {
        (row, col)
    });
    while lines.len() <= caret.0 {
        lines.push(String::new());
    }
    (lines, caret.0, caret.1)
}

fn draw_detail_popup(frame: &mut Frame<'_>, app: &mut App) {
    let Some(item) = app
        .tabs
        .get(app.active)
        .and_then(|tab| tab.items.get(tab.selected))
    else {
        return;
    };
    let area = centered_rect(88, 88, frame.area());
    frame.render_widget(Clear, area);
    let block = panel(" Post details · Esc close ", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                item.author.as_str(),
                Style::default()
                    .fg(account_color(&item.acct))
                    .add_modifier(Modifier::BOLD),
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
    if !item.details.is_empty() {
        lines.push(Line::from(Span::styled(
            item.details.as_str(),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.extend(item.content.split('\n').map(|s| Line::from(s.to_owned())));
    if let Some(preview) = &item.link_preview {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Link preview",
            Style::default().fg(Color::Cyan),
        )));
        lines.extend(preview.lines().map(|s| Line::from(s.to_owned())));
    }
    if !item.attachments.is_empty() || !item.links.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Media and links · j/k select · Enter/o open",
            Style::default().fg(Color::Cyan),
        )));
        for (i, attachment) in item.attachments.iter().enumerate() {
            let marker = if app.detail_index == i { "▸" } else { " " };
            let description = if attachment.description.is_empty() {
                String::new()
            } else {
                format!(" — {}", attachment.description)
            };
            lines.push(Line::from(Span::styled(
                format!("{} {} {}{}", marker, i + 1, attachment.kind, description),
                if app.detail_index == i {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            )));
            lines.push(Line::from(format!("    {}", attachment.url)));
        }
        for (i, link) in item.links.iter().enumerate() {
            let index = i + item.attachments.len();
            let marker = if app.detail_index == index {
                "▸"
            } else {
                " "
            };
            lines.push(Line::from(Span::styled(
                format!("{} {} {}", marker, index + 1, link),
                if app.detail_index == index {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ PgUp/PgDown scroll · Esc close",
        Style::default().fg(Color::DarkGray),
    )));
    let (text_area, image_area) = if !item.media.is_empty()
        && app.native_preview.is_some()
        && inner.width >= 80
        && inner.height >= 8
    {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
            .split(inner);
        (columns[0], Some(columns[1]))
    } else {
        (inner, None)
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll, 0)),
        text_area,
    );
    if let (Some(area), Some(preview)) = (image_area, &mut app.native_preview) {
        let block = panel(" Media preview ", false);
        let image_area = block.inner(area);
        frame.render_widget(block, area);
        if image_area.width > 0 && image_area.height > 0 {
            preview.render(frame, image_area);
        }
    }
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
        "Enter           Show post details, media and links",
        "a               Add local or federated timeline",
        "c               Compose a post",
        "r               Refresh current timeline",
        "f / b           Toggle favourite / boost",
        "?               Toggle this help",
        "q               Quit",
        "",
        "Details: j/k select, Enter/o opens, arrows scroll.",
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn render(app: &mut App, width: u16, height: u16) -> String {
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
        let mut app = crate::app::test_app();
        let screen = render(&mut app, 120, 36);
        assert!(screen.contains("Home"));
        assert!(screen.contains("Notifications"));
        assert!(screen.contains("Alice"));
        assert!(screen.contains("Hello from Alice"));
        assert!(screen.contains("Media (1)"));
        assert!(screen.contains("Image preview"));
    }

    #[test]
    fn narrow_terminal_stacks_timeline_detail_and_notifications() {
        let mut app = crate::app::test_app();
        let screen = render(&mut app, 60, 30);
        assert!(screen.contains("Home"));
        assert!(screen.contains("Notifications"));
        assert!(screen.contains("Hello from Alice"));
        assert!(screen.contains("Image preview"));
    }

    #[test]
    fn notifications_stay_visible_and_can_be_selected() {
        let mut app = crate::app::test_app();
        let mut notification = app.tabs[0].items[0].clone();
        notification.author = "Bob".into();
        notification.acct = "bob@example.com".into();
        notification.content = "New mention".into();
        app.tabs[1].items.push(notification);
        let screen = render(&mut app, 120, 36);
        assert!(screen.contains("Hello from Alice"));
        assert!(screen.contains("New mention"));
        app.active = 1;
        let screen = render(&mut app, 120, 36);
        assert!(screen.contains("Hello from Alice"));
        assert!(screen.matches("New mention").count() >= 2);
    }

    #[test]
    fn compose_overlay_shows_multiline_draft_and_retention_hint() {
        let mut app = crate::app::test_app();
        app.mode = Mode::Compose;
        app.input = "First line\nSecond line".into();
        let screen = render(&mut app, 100, 36);
        assert!(screen.contains("New post"));
        assert!(screen.contains("Body · editing"));
        assert!(screen.contains("Attachments"));
        assert!(screen.contains("0 / 4 images"));
        assert!(screen.contains("First line"));
        assert!(screen.contains("Second line"));
        assert!(screen.contains("Esc: keep draft"));
        assert!(screen.contains("Ctrl-V"));
    }
    #[test]
    fn wraps_japanese_within_columns_and_keeps_first_140_characters() {
        use unicode_width::UnicodeWidthStr;
        let text = format!("{}終", "あ".repeat(140));
        let lines = wrapped_lines(&text, 13, 140);
        assert!(lines
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= 13));
        assert_eq!(lines.concat(), format!("{}…", "あ".repeat(140)));
        let newlines = wrapped_lines("一行目\n二行目", 20, 140);
        assert_eq!(newlines, ["一行目", "二行目"]);
    }

    #[test]
    fn draft_cursor_uses_japanese_display_width_and_tracks_wrapping() {
        let text = "あいAう";
        let (lines, row, col) = draft_layout(text, "あいA".len(), 5);
        assert_eq!(lines, ["あいA", "う"]);
        assert_eq!((row, col), (1, 0));
        let (_, row, col) = draft_layout("あい\n次", "あい\n".len(), 8);
        assert_eq!((row, col), (1, 0));
    }
    #[test]
    fn detail_popup_shows_post_metadata_link_preview_and_selectable_urls() {
        let mut app = crate::app::test_app();
        let post = &mut app.tabs[0].items[0];
        post.details = "public · 3 replies".into();
        post.link_preview = Some("Example title\nExample description".into());
        post.links = vec!["https://example.test/path".into()];
        app.mode = Mode::Detail;
        let screen = render(&mut app, 120, 40);
        assert!(screen.contains("Post details"));
        assert!(screen.contains("public · 3 replies"));
        assert!(screen.contains("Example title"));
        assert!(screen.contains("https://example.test/path"));
        assert!(screen.contains("Enter/o open"));
    }
}
