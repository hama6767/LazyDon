use crate::app::{App, Mode, ViewItem};
use crate::settings::{Density, Theme};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy)]
struct Palette {
    background: Color,
    text: Color,
    muted: Color,
    accent: Color,
    accent_text: Color,
    selection: Color,
    compose: Color,
    warning: Color,
    light: bool,
}

impl Palette {
    fn new(theme: Theme) -> Self {
        match theme {
            Theme::Dark => Self {
                background: Color::Rgb(18, 22, 29),
                text: Color::White,
                muted: Color::Gray,
                accent: Color::Cyan,
                accent_text: Color::Black,
                selection: Color::Rgb(34, 48, 57),
                compose: Color::Rgb(25, 31, 39),
                warning: Color::Yellow,
                light: false,
            },
            Theme::Light => Self {
                background: Color::Rgb(245, 247, 250),
                text: Color::Rgb(24, 32, 44),
                muted: Color::Rgb(83, 94, 108),
                accent: Color::Rgb(0, 100, 125),
                accent_text: Color::White,
                selection: Color::Rgb(201, 226, 235),
                compose: Color::Rgb(230, 237, 244),
                warning: Color::Rgb(130, 75, 0),
                light: true,
            },
            Theme::HighContrast => Self {
                background: Color::Black,
                text: Color::White,
                muted: Color::Gray,
                accent: Color::LightCyan,
                accent_text: Color::Black,
                selection: Color::Rgb(55, 55, 90),
                compose: Color::Rgb(20, 20, 20),
                warning: Color::Yellow,
                light: false,
            },
        }
    }
}

/// Draw the interface from the current application state.
pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();
    let p = Palette::new(app.settings.theme);
    frame.render_widget(
        Block::default().style(Style::default().fg(p.text).bg(p.background)),
        area,
    );
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
                .fg(p.accent_text)
                .bg(p.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(&app.account, Style::default().fg(p.accent)),
        Span::raw("  ·  "),
        Span::styled(&app.instance, Style::default().fg(p.muted)),
    ]))
    .block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(p.muted)),
    );
    frame.render_widget(header, rows[0]);

    // Reserve a short strip across the post and notification panes for the
    // system monitor. On compact terminals it is placed below all three panes.
    if rows[1].width >= 105 {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(42),
                Constraint::Percentage(30),
                Constraint::Percentage(28),
            ])
            .split(rows[1]);
        draw_timeline(frame, app, columns[0], p);
        let right = Rect::new(
            columns[1].x,
            rows[1].y,
            columns[1].width + columns[2].width,
            rows[1].height,
        );
        let monitor_height = if right.height >= 12 { 4 } else { 0 };
        let top_height = right.height.saturating_sub(monitor_height);
        draw_detail(
            frame,
            app,
            Rect::new(columns[1].x, columns[1].y, columns[1].width, top_height),
            p,
        );
        draw_notifications(
            frame,
            app,
            Rect::new(columns[2].x, columns[2].y, columns[2].width, top_height),
            p,
        );
        if monitor_height > 0 {
            draw_metrics(
                frame,
                Rect::new(right.x, right.y + top_height, right.width, monitor_height),
                p,
            );
        }
    } else {
        let monitor_height = if rows[1].height >= 20 && rows[1].width >= 30 {
            4
        } else {
            0
        };
        let main = Rect::new(
            rows[1].x,
            rows[1].y,
            rows[1].width,
            rows[1].height.saturating_sub(monitor_height),
        );
        let panels = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(40),
                Constraint::Percentage(32),
                Constraint::Percentage(28),
            ])
            .split(main);
        draw_timeline(frame, app, panels[0], p);
        draw_detail(frame, app, panels[1], p);
        draw_notifications(frame, app, panels[2], p);
        if monitor_height > 0 {
            draw_metrics(
                frame,
                Rect::new(
                    rows[1].x,
                    main.y + main.height,
                    rows[1].width,
                    monitor_height,
                ),
                p,
            );
        }
    }

    let footer = Paragraph::new(vec![
        Line::from(Span::styled(
            &app.status,
            Style::default().fg(p.warning),
        )),
        Line::from(Span::styled(
            "j/k move  h/l tab  Enter details  a add  c compose  f favourite  b boost  s settings  ? help  q quit",
            Style::default().fg(p.muted),
        )),
    ]);
    frame.render_widget(footer, rows[2]);

    match app.mode {
        Mode::Normal => {}
        Mode::Help => draw_popup(frame, " Help ", help_text(), 76, 75, p),
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
            p,
        ),
        Mode::Compose => draw_compose(frame, app, p),
        Mode::Detail => draw_detail_popup(frame, app, p),
        Mode::Settings => draw_settings(frame, app, p),
    }
}

