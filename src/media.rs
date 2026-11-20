//! Attachment previews and clipboard image support.
//!
//! Previews use Chafa's plain Unicode output so they work inside a TUI without
//! depending on a particular terminal's graphics protocol.
use anyhow::{anyhow, bail, Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PREVIEW_BYTES: usize = 128 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

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

/// Download and render an attachment as small, uncolored Unicode character art.
/// Call from a worker thread; network requests and image conversion are blocking.
pub fn preview(url: &str) -> Result<Vec<String>> {
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
    command
        .args([
            "--format",
            "symbols",
            "--colors",
            "none",
            "--size",
            "40x12",
            "--duration",
            "0",
        ])
        .arg(&temp.0);
    let output = bounded_output(&mut command, MAX_PREVIEW_BYTES)
        .context("画像プレビューには chafa が必要です（インストール後に再試行してください）")?;
    let output = String::from_utf8(output).context("プレビューを文字列に変換できません")?;
    let lines: Vec<String> = output
        .lines()
        .take(12)
        .map(|line| line.chars().filter(|c| !c.is_control()).collect())
        .collect();
    if lines.is_empty() {
        bail!("画像プレビューを生成できませんでした");
    }
    Ok(lines)
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
