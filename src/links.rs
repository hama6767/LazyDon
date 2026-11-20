//! Post links and the link cards supplied by the Mastodon server.
//!
//! We deliberately do not fetch arbitrary post URLs: the server has already
//! generated link-card metadata, and opening a link is an explicit action.
use anyhow::{bail, Context, Result};
use html2text::render::RichAnnotation;
use serde::Deserialize;
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Card {
    pub url: String,
    pub title: String,
    pub description: String,
    pub provider_name: String,
}

/// Only links safe to pass to a browser. Do not accept userinfo or controls.
pub fn valid_url(url: &str) -> bool {
    if url.len() > 8192 || url.chars().any(char::is_control) {
        return false;
    }
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    matches!(parsed.scheme(), "https" | "http")
        && parsed.host_str().is_some()
        && parsed.username().is_empty()
        && parsed.password().is_none()
}

/// Extract actual link destinations, not just the visually shortened text.
/// HTML parsing and entity decoding are provided by html2text's rich renderer.
pub fn extract(html: &str) -> Vec<String> {
    let mut urls = Vec::new();
    if let Ok(lines) = html2text::from_read_rich(html.as_bytes(), 140) {
        for line in lines {
            for fragment in line.tagged_strings() {
                for annotation in &fragment.tag {
                    if let RichAnnotation::Link(url) = annotation {
                        if valid_url(url) && !urls.contains(url) && urls.len() < 32 {
                            urls.push(url.clone());
                        }
                    }
                }
            }
        }
    }
    urls
}

/// Remove terminal controls and collapse whitespace from untrusted card text.
pub fn display_text(value: &str, max_chars: usize) -> String {
    let clean: String = value
        .chars()
        .map(|c| {
            if c.is_control() || c.is_whitespace() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = clean.chars();
    let result: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{result}…")
    } else {
        result
    }
}

/// Open a selected link or media URL in the user's configured desktop app.
/// The command is invoked directly, without shell interpolation.
pub fn open(url: &str) -> Result<()> {
    if !valid_url(url) {
        bail!("Only HTTP(S) URLs without credentials can be opened");
    }
    let mut child = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Could not start xdg-open")?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Copy the complete URL without relying on how the terminal wraps it.
pub fn copy_to_clipboard(url: &str) -> Result<()> {
    if !valid_url(url) {
        bail!("Only HTTP(S) URLs without credentials can be copied");
    }
    let candidates: &[(&str, &[&str])] =
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])];
    for &(program, args) in candidates {
        if copy_with_command(program, args, url).is_ok() {
            return Ok(());
        }
    }
    bail!("Could not copy URL; install wl-copy (Wayland) or xclip (X11)")
}

fn copy_with_command(program: &str, args: &[&str], url: &str) -> Result<()> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let write_result = child.stdin.take().unwrap().write_all(url.as_bytes());
    let status = child.wait()?;
    write_result?;
    if !status.success() {
        bail!("Clipboard command failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_real_destinations_and_decodes_entities() {
        let html = r#"<p><a href="https://example.org/path?a=1&amp;b=2"><span>example.org/…</span></a> <a href="https://example.org/path?a=1&amp;b=2">again</a> <a href="javascript:alert(1)">bad</a></p>"#;
        assert_eq!(extract(html), vec!["https://example.org/path?a=1&b=2"]);
    }

    #[test]
    fn accepts_only_web_urls_without_credentials_or_controls() {
        assert!(valid_url("https://example.org/watch?v=42"));
        assert!(!valid_url("file:///etc/passwd"));
        assert!(!valid_url("https://user:password@example.org/"));
        assert!(!valid_url("https://example.org/\x1b[2J"));
        assert!(!valid_url("--help"));
    }

    #[test]
    fn clipboard_receives_the_complete_url_without_shell_interpolation() {
        let url = "https://example.org/oauth/authorize?client_id=a%2Bb&scope=read+write";
        copy_with_command(
            "sh",
            &["-c", "value=$(cat); [ \"$value\" = 'https://example.org/oauth/authorize?client_id=a%2Bb&scope=read+write' ]"],
            url,
        )
        .unwrap();
        assert!(copy_to_clipboard("https://example.org/\nmalicious").is_err());
    }

    #[test]
    fn card_text_is_single_line_and_bounded() {
        assert_eq!(display_text("  hello\nworld\x1b  ", 8), "hello wo…");
    }

    #[test]
    fn card_can_be_deserialized_from_mastodon_json() {
        let card: Card = serde_json::from_str(r#"{"url":"https://example.org","title":"Example","description":"A page","provider_name":"Example.org","type":"link"}"#).unwrap();
        assert_eq!(card.title, "Example");
        assert_eq!(card.provider_name, "Example.org");
    }
}
