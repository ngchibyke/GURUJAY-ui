use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    pub app_env: String,
    pub field_key: [u8; 32],
    pub index_key: [u8; 32],
    pub otp_dev_echo: bool,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind_addr", &self.bind_addr)
            .field("app_env", &self.app_env)
            .finish_non_exhaustive()
    }
}

/// Decodes a standard-base64 string that must be exactly 32 bytes.
fn key_from_b64(name: &str, raw: &str) -> Result<[u8; 32]> {
    let bytes = STANDARD.decode(raw.trim()).with_context(|| format!("{name} must be base64"))?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| anyhow::anyhow!("{name} must decode to exactly 32 bytes"))
}

fn key_from_env(name: &str) -> Result<[u8; 32]> {
    key_from_b64(name, &std::env::var(name).with_context(|| format!("{name} is required"))?)
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let app_env = std::env::var("APP_ENV").unwrap_or_else(|_| "development".into());
        let otp_dev_echo = std::env::var("OTP_DEV_ECHO").map(|v| v == "1").unwrap_or(false);
        if otp_dev_echo && app_env == "production" {
            bail!("OTP_DEV_ECHO=1 is not allowed when APP_ENV=production");
        }
        Ok(Self {
            database_url: std::env::var("DATABASE_URL").context("DATABASE_URL is required")?,
            bind_addr: std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into()),
            app_env,
            field_key: key_from_env("FIELD_KEY")?,
            index_key: key_from_env("INDEX_KEY")?,
            otp_dev_echo,
        })
    }
}
