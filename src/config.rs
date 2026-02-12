use anyhow::{Context, Result};
use std::path::PathBuf;

pub struct Config {
    pub api_id: i32,
    pub api_hash: String,
    pub phone_number: Option<String>,
    pub session_path: PathBuf,
    pub state_path: PathBuf,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let api_id: i32 = std::env::var("TG_API_ID")
            .context("TG_API_ID must be set")?
            .parse()
            .context("TG_API_ID must be a valid integer")?;

        let api_hash =
            std::env::var("TG_API_HASH").context("TG_API_HASH must be set")?;

        let phone_number = std::env::var("TG_PHONE_NUMBER").ok();

        let default_dir = dirs_default();
        let session_path = std::env::var("TG_SESSION_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_dir.join("session.sqlite"));

        let state_path = std::env::var("TG_STATE_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_dir.join("state.json"));

        Ok(Config {
            api_id,
            api_hash,
            phone_number,
            session_path,
            state_path,
        })
    }

    /// Ensure parent directories exist for session and state files.
    pub fn ensure_dirs(&self) -> Result<()> {
        if let Some(parent) = self.session_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create session directory")?;
        }
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create state directory")?;
        }
        Ok(())
    }
}

fn dirs_default() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".telegram_dup_checker")
}
