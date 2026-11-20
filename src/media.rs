//! Attachment previews and clipboard image support.
//!
//! Chafa converts images to colored Unicode cells, which Ratatui can render
//! without emitting arbitrary terminal graphics escapes into its screen buffer.
use anyhow::{anyhow, bail, Context, Result};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PREVIEW_BYTES: usize = 128 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
pub type Preview = Vec<Line<'static>>;

struct TempImage(PathBuf);

impl Drop for TempImage {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn temporary_image(bytes: &[u8]) -> Result<TempImage> {
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "mastodon-preview-{}-{sequence}.img",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                let temp = TempImage(path);
                file.write_all(bytes)
                    .context("プレビュー画像を保存できません")?;
                return Ok(temp);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err).context("プレビュー画像用の一時ファイルを作れません"),
        }
    }
    bail!("プレビュー画像用の一時ファイルを作れません")
}

/// Download and render an attachment as colored Unicode character art.
/// Call from a worker thread; network requests and image conversion are blocking.
pub fn preview(url: &str) -> Result<Preview> {
    let parsed = reqwest::Url::parse(url).context("画像 URL が不正です")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("画像 URL は HTTP または HTTPS である必要があります");
    }
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("画像取得の準備に失敗しました")?;
    let response = client
        .get(parsed)
        .send()
        .context("画像を取得できません")?
        .error_for_status()
        .context("画像サーバーがエラーを返しました")?;
    if response
        .content_length()
        .is_some_and(|n| n > MAX_IMAGE_BYTES as u64)
    {
        bail!("プレビュー画像が大きすぎます（最大 16 MiB）");
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("プレビュー画像を読み込めません")?;
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!("プレビュー画像が大きすぎます（最大 16 MiB）");
    }
    let temp = temporary_image(&bytes)?;
    let mut command = Command::new("chafa");
    // Some terminals expose only an indexed palette. Chafa does the palette
    // conversion itself in that case; modern truecolor terminals retain RGB.
    let term = std::env::var("TERM").unwrap_or_default();
    let colorterm = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let colors =
        if term.ends_with("256color") && !matches!(colorterm.as_str(), "truecolor" | "24bit") {
            "256"
        } else {
            "full"
        };
    command
        .args([
            "--format",
            "symbols",
            "--colors",
            colors,
            "--symbols",
            "block",
            "--size",
            "32x16",
            "--optimize",
            "0",
            "--duration",
            "0",
        ])
        .arg(&temp.0);
    let output = bounded_output(&mut command, MAX_PREVIEW_BYTES)
        .context("画像プレビューには chafa が必要です（インストール後に再試行してください）")?;
    let output = String::from_utf8(output).context("プレビューを文字列に変換できません")?;
    let lines = parse_chafa(&output);
    if lines.is_empty() {
        bail!("画像プレビューを生成できませんでした");
    }
    Ok(lines)
}

/// Interpret only SGR color sequences, never pass terminal control bytes on to
/// the UI. Chafa emits resets and either RGB or indexed foreground/background.
fn parse_chafa(output: &str) -> Preview {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut style = Style::default();
    let mut chars = output.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => {
                if !text.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut text), style));
                }
                if chars.peek() == Some(&'[') {
                    chars.next();
                    let mut params = String::new();
                    // CSI is bounded; ignore every non-SGR control sequence.
                    for c in chars.by_ref() {
                        if ('\x40'..='\x7e').contains(&c) {
                            if c == 'm' {
                                style = sgr_style(style, &params);
                            }
                            break;
                        }
                        if params.len() < 128 {
                            params.push(c);
                        }
                    }
                }
            }
            '\n' => {
                if !text.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut text), style));
                }
                lines.push(Line::from(std::mem::take(&mut spans)));
                if lines.len() == 16 {
                    break;
                }
            }
            c if !c.is_control() => text.push(c),
            _ => {}
        }
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    if !spans.is_empty() && lines.len() < 16 {
        lines.push(Line::from(spans));
    }
    lines
}

