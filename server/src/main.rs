use std::sync::Arc;

use anyhow::{Context, bail};
use server::{
    AppState,
    auth::{
        otp::LogOtpSender,
        password::{blocking, hash_password, validate_password},
        routes::{insert_user, validate_display_name},
    },
    build_app,
    config::Config,
    crypto::FieldCipher,
    phone::normalize_ng_phone,
};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let config = Config::from_env()?;
    let db = PgPoolOptions::new().max_connections(10).connect(&config.database_url).await?;
    sqlx::migrate!("./migrations").run(&db).await?;

    let state = AppState {
        db,
        cipher: Arc::new(FieldCipher::new(config.field_key)),
        index_key: Arc::new(config.index_key),
        otp: Arc::new(LogOtpSender { echo: config.otp_dev_echo }),
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => serve(state, &config).await,
        Some("create-owner") => create_owner(state, &args[1..]).await,
        Some(other) => bail!("unknown command: {other}"),
    }
}

async fn serve(state: AppState, config: &Config) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "listening");
    axum::serve(listener, build_app(state)).await?;
    Ok(())
}

fn flag(args: &[String], name: &str) -> anyhow::Result<String> {
    let pos = args.iter().position(|a| a == name).with_context(|| format!("missing {name}"))?;
    args.get(pos + 1).cloned().with_context(|| format!("missing value for {name}"))
}

/// `server create-owner --phone <phone> --name <name>`; password is read without echo.
async fn create_owner(state: AppState, args: &[String]) -> anyhow::Result<()> {
    let phone = normalize_ng_phone(&flag(args, "--phone")?).map_err(|e| anyhow::anyhow!("{e}"))?;
    let name = validate_display_name(&flag(args, "--name")?).map_err(|e| anyhow::anyhow!("{e}"))?;
    let password = rpassword::prompt_password("Password: ")?;
    validate_password(&password).map_err(|e| anyhow::anyhow!("{e}"))?;

    let hash = blocking(move || hash_password(&password)).await.map_err(|e| anyhow::anyhow!("{e}"))?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let id = insert_user(&state, &phone, &name, &hash).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    sqlx::query("UPDATE users SET role = 'owner', phone_verified_at = now() WHERE id = $1")
        .bind(id)
        .execute(&state.db)
        .await?;
    println!("{id}");
    Ok(())
}
