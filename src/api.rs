//! Mastodon HTTP and local credential handling.
use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{multipart, Client, RequestBuilder, Response};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    env, fs,
    io::Read,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

const REDIRECT: &str = "urn:ietf:wg:oauth:2.0:oob";
const SCOPES: &str = "read write:statuses write:media write:favourites write:bookmarks";

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    pub instance: String,
    pub access_token: String,
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        let base = env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .context(
                "設定フォルダーを特定できません。HOME または XDG_CONFIG_HOME を設定してください",
            )?;
        Ok(base.join("lazydon").join("config.json"))
    }

    pub fn load() -> Result<Option<Self>> {
        let path = Self::path()?;
        let legacy_path = path
            .parent()
            .and_then(|p| p.parent())
            .context("設定パスが不正です")?
            .join("tootui")
            .join("config.json");
        Self::load_from(&path, &legacy_path)
    }

    fn load_from(path: &std::path::Path, legacy_path: &std::path::Path) -> Result<Option<Self>> {
        match Self::read_config(path)? {
            Some(config) => Ok(Some(config)),
            None => Self::read_config(legacy_path),
        }
    }

    fn read_config(path: &std::path::Path) -> Result<Option<Self>> {
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("設定ファイルの読み込みに失敗しました")?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("設定を読み込めません: {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path()?;
        let parent = path.parent().context("設定パスが不正です")?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            let mut file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&path)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            serde_json::to_writer_pretty(&mut file, self)?;
        }
        #[cfg(not(unix))]
        fs::write(&path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Authorization {
    pub instance: String,
    pub client_id: String,
    pub client_secret: String,
    pub authorize_url: String,
}

#[derive(Deserialize)]
struct AppRegistration {
    client_id: String,
    client_secret: String,
}
#[derive(Deserialize)]
struct Token {
    access_token: String,
}

fn http_client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("LazyDon/", env!("CARGO_PKG_VERSION")))
        // Never forward credentials or OAuth client secrets to a redirect target.
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()?)
}

/// Require HTTPS for real servers so credentials cannot be sent over plaintext.
pub fn normalize_instance(input: &str) -> Result<String> {
    let input = input.trim().trim_end_matches('/');
    let input = if input.contains("://") {
        input.to_owned()
    } else {
        format!("https://{input}")
    };
    let url = reqwest::Url::parse(&input).context("サーバー URL が不正です")?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("サーバー URL にはホスト名だけを指定してください");
    }
    let local = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
    );
    if url.scheme() != "https" && !(url.scheme() == "http" && local) {
        bail!("サーバー URL は HTTPS を使用してください");
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn decode<T: DeserializeOwned>(response: Response) -> Result<T> {
    const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
    let status = response.status();
    let mut bytes = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("サーバーの応答を読み取れませんでした")?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        bail!("サーバーの応答が大きすぎます");
    }
    if !status.is_success() {
        let message = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
            .unwrap_or_else(|| String::from_utf8_lossy(&bytes).chars().take(240).collect());
        bail!("Mastodon API {status}: {message}");
    }
    serde_json::from_slice(&bytes).context("サーバーの応答を解析できませんでした")
}

