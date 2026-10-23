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
    /// Where the payment provider sends the buyer after paying (`PUBLIC_REDIRECT_URL`).
    /// Fixed by configuration, never taken from a request, and carries no personal data.
    pub public_redirect_url: String,
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
    let bytes = STANDARD
        .decode(raw.trim())
        .with_context(|| format!("{name} must be base64"))?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("{name} must decode to exactly 32 bytes"))
}

/// Environments in which OTP codes may be echoed to stdout.
const DEV_ECHO_ENVS: &[&str] = &["development", "test"];

/// `PUBLIC_REDIRECT_URL` default, used only in development and test.
pub const DEV_REDIRECT_URL: &str = "http://localhost:8080/checkout/return";

/// The payment return URL: optional (with a localhost default) in development and
/// test, required and https everywhere else.
fn redirect_url(raw: Option<String>, dev: bool) -> Result<String> {
    let url = match raw.map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        Some(url) => url,
        None if dev => return Ok(DEV_REDIRECT_URL.into()),
        None => bail!("PUBLIC_REDIRECT_URL is required outside development and test"),
    };
    let ok = if dev {
        url.starts_with("https://") || url.starts_with("http://")
    } else {
        url.starts_with("https://")
    };
    if !ok || url.contains(char::is_whitespace) || url.contains('?') {
        bail!("PUBLIC_REDIRECT_URL must be an https URL without a query string");
    }
    Ok(url)
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Builds the config from any variable source (the process environment in production).
    pub fn from_lookup(var: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let app_env = var("APP_ENV")
            .unwrap_or_else(|| "development".into())
            .trim()
            .to_lowercase();
        let otp_dev_echo = var("OTP_DEV_ECHO").is_some_and(|v| v.trim() == "1");
        if otp_dev_echo && !DEV_ECHO_ENVS.contains(&app_env.as_str()) {
            bail!("OTP_DEV_ECHO=1 is only allowed when APP_ENV is development or test");
        }
        let key = |name: &str| {
            key_from_b64(
                name,
                &var(name).with_context(|| format!("{name} is required"))?,
            )
        };
        let field_key = key("FIELD_KEY")?;
        let index_key = key("INDEX_KEY")?;
        if field_key == index_key {
            bail!("FIELD_KEY and INDEX_KEY must differ");
        }
        let public_redirect_url = redirect_url(
            var("PUBLIC_REDIRECT_URL"),
            DEV_ECHO_ENVS.contains(&app_env.as_str()),
        )?;
        Ok(Self {
            public_redirect_url,
            database_url: var("DATABASE_URL").context("DATABASE_URL is required")?,
            bind_addr: var("BIND_ADDR").unwrap_or_else(|| "127.0.0.1:8080".into()),
            app_env,
            field_key,
            index_key,
            otp_dev_echo,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const KEY_A: &str = "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc="; // 32 x 0x07
    const KEY_B: &str = "CQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQk="; // 32 x 0x09

    fn load(pairs: &[(&str, &str)]) -> Result<Config> {
        let mut env: HashMap<&str, &str> = [
            ("DATABASE_URL", "postgres://x"),
            ("FIELD_KEY", KEY_A),
            ("INDEX_KEY", KEY_B),
            (
                "PUBLIC_REDIRECT_URL",
                "https://shop.example/checkout/return",
            ),
        ]
        .into();
        env.extend(pairs.iter().copied());
        Config::from_lookup(|k| env.get(k).map(|v| v.to_string()))
    }

    #[test]
    fn dev_echo_allowed_only_in_development_or_test() {
        for env in ["development", "test", " Development ", "TEST"] {
            let cfg = load(&[("APP_ENV", env), ("OTP_DEV_ECHO", "1")]).expect(env);
            assert!(cfg.otp_dev_echo);
        }
        for env in ["production", "Production ", "staging", "prod", ""] {
            let err = load(&[("APP_ENV", env), ("OTP_DEV_ECHO", "1")]).unwrap_err();
            assert!(err.to_string().contains("OTP_DEV_ECHO"), "{env}: {err}");
        }
        // Echo off is fine anywhere.
        assert!(!load(&[("APP_ENV", "production")]).unwrap().otp_dev_echo);
    }

    #[test]
    fn app_env_is_trimmed_and_lowercased() {
        assert_eq!(
            load(&[("APP_ENV", " Production ")]).unwrap().app_env,
            "production"
        );
        assert_eq!(load(&[]).unwrap().app_env, "development");
    }

    #[test]
    fn redirect_url_defaults_only_in_development() {
        let dev = load(&[("PUBLIC_REDIRECT_URL", "")]).unwrap();
        assert_eq!(dev.public_redirect_url, DEV_REDIRECT_URL);
        let err = load(&[("APP_ENV", "production"), ("PUBLIC_REDIRECT_URL", "")]).unwrap_err();
        assert!(err.to_string().contains("PUBLIC_REDIRECT_URL"), "{err}");
        // Production needs https and no query string.
        for bad in [
            "http://shop.example/r",
            "https://shop.example/r?x=1",
            "ftp://x",
        ] {
            assert!(
                load(&[("APP_ENV", "production"), ("PUBLIC_REDIRECT_URL", bad)]).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            load(&[("APP_ENV", "production")])
                .unwrap()
                .public_redirect_url,
            "https://shop.example/checkout/return"
        );
    }

    #[test]
    fn identical_field_and_index_keys_refused() {
        let err = load(&[("INDEX_KEY", KEY_A)]).unwrap_err();
        assert!(
            err.to_string()
                .contains("FIELD_KEY and INDEX_KEY must differ"),
            "{err}"
        );
    }
}
