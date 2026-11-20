//! User interface preferences, stored separately from the account token.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    HighContrast,
}
impl Theme {
    pub fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::HighContrast,
            Self::HighContrast => Self::Dark,
        }
    }
    pub fn previous(self) -> Self {
        match self {
            Self::Dark => Self::HighContrast,
            Self::Light => Self::Dark,
            Self::HighContrast => Self::Light,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "Dark",
            Self::Light => "Light",
            Self::HighContrast => "High contrast",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Density {
    #[default]
    Compact,
    Comfortable,
}
impl Density {
    pub fn next(self) -> Self {
        match self {
            Self::Compact => Self::Comfortable,
            Self::Comfortable => Self::Compact,
        }
    }
    pub fn previous(self) -> Self {
        self.next()
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Comfortable => "Comfortable",
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct Settings {
    pub theme: Theme,
    pub density: Density,
    /// None leaves the terminal's font size untouched; Some is a Kitty size in points.
    pub font_size: Option<u8>,
}
impl Settings {
    pub fn path() -> Result<PathBuf> {
        let base = env::var_os("XDG_CONFIG_HOME")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .context("Cannot locate settings; set HOME or XDG_CONFIG_HOME")?;
        Ok(base.join("lazydon").join("settings.json"))
    }
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::path()?)
    }
    fn load_from(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => {
                let settings: Self =
                    serde_json::from_slice(&bytes).context("Could not parse display settings")?;
                settings.validate()?;
                Ok(settings)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("Cannot read {}", path.display())),
        }
    }
    fn validate(&self) -> Result<()> {
        if let Some(size) = self.font_size {
            if !(8..=24).contains(&size) {
                bail!("Font size must be between 8 and 24 pt");
            }
        }
        Ok(())
    }
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }
    fn save_to(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path.parent().context("Invalid settings path")?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(path)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            serde_json::to_writer_pretty(&mut file, self)?;
        }
        #[cfg(not(unix))]
        fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }
}

/// Explicitly apply a font size to the current Kitty OS window. This is never
/// called at startup. Kitty must allow remote control; all splits in the same
/// OS window share a font size. Other terminals should use their own settings.
pub fn apply_kitty_font_size(size: Option<u8>) -> Result<()> {
    if env::var_os("KITTY_WINDOW_ID").is_none()
        || env::var_os("TMUX").is_some()
        || env::var_os("STY").is_some()
    {
        bail!("Font control requires Kitty outside tmux/screen; change the size in your terminal instead");
    }
    if let Some(size) = size {
        if !(8..=24).contains(&size) {
            bail!("Font size must be between 8 and 24 pt");
        }
    }
    // Target the controlling terminal, never an unrelated KITTY_LISTEN_ON socket.
    // Silence the child because its diagnostic output must not corrupt the TUI.
    let mut child = Command::new("kitty")
        .args(["@", "set-font-size", "--", &size.unwrap_or(0).to_string()])
        .env_remove("KITTY_LISTEN_ON")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Kitty executable not found")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!("Kitty refused font control; enable remote control for this window");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Kitty font control timed out; check remote control settings");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_round_trip_and_missing_defaults() {
        let dir = env::temp_dir().join(format!("lazydon-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(Settings::load_from(&path).unwrap(), Settings::default());
        let settings = Settings {
            theme: Theme::Light,
            density: Density::Comfortable,
            font_size: Some(11),
        };
        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path).unwrap(), settings);
        let _ = fs::remove_dir_all(dir);
    }
    #[test]
    fn invalid_font_size_is_rejected() {
        assert!(Settings {
            theme: Theme::Dark,
            density: Density::Compact,
            font_size: Some(4)
        }
        .validate()
        .is_err());
    }
}
