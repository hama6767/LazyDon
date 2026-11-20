//! Attachment previews and clipboard image support.
//!
//! Native terminal image protocols with a color half-block fallback.
use anyhow::{anyhow, bail, Context, Result};
use ratatui::{layout::Rect, widgets::Paragraph, Frame};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::StatefulProtocol,
    thread::{ThreadImage, ThreadProtocol},
    Resize,
};
use std::io::{Cursor, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CLIPBOARD_BYTES: usize = 16 * 1024 * 1024;

/// Read a bounded image response off the UI thread.
fn download_image(url: &str) -> Result<Vec<u8>> {
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
        bail!("プレビュー画像が大きすぎます（最大 32 MiB）");
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .context("プレビュー画像を読み込めません")?;
    if bytes.len() > MAX_IMAGE_BYTES {
        bail!("プレビュー画像が大きすぎます（最大 32 MiB）");
    }
    Ok(bytes)
}

/// Detect using environment variables rather than reading terminal responses:
/// probing stdin races with crossterm and may steal Japanese IME input.
pub fn image_picker() -> Picker {
    let font_size = crossterm::terminal::window_size()
        .ok()
        .and_then(|size| {
            if size.columns == 0
                || size.rows == 0
                || size.width < size.columns
                || size.height < size.rows
            {
                None
            } else {
                Some((size.width / size.columns, size.height / size.rows))
            }
        })
        .unwrap_or((10, 20));
    let mut picker = Picker::from_fontsize(font_size);
    picker.set_protocol_type(select_protocol(
        &std::env::var("LAZYDON_IMAGE_PROTOCOL").unwrap_or_default(),
        &std::env::var("TERM").unwrap_or_default(),
        &std::env::var("TERM_PROGRAM").unwrap_or_default(),
    ));
    picker
}

fn select_protocol(override_value: &str, term: &str, program: &str) -> ProtocolType {
    match override_value.to_ascii_lowercase().as_str() {
        "kitty" => return ProtocolType::Kitty,
        "iterm" | "iterm2" => return ProtocolType::Iterm2,
        "sixel" | "sixels" => return ProtocolType::Sixel,
        "halfblocks" | "halfblock" | "unicode" => return ProtocolType::Halfblocks,
        "" | "auto" => {}
        _ => return ProtocolType::Halfblocks,
    }
    let term = term.to_ascii_lowercase();
    let program = program.to_ascii_lowercase();
    if term.starts_with("tmux") || term.starts_with("screen") {
        // Multiplexers vary in graphics passthrough configuration.
        ProtocolType::Halfblocks
    } else if term.contains("kitty") || term.contains("ghostty") || program.contains("ghostty") {
        ProtocolType::Kitty
    } else if program.contains("wezterm") || program.contains("iterm") {
        ProtocolType::Iterm2
    } else if term == "foot" || term.starts_with("foot-") {
        ProtocolType::Sixel
    } else {
        ProtocolType::Halfblocks
    }
}

type ResizeRequest = (StatefulProtocol, Resize, Rect);
enum NativeMessage {
    Loaded(Result<StatefulProtocol, String>),
    Resized(StatefulProtocol),
}

/// A per-selection image. Fetch, decode, resizing and terminal-protocol
/// encoding run in a worker so slow images never block input or rendering.
pub struct NativePreview {
    requests: Sender<ResizeRequest>,
    results: Receiver<NativeMessage>,
    state: Option<ThreadProtocol>,
    error: Option<String>,
}

impl NativePreview {
    pub fn start(picker: Picker, url: String) -> Self {
        let (requests, incoming) = mpsc::channel::<ResizeRequest>();
        let (outgoing, results) = mpsc::channel();
        thread::spawn(move || {
            let loaded = download_image(&url)
                .and_then(|bytes| decode_image(&bytes))
                .map(|image| picker.new_resize_protocol(image))
                .map_err(|e| format!("{e:#}"));
            let ok = loaded.is_ok();
            if outgoing.send(NativeMessage::Loaded(loaded)).is_err() || !ok {
                return;
            }
            while let Ok((mut protocol, resize, area)) = incoming.recv() {
                protocol.resize_encode(&resize, protocol.background_color(), area);
                if outgoing.send(NativeMessage::Resized(protocol)).is_err() {
                    break;
                }
            }
        });
        Self {
            requests,
            results,
            state: None,
            error: None,
        }
    }

    /// Render in the exact allocated cell rectangle. Resize requests are sent
    /// to the worker; subsequent frames show the encoded result when ready.
    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        while let Ok(message) = self.results.try_recv() {
            match message {
                NativeMessage::Loaded(Ok(protocol)) => {
                    self.state = Some(ThreadProtocol::new(self.requests.clone(), protocol));
                }
                NativeMessage::Loaded(Err(error)) => self.error = Some(error),
                NativeMessage::Resized(protocol) => {
                    if let Some(state) = &mut self.state {
                        state.set_protocol(protocol);
                    }
                }
            }
        }
        if let Some(state) = &mut self.state {
            frame.render_stateful_widget(ThreadImage::default(), area, state);
        } else {
            frame.render_widget(
                Paragraph::new(self.error.as_deref().unwrap_or("Loading image…")),
                area,
            );
        }
    }
}