fn sgr_style(mut style: Style, params: &str) -> Style {
    let values: Option<Vec<u16>> = if params.is_empty() {
        Some(vec![0])
    } else {
        params.split(';').map(|v| v.parse::<u16>().ok()).collect()
    };
    let Some(values) = values else { return style };
    let mut i = 0;
    while i < values.len() {
        match values[i] {
            0 => style = Style::default(),
            39 => style = style.fg(Color::Reset),
            49 => style = style.bg(Color::Reset),
            which @ (38 | 48) if values.get(i + 1) == Some(&2) && i + 4 < values.len() => {
                let rgb = &values[i + 2..i + 5];
                if rgb.iter().all(|&n| n <= 255) {
                    let color = Color::Rgb(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8);
                    style = if which == 38 {
                        style.fg(color)
                    } else {
                        style.bg(color)
                    };
                }
                i += 4;
            }
            which @ (38 | 48) if values.get(i + 1) == Some(&5) && i + 2 < values.len() => {
                if values[i + 2] <= 255 {
                    let color = Color::Indexed(values[i + 2] as u8);
                    style = if which == 38 {
                        style.fg(color)
                    } else {
                        style.bg(color)
                    };
                }
                i += 2;
            }
            _ => {}
        }
        i += 1;
    }
    style
}

/// Read a PNG image from a Wayland or X11 clipboard. No shell is involved.
pub fn clipboard_png() -> Result<Vec<u8>> {
    let mut errors = Vec::new();
    for (program, args) in [
        ("wl-paste", &["--type", "image/png"][..]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"][..],
        ),
    ] {
        let mut command = Command::new(program);
        command.args(args);
        match bounded_output(&mut command, MAX_IMAGE_BYTES) {
            Ok(bytes) if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => return Ok(bytes),
            Ok(_) => errors.push(format!("{program}: 画像が PNG ではありません")),
            Err(err) => errors.push(format!("{program}: {err}")),
        }
    }
    Err(anyhow!("クリップボードの PNG 画像を読み取れません。画像をコピーし、Wayland では wl-paste、X11 では xclip を利用できるようにしてください（{}）", errors.join("; ")))
}

/// Read at most `limit` bytes; kill the child on overflow to avoid unbounded
/// memory use and avoid waiting on a pipe that no longer has a reader.
fn bounded_output(command: &mut Command, limit: usize) -> Result<Vec<u8>> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("コマンドを起動できません")?;
    let mut output = Vec::new();
    let read_result = child
        .stdout
        .take()
        .expect("stdout was piped")
        .take(limit as u64 + 1)
        .read_to_end(&mut output);
    if read_result.is_err() || output.len() > limit {
        let _ = child.kill();
        let _ = child.wait();
        read_result.context("コマンドの出力を読み取れません")?;
        bail!("画像データが大きすぎます");
    }
    let status = child.wait().context("コマンドの終了を待てません")?;
    if !status.success() {
        bail!("コマンドが失敗しました ({status})");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_keeps_rgb_foreground_and_background() {
        let lines = parse_chafa("\x1b[38;2;12;34;56;48;2;78;90;123m▀\x1b[0m!\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content, "▀");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(12, 34, 56)));
        assert_eq!(lines[0].spans[0].style.bg, Some(Color::Rgb(78, 90, 123)));
        assert_eq!(lines[0].spans[1].style, Style::default());
    }

    #[test]
    fn preview_supports_indexed_colors_and_ignores_controls() {
        let lines = parse_chafa("\x1b[38;5;120;48;5;42m█\x1b[2JX\x07\n");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(120)));
        assert_eq!(lines[0].spans[0].style.bg, Some(Color::Indexed(42)));
        assert_eq!(
            lines[0]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>(),
            "█X"
        );
    }
}
