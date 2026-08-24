//! RFC 6238 TOTP (HMAC-SHA1, 6 digits, 30s step, skew ±1).

use qrcode::QrCode;
use totp_rs::{Algorithm, Secret, TOTP};

use super::MfaError;

pub const KIND: &str = "totp";
const ISSUER: &str = "Peckboard";
const DIGITS: usize = 6;
/// Window skew is applied in [`verify`], not inside totp-rs `check` — that
/// would re-accept a just-used timestep via a neighbouring window.
const SKEW: u8 = 0;
const STEP: u64 = 30;
pub struct EnrollMaterial {
    pub secret_b32: String,
    pub otpauth_url: String,
    pub qr_svg: String,
}

fn sanitize_account(account: &str) -> String {
    let trimmed = account.trim();
    if trimmed.is_empty() {
        return "user".into();
    }
    trimmed.replace(':', "-")
}

fn totp_for(secret_bytes: Vec<u8>, account: &str) -> Result<TOTP, MfaError> {
    TOTP::new(
        Algorithm::SHA1,
        DIGITS,
        SKEW,
        STEP,
        secret_bytes,
        Some(ISSUER.to_string()),
        sanitize_account(account),
    )
    .map_err(|e| MfaError::Internal(anyhow::anyhow!("totp: {e}")))
}

fn secret_bytes(secret_b32: &str) -> Result<Vec<u8>, MfaError> {
    Secret::Encoded(secret_b32.trim().to_string())
        .to_bytes()
        .map_err(|e| MfaError::Internal(anyhow::anyhow!("totp secret: {e}")))
}

/// Fresh TOTP secret + otpauth URL + QR SVG for enrollment.
pub fn enroll_material(account: &str) -> Result<EnrollMaterial, MfaError> {
    let secret = Secret::generate_secret();
    let secret_b32 = secret.to_encoded().to_string();
    let bytes = secret
        .to_bytes()
        .map_err(|e| MfaError::Internal(anyhow::anyhow!("totp secret: {e}")))?;
    let totp = totp_for(bytes, account)?;
    let otpauth_url = totp.get_url();
    let qr_svg = qr_svg(&otpauth_url);
    Ok(EnrollMaterial {
        secret_b32,
        otpauth_url,
        qr_svg,
    })
}

fn qr_svg(otpauth_url: &str) -> String {
    match QrCode::new(otpauth_url.as_bytes()) {
        Ok(code) => code
            .render::<qrcode::render::svg::Color>()
            .min_dimensions(160, 160)
            .build(),
        Err(_) => String::new(),
    }
}

/// Verify `code` against `secret_b32`. Returns the matching timestep so
/// the caller can persist it and reject replays. `last_timestep` is the
/// previously accepted window (inclusive); any window at or before it is
/// skipped.
pub fn verify(
    secret_b32: &str,
    code: &str,
    last_timestep: Option<i64>,
    now_unix: u64,
) -> Result<i64, MfaError> {
    let digits: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() != DIGITS {
        return Err(MfaError::InvalidCode);
    }
    let totp = totp_for(secret_bytes(secret_b32)?, "verify")?;
    let current = (now_unix / STEP) as i64;
    let last = last_timestep.unwrap_or(-1);
    for delta in [-1, 0, 1] {
        let ts = current + delta;
        if ts < 0 || ts <= last {
            continue;
        }
        let window_unix = (ts as u64).saturating_mul(STEP);
        if totp.check(&digits, window_unix) {
            return Ok(ts);
        }
    }
    Err(MfaError::InvalidCode)
}

/// Current TOTP code for tests / e2e helpers that already know the secret.
#[cfg(test)]
pub fn generate_now(secret_b32: &str, now_unix: u64) -> Result<String, MfaError> {
    let totp = totp_for(secret_bytes(secret_b32)?, "verify")?;
    Ok(totp.generate(now_unix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_then_verify_in_window() {
        let mat = enroll_material("alice").unwrap();
        let now = 1_700_000_000u64;
        let code = generate_now(&mat.secret_b32, now).unwrap();
        assert_eq!(code.len(), 6);
        let ts = verify(&mat.secret_b32, &code, None, now).unwrap();
        assert!(ts > 0);
    }

    #[test]
    fn reject_wrong_code() {
        let mat = enroll_material("alice").unwrap();
        let err = verify(&mat.secret_b32, "000000", None, 1_700_000_000).unwrap_err();
        assert!(matches!(err, MfaError::InvalidCode));
    }

    #[test]
    fn reject_replay_of_same_timestep() {
        let mat = enroll_material("alice").unwrap();
        let now = 1_700_000_030u64;
        let code = generate_now(&mat.secret_b32, now).unwrap();
        let ts = verify(&mat.secret_b32, &code, None, now).unwrap();
        let err = verify(&mat.secret_b32, &code, Some(ts), now).unwrap_err();
        assert!(matches!(err, MfaError::InvalidCode));
    }

    #[test]
    fn otpauth_url_names_issuer_and_account() {
        let mat = enroll_material("alice").unwrap();
        assert!(mat.otpauth_url.starts_with("otpauth://totp/"));
        assert!(mat.otpauth_url.contains("Peckboard"));
        assert!(mat.otpauth_url.contains("alice"));
        assert!(mat.qr_svg.contains("<svg"));
    }
}