fn draw_timeline(frame: &mut Frame<'_>, app: &App, area: Rect, p: Palette) {
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
            Paragraph::new("No timelines").block(panel(" Timelines ", true, p)),
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
                .fg(p.accent_text)
                .bg(p.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.muted)
        };
        tab_spans.push(Span::styled(format!(" {} ", tab.title), style));
        tab_spans.push(Span::raw(" "));
    }
    frame.render_widget(
        Paragraph::new(Line::from(tab_spans)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Timelines ")
                .border_style(Style::default().fg(p.muted)),
        ),
        sections[0],
    );

    let width = sections[1].width.saturating_sub(3) as usize;
    let items: Vec<ListItem<'_>> = tab
        .items
        .iter()
        .map(|item| item_row(item, width, p, app.settings.density))
        .collect();
    let title = format!(" {} · {} ", tab.title, tab.items.len());
    let timeline_focused = timeline_index == Some(app.active);
    let list = List::new(items)
        .block(panel(&title, timeline_focused, p))
        .highlight_style(Style::default().bg(p.selection))
        .highlight_symbol(if timeline_focused { "▌" } else { " " });
    let mut state = ListState::default();
    if !tab.items.is_empty() {
        state.select(Some(tab.selected.min(tab.items.len() - 1)));
    }
    frame.render_stateful_widget(list, sections[1], &mut state);
    if tab.items.is_empty() {
        let inner = panel("", true, p).inner(sections[1]);
        frame.render_widget(
            Paragraph::new("No posts yet. Press r to refresh.")
                .style(Style::default().fg(p.muted))
                .wrap(Wrap { trim: true }),
            inner,
        );
    }
}

fn draw_notifications(frame: &mut Frame<'_>, app: &App, area: Rect, p: Palette) {
    let Some((index, tab)) = app
        .tabs
        .iter()
        .enumerate()
        .find(|(_, tab)| tab.title == "Notifications")
    else {
        frame.render_widget(
            Paragraph::new("Notifications unavailable").block(panel(" Notifications ", false, p)),
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
            .map(|item| notification_row(item, width, p, app.settings.density))
            .collect::<Vec<_>>(),
    )
    .block(panel(&title, focused, p))
    .highlight_style(Style::default().bg(p.selection))
    .highlight_symbol(if focused { "▌" } else { " " });
    let mut state = ListState::default();
    if !tab.items.is_empty() {
        state.select(Some(tab.selected.min(tab.items.len() - 1)));
    }
    frame.render_stateful_widget(list, area, &mut state);
    if tab.items.is_empty() {
        let inner = panel("", focused, p).inner(area);
        frame.render_widget(
            Paragraph::new("No notifications yet.")
                .style(Style::default().fg(p.muted))
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

fn mention_spans(text: &str, p: Palette) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let starts_mention = chars[i] == '@'
            && i + 1 < chars.len()
            && (chars[i + 1].is_ascii_alphanumeric() || chars[i + 1] == '_')
            && (i == 0 || !(chars[i - 1].is_alphanumeric() || chars[i - 1] == '_'));
        if starts_mention {
            if !plain.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut plain),
                    Style::default().fg(p.text),
                ));
            }
            let start = i;
            i += 1;
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric() || matches!(chars[i], '_' | '-' | '.' | '@'))
            {
                i += 1;
            }
            let mention: String = chars[start..i].iter().collect();
            let account = mention.trim_start_matches('@').trim_end_matches('.');
            spans.push(Span::styled(
                mention.clone(),
                Style::default()
                    .fg(account_color(account, p))
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            plain.push(chars[i]);
            i += 1;
        }
    }
    if !plain.is_empty() {
        spans.push(Span::styled(plain, Style::default().fg(p.text)));
    }
    spans
}

