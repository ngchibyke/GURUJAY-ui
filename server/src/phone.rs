use crate::error::AppError;

fn invalid() -> AppError {
    AppError::Unprocessable("invalid_phone", "Enter a valid Nigerian phone number".to_string())
}

/// Normalizes a Nigerian mobile number to E.164 (`+234XXXXXXXXXX`).
/// Accepts `0` + 10 digits, `234` + 10 digits or `+234` + 10 digits, ignoring spaces and dashes.
/// The 10 national digits must start with 7, 8 or 9.
pub fn normalize_ng_phone(raw: &str) -> Result<String, AppError> {
    let cleaned: String = raw.chars().filter(|c| *c != ' ' && *c != '-').collect();
    let national = if let Some(rest) = cleaned.strip_prefix("+234") {
        rest
    } else if let Some(rest) = cleaned.strip_prefix("234") {
        rest
    } else if let Some(rest) = cleaned.strip_prefix('0') {
        rest
    } else {
        return Err(invalid());
    };
    let valid = national.len() == 10
        && national.bytes().all(|b| b.is_ascii_digit())
        && matches!(national.as_bytes()[0], b'7' | b'8' | b'9');
    if valid { Ok(format!("+234{national}")) } else { Err(invalid()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_formats() {
        for raw in ["08031234567", "+2348031234567", "2348031234567", "0803 123 4567", "0803-123-4567"] {
            assert_eq!(normalize_ng_phone(raw).unwrap(), "+2348031234567");
        }
    }
    #[test]
    fn rejects_bad_numbers() {
        for raw in ["", "12345", "0803123456", "080312345678", "+447911123456", "08O31234567", "05031234567"] {
            assert!(normalize_ng_phone(raw).is_err());
        }
    }
    #[test]
    fn error_does_not_echo_input() {
        match normalize_ng_phone("08O31234567") {
            Err(AppError::Unprocessable("invalid_phone", msg)) => assert!(!msg.contains("08O3")),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
