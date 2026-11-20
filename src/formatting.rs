//! Convert Mastodon HTML to readable post text without link footnotes.
//!
//! The rich renderer keeps link targets as annotations instead of inserting
//! Markdown-style reference markers. Link destinations are extracted
//! separately for the detail view.

/// Render post HTML as text. Layout is left to the terminal UI, so avoid
/// inserting artificial line breaks at ordinary timeline widths.
pub fn plain(html: &str) -> String {
    const RENDER_WIDTH: usize = 16_384;
    match html2text::from_read_rich(html.as_bytes(), RENDER_WIDTH) {
        Ok(lines) => {
            let text = lines
                .into_iter()
                .map(|line| {
                    line.tagged_strings()
                        .map(|part| part.s.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            sanitize(&text).trim().to_owned()
        }
        // Invalid or overly complex HTML is still shown as safe terminal text.
        Err(_) => sanitize(html).trim().to_owned(),
    }
}

fn sanitize(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\n' => Some('\n'),
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::plain;

    #[test]
    fn mastodon_mentions_have_no_reference_numbers_or_footnotes() {
        let html = r#"<p>こんにちは <span class="h-card"><a href="https://example.social/@siruserver" class="u-url mention">@<span>siruserver</span></a></span> さん！</p>"#;
        assert_eq!(plain(html), "こんにちは @siruserver さん！");
    }

    #[test]
    fn anchor_text_is_readable_and_entities_are_decoded() {
        let html = r#"<p>こちらは <a href="https://example.org/?a=1&amp;b=2">記事 &amp; 写真</a> です。</p>"#;
        assert_eq!(plain(html), "こちらは 記事 & 写真 です。");
    }

    #[test]
    fn keeps_paragraphs_and_line_breaks_without_terminal_controls() {
        let html = "<p>一行目<br>二行目\u{1b}[31m</p><p>次の段落</p>";
        let text = plain(html);
        assert!(text.contains("一行目\n二行目"), "{text:?}");
        assert!(text.contains("次の段落"));
        assert!(!text.contains('\u{1b}'));
    }

    #[test]
    fn does_not_wrap_ordinary_long_lines() {
        let body = "あ".repeat(200);
        assert_eq!(plain(&format!("<p>{body}</p>")), body);
    }
}
