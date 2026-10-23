use std::sync::OnceLock;

use anyhow::anyhow;
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};

use crate::error::AppError;

pub const MIN_PASSWORD_LEN: usize = 10;
pub const MAX_PASSWORD_LEN: usize = 128;

pub fn validate_password(password: &str) -> Result<(), AppError> {
    let len = password.chars().count();
    if len < MIN_PASSWORD_LEN {
        return Err(AppError::Validation(format!(
            "Password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    if len > MAX_PASSWORD_LEN {
        return Err(AppError::Validation(format!(
            "Password must be at most {MAX_PASSWORD_LEN} characters"
        )));
    }
    Ok(())
}

/// Argon2id with the crate's default parameters, PHC string output.
pub fn hash_password(password: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(anyhow!("password hashing failed: {e}")))
}

pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// Burns the same Argon2 work as a real verification (used for unknown accounts)
/// so response time does not reveal whether an account exists.
pub fn dummy_verify(password: &str) {
    static DUMMY: OnceLock<String> = OnceLock::new();
    let phc = DUMMY.get_or_init(|| hash_password("dummy-password-for-timing").expect("hash dummy"));
    let _ = verify_password(password, phc);
}

/// Runs a CPU-heavy closure off the async runtime threads.
pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, AppError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::Internal(anyhow!("blocking task failed: {e}")))
}