pub fn register_app(instance: &str) -> Result<Authorization> {
    let instance = normalize_instance(instance)?;
    let client = http_client()?;
    let app: AppRegistration = decode(
        client
            .post(format!("{instance}/api/v1/apps"))
            .form(&[
                ("client_name", "LazyDon"),
                ("redirect_uris", REDIRECT),
                ("scopes", SCOPES),
            ])
            .send()
            .context("アプリの登録に失敗しました")?,
    )?;
    let mut url = reqwest::Url::parse(&format!("{instance}/oauth/authorize"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &app.client_id)
        .append_pair("redirect_uri", REDIRECT)
        .append_pair("scope", SCOPES);
    Ok(Authorization {
        instance,
        client_id: app.client_id,
        client_secret: app.client_secret,
        authorize_url: url.to_string(),
    })
}

pub fn exchange_code(auth: &Authorization, code: &str) -> Result<Config> {
    let token: Token = decode(
        http_client()?
            .post(format!("{}/oauth/token", auth.instance))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &auth.client_id),
                ("client_secret", &auth.client_secret),
                ("redirect_uri", REDIRECT),
                ("code", code.trim()),
            ])
            .send()
            .context("認証コードの交換に失敗しました")?,
    )?;
    Ok(Config {
        instance: auth.instance.clone(),
        access_token: token.access_token,
    })
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Account {
    pub id: String,
    pub username: String,
    pub acct: String,
    pub display_name: String,
    pub url: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct MediaAttachment {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub url: Option<String>,
    pub preview_url: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Status {
    pub id: String,
    pub created_at: String,
    pub in_reply_to_id: Option<String>,
    pub sensitive: bool,
    pub spoiler_text: String,
    pub visibility: String,
    pub url: Option<String>,
    pub content: String,
    pub account: Account,
    pub media_attachments: Vec<MediaAttachment>,
    pub card: Option<crate::links::Card>,
    pub reblog: Option<Box<Status>>,
    pub replies_count: u64,
    pub reblogs_count: u64,
    pub favourites_count: u64,
    pub favourited: Option<bool>,
    pub reblogged: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Notification {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub created_at: String,
    pub account: Account,
    pub status: Option<Status>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timeline {
    Home,
    Local,
    Federated,
}

#[derive(Clone)]
pub struct Mastodon {
    client: Client,
    pub config: Config,
}

impl Mastodon {
    pub fn new(mut config: Config) -> Result<Self> {
        config.instance = normalize_instance(&config.instance)?;
        if config.access_token.trim().is_empty() {
            bail!("アクセストークンが空です");
        }
        Ok(Self {
            client: http_client()?,
            config,
        })
    }
    fn get(&self, path: &str) -> RequestBuilder {
        self.client
            .get(format!("{}{path}", self.config.instance))
            .bearer_auth(&self.config.access_token)
    }
    fn post_request(&self, path: &str) -> RequestBuilder {
        self.client
            .post(format!("{}{path}", self.config.instance))
            .bearer_auth(&self.config.access_token)
    }
    pub fn verify_credentials(&self) -> Result<Account> {
        decode(self.get("/api/v1/accounts/verify_credentials").send()?)
    }
    pub fn timeline(&self, timeline: Timeline) -> Result<Vec<Status>> {
        let (path, local) = match timeline {
            Timeline::Home => ("/api/v1/timelines/home", None),
            Timeline::Local => ("/api/v1/timelines/public", Some("true")),
            Timeline::Federated => ("/api/v1/timelines/public", Some("false")),
        };
        let mut req = self.get(path).query(&[("limit", "40")]);
        if let Some(local) = local {
            req = req.query(&[("local", local)]);
        }
        decode(req.send().context("タイムラインを取得できませんでした")?)
    }
    pub fn notifications(&self) -> Result<Vec<Notification>> {
        decode(
            self.get("/api/v1/notifications")
                .query(&[("limit", "40")])
                .send()
                .context("通知を取得できませんでした")?,
        )
    }
    pub fn post(&self, text: &str, media_ids: &[String], reply_to: Option<&str>) -> Result<Status> {
        let mut fields = vec![("status", text.to_owned())];
        for id in media_ids {
            fields.push(("media_ids[]", id.clone()));
        }
        if let Some(id) = reply_to {
            fields.push(("in_reply_to_id", id.to_owned()));
        }
        decode(
            self.post_request("/api/v1/statuses")
                .form(&fields)
                .send()
                .context("投稿できませんでした")?,
        )
    }
    pub fn upload_media(&self, bytes: Vec<u8>, filename: &str) -> Result<MediaAttachment> {
        if bytes.is_empty() {
            bail!("画像が空です");
        }
        let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "image/png"
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            "image/gif"
        } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
            "image/webp"
        } else {
            return Err(anyhow!("PNG、JPEG、GIF、WebP の画像を選択してください"));
        };
        let file = multipart::Part::bytes(bytes)
            .file_name(filename.to_owned())
            .mime_str(mime)?;
        let attachment: MediaAttachment = decode(
            self.post_request("/api/v2/media")
                .multipart(multipart::Form::new().part("file", file))
                .send()
                .context("画像をアップロードできませんでした")?,
        )?;
        // v2 media can return before processing completes. Do not post until
        // it is ready; bound the total wait and each HTTP request.
        wait_for_media(
            attachment,
            Duration::from_secs(60),
            Duration::from_secs(1),
            |id, remaining| {
                decode(
                    self.get(&format!("/api/v1/media/{id}"))
                        .timeout(remaining.min(Duration::from_secs(20)))
                        .send()
                        .context("画像の処理状況を取得できませんでした")?,
                )
            },
        )
    }
    /// `action` is deliberately limited to Mastodon status actions, never an arbitrary path.
    pub fn action(&self, status_id: &str, action: &str) -> Result<Status> {
        if status_id.is_empty() || !status_id.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("投稿 ID が不正です");
        }
        if !matches!(
            action,
            "favourite" | "unfavourite" | "reblog" | "unreblog" | "bookmark" | "unbookmark"
        ) {
            bail!("未対応の操作です");
        }
        decode(
            self.post_request(&format!("/api/v1/statuses/{status_id}/{action}"))
                .send()?,
        )
    }
}

fn wait_for_media(
    mut attachment: MediaAttachment,
    timeout: Duration,
    interval: Duration,
    mut fetch: impl FnMut(&str, Duration) -> Result<MediaAttachment>,
) -> Result<MediaAttachment> {
    let deadline = Instant::now() + timeout;
    while attachment.url.as_deref().is_none_or(str::is_empty) {
        if attachment.id.is_empty() || !attachment.id.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("アップロードされた画像の ID が不正です");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("画像の処理が時間内に完了しませんでした。再度お試しください");
        }
        thread::sleep(remaining.min(interval));
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("画像の処理が時間内に完了しませんでした。再度お試しください");
        }
        attachment = fetch(&attachment.id, remaining)?;
    }
    Ok(attachment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_uses_legacy_only_when_new_config_is_missing() {
        let dir = std::env::temp_dir().join(format!(
            "lazydon-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let current = dir.join("current.json");
        let legacy = dir.join("legacy.json");
        fs::write(
            &legacy,
            r#"{"instance":"https://old.example","access_token":"old"}"#,
        )
        .unwrap();
        assert_eq!(
            Config::load_from(&current, &legacy)
                .unwrap()
                .unwrap()
                .access_token,
            "old"
        );
        fs::write(
            &current,
            r#"{"instance":"https://new.example","access_token":"new"}"#,
        )
        .unwrap();
        assert_eq!(
            Config::load_from(&current, &legacy)
                .unwrap()
                .unwrap()
                .access_token,
            "new"
        );
        fs::write(&current, "invalid").unwrap();
        assert!(Config::load_from(&current, &legacy).is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn server_url_validation_blocks_insecure_and_path_urls() {
        assert_eq!(
            normalize_instance(" mastodon.social/ ").unwrap(),
            "https://mastodon.social"
        );
        assert_eq!(
            normalize_instance("http://localhost:3000").unwrap(),
            "http://localhost:3000"
        );
        assert!(normalize_instance("http://example.com").is_err());
        assert!(normalize_instance("https://user@example.com").is_err());
        assert!(normalize_instance("https://example.com/path").is_err());
    }

    #[test]
    fn deserialize_reblog_and_notification_media() {
        let notification: Notification = serde_json::from_str(r#"{
            "id":"1", "type":"reblog", "account":{"id":"a", "acct":"alice"},
            "status":{"id":"2", "reblog":{"id":"3", "account":{"acct":"bob"},
                "media_attachments":[{"id":"4", "type":"image", "url":"https://example.com/img.png"}]}}
        }"#).unwrap();
        let original = notification.status.unwrap().reblog.unwrap();
        assert_eq!(original.account.acct, "bob");
        assert_eq!(
            original.media_attachments[0].url.as_deref(),
            Some("https://example.com/img.png")
        );
    }

    #[test]
    fn waits_for_async_media_before_returning() {
        let pending = MediaAttachment {
            id: "42".into(),
            ..Default::default()
        };
        let mut requests = 0;
        let result = wait_for_media(pending, Duration::from_secs(2), Duration::ZERO, |id, _| {
            assert_eq!(id, "42");
            requests += 1;
            Ok(MediaAttachment {
                id: id.into(),
                url: (requests == 2).then(|| "https://example.com/image.png".into()),
                ..Default::default()
            })
        })
        .unwrap();
        assert_eq!(requests, 2);
        assert_eq!(result.url.as_deref(), Some("https://example.com/image.png"));
        let pending = MediaAttachment {
            id: "42".into(),
            ..Default::default()
        };
        assert!(wait_for_media(
            pending,
            Duration::ZERO,
            Duration::ZERO,
            |_, _| unreachable!()
        )
        .is_err());
    }
}