fn decode_image(bytes: &[u8]) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("画像形式を判別できません")?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().context("画像をデコードできません")
}

/// Read a PNG image from a Wayland or X11 clipboard. No shell is involved.
pub fn clipboard_png() -> Result<Vec<u8>> {
    if cfg!(target_os = "macos") {
        bail!("Clipboard image attachments are not supported on macOS");
    }
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
        match bounded_output(&mut command, MAX_CLIPBOARD_BYTES) {
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

    fn check_async_render(protocol: ratatui_image::picker::ProtocolType, expected: &str) {
        use ratatui::{backend::TestBackend, Terminal};
        use std::{
            io::{Read, Write},
            net::TcpListener,
            time::Instant,
        };

        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            128,
            128,
            image::Rgba([12, 34, 56, 255]),
        ));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let bytes = bytes.into_inner();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/image", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            )
            .unwrap();
            stream.write_all(&bytes).unwrap();
        });
        let mut picker = Picker::from_fontsize((10, 20));
        picker.set_protocol_type(protocol);
        let mut preview = NativePreview::start(picker, url);
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            terminal
                .draw(|frame| preview.render(frame, frame.area()))
                .unwrap();
            if terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.symbol().contains(expected))
            {
                break;
            }
            assert!(Instant::now() < deadline, "preview did not render");
            thread::sleep(Duration::from_millis(10));
        }
        server.join().unwrap();
    }

    #[test]
    fn native_preview_downloads_and_renders_without_blocking() {
        check_async_render(ratatui_image::picker::ProtocolType::Halfblocks, "▀");
        check_async_render(ratatui_image::picker::ProtocolType::Kitty, "\x1b_G");
    }

    #[test]
    fn detects_protocol_without_reading_stdin() {
        assert_eq!(select_protocol("", "xterm-kitty", ""), ProtocolType::Kitty);
        assert_eq!(
            select_protocol("", "xterm-256color", "WezTerm"),
            ProtocolType::Iterm2
        );
        assert_eq!(select_protocol("", "foot-extra", ""), ProtocolType::Sixel);
        assert_eq!(
            select_protocol("", "tmux-256color", "WezTerm"),
            ProtocolType::Halfblocks
        );
        assert_eq!(
            select_protocol("sixel", "xterm-256color", ""),
            ProtocolType::Sixel
        );
        assert_eq!(
            select_protocol("typo", "xterm-kitty", ""),
            ProtocolType::Halfblocks
        );
    }

    #[cfg(unix)]
    #[test]
    fn picker_does_not_change_tmux_settings() {
        use std::{fs, os::unix::fs::PermissionsExt};
        if std::env::var_os("LAZYDON_TMUX_TEST_CHILD").is_some() {
            let _ = image_picker();
            return;
        }
        let dir = std::env::temp_dir().join(format!("lazydon-tmux-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let executable = dir.join("tmux");
        let marker = dir.join("invoked");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf called > \"$FAKE_TMUX_MARKER\"\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let result = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("media::tests::picker_does_not_change_tmux_settings")
            .env("LAZYDON_TMUX_TEST_CHILD", "1")
            .env("TERM", "tmux-256color")
            .env("PATH", &dir)
            .env("FAKE_TMUX_MARKER", &marker)
            .output()
            .unwrap();
        let invoked = marker.exists();
        fs::remove_dir_all(dir).unwrap();
        assert!(
            result.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!invoked, "picker unexpectedly invoked tmux");
    }

    #[test]
    fn native_preview_decodes_png_and_rejects_invalid_data() {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            3,
            image::Rgba([12, 34, 56, 255]),
        ));
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let decoded = decode_image(bytes.get_ref()).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (2, 3));
        assert!(decode_image(b"not an image").is_err());
    }
}