fn body_lines(text: &str, width: usize, p: Palette) -> Vec<Line<'static>> {
    wrapped_lines(text, width.saturating_sub(5), 140)
        .into_iter()
        .map(|part| {
            let mut spans = vec![Span::styled("  │  ", Style::default().fg(p.muted))];
            spans.extend(mention_spans(&part, p));
            Line::from(spans)
        })
        .collect()
}

fn notification_row(item: &ViewItem, width: usize, p: Palette, density: Density) -> ListItem<'_> {
    let event = if item.context == "notification" {
        item.content.as_str()
    } else {
        item.context.split(" ♥").next().unwrap_or(&item.context)
    };
    let mut lines = vec![Line::from(Span::styled(
        event.to_owned(),
        Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
    ))];
    lines.extend(body_lines(&item.content, width, p));
    lines.push(Line::from(Span::styled(
        format!("  └─ @{} · {}", item.acct, item.timestamp),
        Style::default().fg(p.muted),
    )));
    if density == Density::Comfortable {
        lines.push(Line::from(""));
    }
    ListItem::new(lines)
}

fn item_row(item: &ViewItem, width: usize, p: Palette, density: Density) -> ListItem<'_> {
    let color = account_color(&item.acct, p);
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
        Span::styled(format!(" @{}", item.acct), Style::default().fg(p.muted)),
    ])];
    lines.extend(body_lines(&item.content, width, p));
    lines.push(Line::from(Span::styled(
        format!("  └─ {}{}{}", item.timestamp, context, media),
        Style::default().fg(p.muted),
    )));
    if density == Density::Comfortable {
        lines.push(Line::from(""));
    }
    ListItem::new(lines)
}

fn draw_detail(frame: &mut Frame<'_>, app: &mut App, area: Rect, p: Palette) {
    let Some(item) = app
        .tabs
        .get(app.active)
        .and_then(|tab| tab.items.get(tab.selected))
    else {
        frame.render_widget(
            Paragraph::new("Select a post to see its details.")
                .style(Style::default().fg(p.muted))
                .block(panel(" Post ", false, p)),
            area,
        );
        return;
    };
    let color = account_color(&item.acct, p);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("● ", Style::default().fg(color)),
            Span::styled(
                item.author.as_str(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" @{}", item.acct), Style::default().fg(p.muted)),
        ]),
        Line::from(Span::styled(
            format!("{}  {}", item.timestamp, item.context),
            Style::default().fg(p.muted),
        )),
        Line::from(""),
    ];
    lines.extend(
        item.content
            .lines()
            .map(|line| Line::from(mention_spans(line, p))),
    );
    let block = panel(" Post ", false, p);
    let inner = block.inner(area);
    if let Some(preview) = &item.link_preview {
        lines.extend(
            wrapped_lines(preview, inner.width.max(2) as usize, 200)
                .into_iter()
                .take(2)
                .map(|line| Line::from(Span::styled(line, Style::default().fg(p.accent)))),
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
                .style(Style::default().fg(p.accent)),
            sections[1],
        );
        // Avoid emitting image graphics under a modal, where they could obscure it.
        if app.mode == Mode::Normal && sections[2].width > 0 && sections[2].height > 0 {
            if let Some(preview) = &mut app.native_preview {
                preview.render(frame, sections[2]);
            } else {
                frame.render_widget(
                    Paragraph::new("Image preview unavailable").style(Style::default().fg(p.muted)),
                    sections[2],
                );
            }
        }
    }
}

