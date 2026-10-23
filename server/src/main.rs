use std::sync::Arc;

use anyhow::{Context, bail};
use server::{
    AppState,
    auth::{
        otp::LogOtpSender,
        password::{blocking, hash_password, validate_password},
        routes::{create_owner_account, validate_display_name},
    },
    build_app,
    config::Config,
    crypto::FieldCipher,
    phone::normalize_ng_phone,
};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

/// How long open connections get to finish after a shutdown signal.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug)]
enum Command {
    /// `server` or `server serve`
    Serve,
    /// `server create-owner --phone <phone> --name <name>` (flags kept as given)
    CreateOwner(Vec<String>),
}

fn parse_command(args: &[String]) -> anyhow::Result<Command> {
    match args.split_first() {
        None => Ok(Command::Serve),
        Some((cmd, rest)) if cmd == "serve" => {
            if !rest.is_empty() {
                bail!("serve takes no arguments");
            }
            Ok(Command::Serve)
        }
        Some((cmd, rest)) if cmd == "create-owner" => Ok(Command::CreateOwner(rest.to_vec())),
        // The value is not echoed: it could be anything a user typed.
        Some(_) => bail!("unknown command (expected: serve, create-owner)"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = parse_command(&args)?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env()?;
    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect(&config.database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&db).await?;

    let state = AppState {
        db,
        cipher: Arc::new(FieldCipher::new(config.field_key)),
        index_key: Arc::new(config.index_key),
        otp: Arc::new(LogOtpSender {
            echo: config.otp_dev_echo,
        }),
    };

    match command {
        Command::Serve => serve(state, &config).await,
        Command::CreateOwner(flags) => create_owner(state, &flags).await,
    }
}

async fn serve(state: AppState, config: &Config) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(&config.bind_addr).await?;
    tracing::info!(addr = %config.bind_addr, "listening");
    let db = state.db.clone();
    let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, build_app(state)).with_graceful_shutdown(async move {
        shutdown_signal().await;
        tracing::info!("shutdown signal received, draining connections");
        let _ = signalled_tx.send(());
    });
    // Idle keep-alive connections can stall the drain forever, so bound it once the signal fires.
    tokio::select! {
        res = server => res?,
        _ = async {
            if signalled_rx.await.is_ok() {
                tokio::time::sleep(SHUTDOWN_GRACE).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => tracing::warn!("drain timed out, shutting down with connections still open"),
    }
    db.close().await;
    tracing::info!("shut down");
    Ok(())
}

/// Resolves on Ctrl-C or (on unix) SIGTERM so in-flight requests can finish.
async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

fn flag(args: &[String], name: &str) -> anyhow::Result<String> {
    let pos = args
        .iter()
        .position(|a| a == name)
        .with_context(|| format!("missing {name}"))?;
    args.get(pos + 1)
        .cloned()
        .with_context(|| format!("missing value for {name}"))
}

/// `server create-owner --phone <phone> --name <name>`; password is read without echo.
async fn create_owner(state: AppState, args: &[String]) -> anyhow::Result<()> {
    let phone = normalize_ng_phone(&flag(args, "--phone")?).map_err(|e| anyhow::anyhow!("{e}"))?;
    let name = validate_display_name(&flag(args, "--name")?).map_err(|e| anyhow::anyhow!("{e}"))?;
    let password = rpassword::prompt_password("Password: ")?;
    validate_password(&password).map_err(|e| anyhow::anyhow!("{e}"))?;

    let hash = blocking(move || hash_password(&password))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let id = create_owner_account(&state, &phone, &name, &hash)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{id}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_argument_and_serve_both_start_the_server() {
        assert!(matches!(parse_command(&args(&[])).unwrap(), Command::Serve));
        assert!(matches!(
            parse_command(&args(&["serve"])).unwrap(),
            Command::Serve
        ));
    }

    #[test]
    fn create_owner_keeps_its_flags() {
        let cmd = parse_command(&args(&["create-owner", "--phone", "080", "--name", "N"])).unwrap();
        let Command::CreateOwner(rest) = cmd else {
            panic!("expected create-owner")
        };
        assert_eq!(rest, args(&["--phone", "080", "--name", "N"]));
    }

    #[test]
    fn unknown_command_and_extra_serve_args_rejected() {
        assert!(parse_command(&args(&["bogus"])).is_err());
        assert!(parse_command(&args(&["serve", "now"])).is_err());
    }
}
