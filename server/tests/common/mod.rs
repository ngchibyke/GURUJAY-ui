#![allow(dead_code)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    http::{Method, Request, header},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use server::{
    AppState, auth::otp::OtpSender, build_app, crypto::FieldCipher, error::AppError,
    phone::normalize_ng_phone,
};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

pub const TEST_FIELD_KEY: [u8; 32] = [7u8; 32];
pub const TEST_INDEX_KEY: [u8; 32] = [9u8; 32];
pub const TEST_PASSWORD: &str = "correct-horse-1";

/// Captures the last OTP code sent to each (E.164) phone number.
#[derive(Default)]
pub struct RecordingOtpSender {
    codes: Mutex<HashMap<String, String>>,
    sent: Mutex<usize>,
}

impl RecordingOtpSender {
    pub fn sent_count(&self) -> usize {
        *self.sent.lock().unwrap()
    }

    pub fn last_code(&self, phone_e164: &str) -> Option<String> {
        self.codes.lock().unwrap().get(phone_e164).cloned()
    }
}

#[async_trait]
impl OtpSender for RecordingOtpSender {
    async fn send(&self, phone_e164: &str, code: &str) -> Result<(), AppError> {
        self.codes
            .lock()
            .unwrap()
            .insert(phone_e164.to_string(), code.to_string());
        *self.sent.lock().unwrap() += 1;
        Ok(())
    }
}

pub struct TestResponse {
    pub status: u16,
    pub json: Value,
}

pub struct TestApp {
    router: Router,
    pub state: AppState,
    pub pool: PgPool,
    pub otp: Arc<RecordingOtpSender>,
}

impl TestApp {
    pub async fn new(pool: PgPool) -> Self {
        let otp = Arc::new(RecordingOtpSender::default());
        let state = AppState {
            db: pool.clone(),
            cipher: Arc::new(FieldCipher::new(TEST_FIELD_KEY)),
            index_key: Arc::new(TEST_INDEX_KEY),
            otp: otp.clone(),
        };
        Self {
            router: build_app(state.clone()),
            state,
            pool,
            otp,
        }
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        token: Option<&str>,
    ) -> TestResponse {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let req = match body {
            Some(json) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json.to_string())),
            None => builder.body(Body::empty()),
        }
        .expect("valid request");

        let res = self.router.clone().oneshot(req).await.expect("infallible");
        let status = res.status().as_u16();
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        TestResponse { status, json }
    }

    /// Sends a raw (possibly non-JSON) body with a JSON content type.
    pub async fn request_raw(&self, method: Method, path: &str, body: &str) -> TestResponse {
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("valid request");
        let res = self.router.clone().oneshot(req).await.expect("infallible");
        let status = res.status().as_u16();
        let bytes = res.into_body().collect().await.expect("body").to_bytes();
        TestResponse {
            status,
            json: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        }
    }

    /// Makes outstanding OTPs look older so the issue throttle window passes.
    pub async fn age_otps(&self, secs: i32) {
        sqlx::query(
            "UPDATE otp_codes SET created_at = created_at - make_interval(secs => $1), \
                                  sent_at = sent_at - make_interval(secs => $1)",
        )
        .bind(f64::from(secs))
        .execute(&self.pool)
        .await
        .unwrap();
    }

    /// The last OTP code sent to `phone` (any accepted format).
    pub fn last_code(&self, phone: &str) -> String {
        let e164 = normalize_ng_phone(phone).expect("valid test phone");
        self.otp.last_code(&e164).expect("an OTP was sent")
    }

    pub async fn sign_up(&self, phone: &str, password: &str) -> TestResponse {
        self.request(
            Method::POST,
            "/auth/sign-up",
            Some(json!({ "phone": phone, "password": password, "display_name": "Ada" })),
            None,
        )
        .await
    }

    pub async fn sign_in(&self, phone: &str, password: &str) -> TestResponse {
        self.request(
            Method::POST,
            "/auth/sign-in",
            Some(json!({ "phone": phone, "password": password })),
            None,
        )
        .await
    }

    /// POST /auth/verify-phone with a verification id and code.
    pub async fn verify(&self, verification_id: &Value, code: &str) -> TestResponse {
        self.request(
            Method::POST,
            "/auth/verify-phone",
            Some(json!({ "verification_id": verification_id, "code": code })),
            None,
        )
        .await
    }

    /// Signs up, verifies the phone with the emitted OTP, returns (user id, session token).
    pub async fn sign_up_verified(&self, phone: &str) -> (Uuid, String) {
        let res = self.sign_up(phone, TEST_PASSWORD).await;
        assert_eq!(res.status, 201, "sign-up failed: {}", res.json);
        let user_id: Uuid = res.json["user_id"].as_str().unwrap().parse().unwrap();
        let verify = self
            .verify(&res.json["verification_id"], &self.last_code(phone))
            .await;
        assert_eq!(verify.status, 200, "verify failed: {}", verify.json);
        (user_id, verify.json["token"].as_str().unwrap().to_string())
    }

    /// Creates a verified user, promotes it to owner, returns its session token.
    pub async fn make_owner(&self) -> String {
        let (user_id, token) = self.sign_up_verified("08099999999").await;
        sqlx::query("UPDATE users SET role = 'owner' WHERE id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await
            .expect("promote to owner");
        token
    }

    /// Creates a verified user, promotes it to support, returns its session token.
    pub async fn make_support(&self) -> String {
        let (user_id, token) = self.sign_up_verified("08088888888").await;
        sqlx::query("UPDATE users SET role = 'support' WHERE id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await
            .expect("promote to support");
        token
    }

    /// Owner creates an access code; returns (code id, plain code).
    pub async fn create_access_code(&self, owner_token: &str, body: Value) -> (Uuid, String) {
        let res = self
            .request(
                Method::POST,
                "/admin/access-codes",
                Some(body),
                Some(owner_token),
            )
            .await;
        assert_eq!(res.status, 201, "create code failed: {}", res.json);
        (
            res.json["id"].as_str().unwrap().parse().unwrap(),
            res.json["code"].as_str().unwrap().to_string(),
        )
    }

    pub async fn redeem(&self, token: &str, code: &str) -> TestResponse {
        self.request(
            Method::POST,
            "/seller/redeem",
            Some(json!({ "code": code })),
            Some(token),
        )
        .await
    }

    /// The owner's session token: signs in if the owner already exists, else creates it.
    pub async fn owner_token(&self) -> String {
        let res = self
            .request(
                Method::POST,
                "/auth/sign-in",
                Some(json!({ "phone": "08099999999", "password": TEST_PASSWORD })),
                None,
            )
            .await;
        if res.status == 200 {
            res.json["token"].as_str().unwrap().to_string()
        } else {
            self.make_owner().await
        }
    }

    /// Verified user who has redeemed an owner-issued code; returns (user id, token).
    pub async fn make_seller(&self, phone: &str) -> (Uuid, String) {
        let owner = self.owner_token().await;
        let (_, code) = self.create_access_code(&owner, json!({})).await;
        let (id, token) = self.sign_up_verified(phone).await;
        let res = self.redeem(&token, &code).await;
        assert_eq!(res.status, 200, "redeem failed: {}", res.json);
        (id, token)
    }
}