fn draw_compose(frame: &mut Frame<'_>, app: &App, p: Palette) {
    let area = centered_rect(80, 75, frame.area());
    frame.render_widget(Clear, area);
    let block = panel(" New post ", true, p);
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
            Span::styled("Posting as ", Style::default().fg(p.muted)),
            Span::styled(&app.account, Style::default().fg(p.accent)),
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
            .block(panel(" Body · editing ", true, p))
            .style(
                Style::default()
                    .fg(if app.input.is_empty() {
                        p.muted
                    } else {
                        p.text
                    })
                    .bg(p.compose),
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
                Span::styled("Attachments  ", Style::default().fg(p.muted)),
                Span::raw(format!("{} / 4 images", app.attachment_count())),
                Span::styled("    ·    Characters  ", Style::default().fg(p.muted)),
                Span::raw(app.input.chars().count().to_string()),
            ]),
            Line::from(Span::styled(&app.status, Style::default().fg(p.warning))),
        ]),
        sections[2],
    );
    frame.render_widget(
        Paragraph::new("Ctrl-S: send  ·  Ctrl-V: attach image  ·  Esc: keep draft")
            .style(Style::default().fg(p.accent))
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

fn draw_detail_popup(frame: &mut Frame<'_>, app: &mut App, p: Palette) {
    let Some(item) = app
        .tabs
        .get(app.active)
        .and_then(|tab| tab.items.get(tab.selected))
    else {
        return;
    };
    let area = centered_rect(88, 88, frame.area());
    frame.render_widget(Clear, area);
    let block = panel(" Post details · Esc close ", true, p);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                item.author.as_str(),
                Style::default()
                    .fg(account_color(&item.acct, p))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" @{}", item.acct), Style::default().fg(p.muted)),
        ]),
        Line::from(Span::styled(
            format!("{}  {}", item.timestamp, item.context),
            Style::default().fg(p.muted),
        )),
        Line::from(""),
    ];
    if !item.details.is_empty() {
        lines.push(Line::from(Span::styled(
            item.details.as_str(),
            Style::default().fg(p.muted),
        )));
    }
    lines.extend(
        item.content
            .split('\n')
            .map(|s| Line::from(mention_spans(s, p))),
    );
    if let Some(preview) = &item.link_preview {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Link preview",
            Style::default().fg(p.accent),
        )));
        lines.extend(preview.lines().map(|s| Line::from(s.to_owned())));
    }
    if !item.attachments.is_empty() || !item.links.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Media and links · j/k select · Enter/o open",
            Style::default().fg(p.accent),
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
                    Style::default().fg(p.warning).add_modifier(Modifier::BOLD)
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
                    Style::default().fg(p.warning).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                },
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ PgUp/PgDown scroll · Esc close",
        Style::default().fg(p.muted),
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
        let block = panel(" Media preview ", false, p);
        let image_area = block.inner(area);
        frame.render_widget(block, area);
        if image_area.width > 0 && image_area.height > 0 {
            preview.render(frame, image_area);
        }
    }
}

fn draw_metrics(frame: &mut Frame<'_>, area: Rect, p: Palette) {
    let metrics = crate::metrics::current();
    let bar_width = (area.width as usize).saturating_sub(30).clamp(4, 40);
    fn bar(value: Option<f64>, width: usize) -> String {
        let filled = value
            .map(|v| ((v.clamp(0.0, 100.0) / 100.0) * width as f64).round() as usize)
            .unwrap_or(0);
        format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
    }
    fn pct(value: Option<f64>) -> String {
        value
            .map(|v| format!("{:>5.1}%", v.clamp(0.0, 100.0)))
            .unwrap_or_else(|| "   --%".into())
    }
    let memory = match (metrics.memory_used_bytes, metrics.memory_total_bytes) {
        (Some(used), Some(total)) => format!(
            " {:.1}/{:.1} GiB",
            used as f64 / 1073741824.0,
            total as f64 / 1073741824.0
        ),
        _ => String::new(),
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(" CPU ", Style::default().fg(p.muted)),
            Span::styled(
                bar(metrics.cpu_percent, bar_width),
                Style::default().fg(p.accent),
            ),
            Span::raw(format!(" {}", pct(metrics.cpu_percent))),
        ]),
        Line::from(vec![
            Span::styled(" RAM ", Style::default().fg(p.muted)),
            Span::styled(
                bar(metrics.memory_percent, bar_width),
                Style::default().fg(p.accent),
            ),
            Span::raw(format!(" {}{}", pct(metrics.memory_percent), memory)),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(panel(" System ", false, p)),
        area,
    );
}

fn draw_settings(frame: &mut Frame<'_>, app: &App, p: Palette) {
    let area = centered_rect(72, 55, frame.area());
    frame.render_widget(Clear, area);
    let value = app
        .font_candidate
        .map(|n| format!("{n} pt (Kitty)"))
        .unwrap_or_else(|| "Terminal default".into());
    let values = [
        ("Theme", app.settings.theme.label().to_owned()),
        ("Density", app.settings.density.label().to_owned()),
        ("Font size", value),
    ];
    let mut lines = vec![
        Line::from(""),
        Line::from(" j/k select · h/l change · Enter apply · Esc close"),
        Line::from(""),
    ];
    for (i, (label, value)) in values.into_iter().enumerate() {
        let selected = i == app.settings_index;
        let style = if selected {
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(p.text)
        };
        lines.push(Line::from(Span::styled(
            format!(
                " {} {:<12}  {}",
                if selected { "▸" } else { " " },
                label,
                value
            ),
            style,
        )));
    }
    lines.extend([
        Line::from(""),
        Line::from(Span::styled(
            " 0: terminal default",
            Style::default().fg(p.muted),
        )),
        Line::from(Span::styled(
            " Font changes need Kitty remote control and affect the OS",
            Style::default().fg(p.muted),
        )),
        Line::from(Span::styled(
            " window, including splits. Other terminals use their settings.",
            Style::default().fg(p.muted),
        )),
    ]);
    let block = panel(" Settings ", true, p);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        sections[0],
    );
    frame.render_widget(
        Paragraph::new(app.settings_message.as_str()).style(Style::default().fg(p.warning)),
        sections[1],
    );
}

fn panel(title: &str, active: bool, p: Palette) -> Block<'_> {
    Block::default()
        .title(title.to_string())
        .style(Style::default().fg(p.text).bg(p.background))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if active { p.accent } else { p.muted }))
}

fn draw_popup(
    frame: &mut Frame<'_>,
    title: &str,
    lines: Vec<Line<'_>>,
    width: u16,
    height: u16,
    p: Palette,
) {
    let area = centered_rect(width, height, frame.area());
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(panel(title, true, p))
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
        "s               Settings",
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

fn account_color(account: &str, p: Palette) -> Color {
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
    const LIGHT_COLORS: [Color; 8] = [
        Color::Rgb(0, 98, 123),
        Color::Rgb(0, 104, 68),
        Color::Rgb(128, 76, 0),
        Color::Rgb(137, 43, 130),
        Color::Rgb(25, 75, 160),
        Color::Rgb(34, 107, 38),
        Color::Rgb(160, 45, 55),
        Color::Rgb(122, 47, 138),
    ];
    if p.light {
        LIGHT_COLORS[hash as usize % LIGHT_COLORS.len()]
    } else {
        COLORS[hash as usize % COLORS.len()]
    }
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
    fn monitor_and_settings_are_visible() {
        let mut app = crate::app::test_app();
        let screen = render(&mut app, 120, 36);
        assert!(screen.contains("System"));
        assert!(screen.contains("CPU"));
        assert!(screen.contains("RAM"));
        app.mode = Mode::Settings;
        app.settings_index = 2;
        app.font_candidate = Some(11);
        app.status = "Font change failed".into();
        let screen = render(&mut app, 120, 36);
        assert!(screen.contains("Settings"));
        assert!(screen.contains("Theme"));
        assert!(screen.contains("Density"));
        assert!(screen.contains("11 pt (Kitty)"));
        assert!(screen.contains("including splits"));
        assert!(screen.contains("0: terminal default"));
        assert!(screen.contains("Font change failed"));
    }

    #[test]
    fn themes_and_density_change_visuals_without_touching_content() {
        let mut app = crate::app::test_app();
        let backend = TestBackend::new(120, 36);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let dark = terminal.backend().buffer()[(119, 35)].bg;
        app.settings.theme = Theme::Light;
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let light = terminal.backend().buffer()[(119, 35)].bg;
        assert_ne!(dark, light);
        let item = &app.tabs[0].items[0];
        let p = Palette::new(Theme::Light);
        assert_eq!(
            item_row(item, 40, p, Density::Comfortable).height(),
            item_row(item, 40, p, Density::Compact).height() + 1
        );
    }

    #[test]
    fn mentions_have_account_color_and_do_not_color_email_addresses() {
        let p = Palette::new(Theme::Dark);
        let spans = mention_spans("こんにちは @siruserver mail@example.org", p);
        let mention = spans.iter().find(|s| s.content == "@siruserver").unwrap();
        assert_eq!(mention.style.fg, Some(account_color("siruserver", p)));
        assert!(spans
            .iter()
            .any(|s| s.content.contains("mail@example.org") && s.style.fg == Some(p.text)));
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
