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
    AppState,
    auth::otp::OtpSender,
    build_app,
    crypto::FieldCipher,
    error::AppError,
    payments::{ProviderRegistry, fake::FakeProvider},
    phone::normalize_ng_phone,
};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

pub const TEST_FIELD_KEY: [u8; 32] = [7u8; 32];
pub const TEST_INDEX_KEY: [u8; 32] = [9u8; 32];
pub const TEST_PASSWORD: &str = "correct-horse-1";
pub const TEST_REDIRECT_URL: &str = "http://localhost:8080/checkout/return";

/// A published product with one variant in an active store; see
/// [`TestApp::make_active_store_with_product`].
pub struct Listing {
    pub seller_id: Uuid,
    pub seller_token: String,
    pub store_id: Uuid,
    pub product_id: Uuid,
    pub variant_id: Uuid,
}

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
    /// The registered payment provider (name "fake", method "mono_directpay").
    pub fake: Arc<FakeProvider>,
}

impl TestApp {
    pub async fn new(pool: PgPool) -> Self {
        let otp = Arc::new(RecordingOtpSender::default());
        let fake = Arc::new(FakeProvider::new());
        let mut providers = ProviderRegistry::new();
        providers.register(fake.clone());
        let state = AppState {
            db: pool.clone(),
            cipher: Arc::new(FieldCipher::new(TEST_FIELD_KEY)),
            index_key: Arc::new(TEST_INDEX_KEY),
            otp: otp.clone(),
            providers: Arc::new(providers),
            payment_redirect_url: TEST_REDIRECT_URL.into(),
        };
        Self {
            router: build_app(state.clone()),
            state,
            pool,
            otp,
            fake,
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
        self.send(builder, body).await
    }

    /// Like [`request`](Self::request), with extra headers.
    pub async fn request_with_headers(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        token: Option<&str>,
        headers: &[(&str, &str)],
    ) -> TestResponse {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        self.send(builder, body).await
    }

    async fn send(
        &self,
        builder: axum::http::request::Builder,
        body: Option<Value>,
    ) -> TestResponse {
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

    /// Seller (`phone`) with an active store named `store_name`, activated the real
    /// way (a provider-verified payout account), holding one published product with
    /// a single variant at `price_kobo` and `stock`.
    pub async fn make_active_store_with_product(
        &self,
        phone: &str,
        store_name: &str,
        price_kobo: i64,
        stock: i32,
    ) -> Listing {
        let (seller_id, seller_token) = self.make_seller(phone).await;
        let res = self
            .request(
                Method::POST,
                "/stores",
                Some(json!({
                    "name": store_name,
                    "pickup_address": "12 Unique Street, Wuse 2",
                    "pickup_state": "FCT",
                    "delivery_courier": true,
                    "delivery_self": false
                })),
                Some(&seller_token),
            )
            .await;
        assert_eq!(res.status, 201, "create store: {}", res.json);
        let store_id: Uuid = res.json["id"].as_str().unwrap().parse().unwrap();

        sqlx::query("UPDATE platform_settings SET payout_provider = 'fake'")
            .execute(&self.pool)
            .await
            .unwrap();
        // Sellers sign up as "Ada"; the resolved account name must match it.
        self.fake.set_lookup(Ok("ADA OBI".into()));
        let res = self
            .request(
                Method::PUT,
                &format!("/stores/{store_id}/payout-account"),
                Some(json!({
                    "bank_code": "058",
                    "account_number": "0123456785",
                    "password": TEST_PASSWORD
                })),
                Some(&seller_token),
            )
            .await;
        assert_eq!(res.status, 200, "payout account: {}", res.json);
        assert_eq!(res.json["status"], "verified", "{}", res.json);

        let product_id = self
            .add_product(&seller_token, store_id, price_kobo, &[(json!({}), stock)])
            .await;
        let variant_id = self.variant_ids(product_id).await[0];
        Listing {
            seller_id,
            seller_token,
            store_id,
            product_id,
            variant_id,
        }
    }

    /// Adds a published product to `store_id` with the given (attributes, stock)
    /// variants at `price_kobo`; returns the product id.
    pub async fn add_product(
        &self,
        seller_token: &str,
        store_id: Uuid,
        price_kobo: i64,
        variants: &[(Value, i32)],
    ) -> Uuid {
        let variants: Vec<Value> = variants
            .iter()
            .map(|(attrs, stock)| json!({ "attributes": attrs, "stock": stock }))
            .collect();
        let res = self
            .request(
                Method::POST,
                &format!("/stores/{store_id}/products"),
                Some(json!({
                    "name": "Ankara Shirt",
                    "description": "Handmade shirt",
                    "category": "fashion",
                    "condition": "new",
                    "photo_keys": ["photos/a.jpg"],
                    "weight_grams": 300,
                    "base_price_kobo": price_kobo,
                    "variants": variants
                })),
                Some(seller_token),
            )
            .await;
        assert_eq!(res.status, 201, "create product: {}", res.json);
        let product_id: Uuid = res.json["id"].as_str().unwrap().parse().unwrap();
        let res = self
            .request(
                Method::PATCH,
                &format!("/products/{product_id}"),
                Some(json!({ "is_published": true })),
                Some(seller_token),
            )
            .await;
        assert_eq!(res.status, 200, "publish: {}", res.json);
        product_id
    }

    /// The product's variant ids, ordered by id.
    pub async fn variant_ids(&self, product_id: Uuid) -> Vec<Uuid> {
        sqlx::query_scalar("SELECT id FROM product_variants WHERE product_id = $1 ORDER BY id")
            .bind(product_id)
            .fetch_all(&self.pool)
            .await
            .unwrap()
    }

    /// Units held by pending checkouts.
    pub async fn reserved(&self, variant_id: Uuid) -> i32 {
        sqlx::query_scalar("SELECT reserved_quantity FROM product_variants WHERE id = $1")
            .bind(variant_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// Units a buyer can still buy (`stock - reserved_quantity`).
    pub async fn available(&self, variant_id: Uuid) -> i32 {
        self.stock(variant_id).await - self.reserved(variant_id).await
    }

    /// On-hand stock.
    pub async fn stock(&self, variant_id: Uuid) -> i32 {
        sqlx::query_scalar("SELECT stock FROM product_variants WHERE id = $1")
            .bind(variant_id)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    /// PUT /cart/items.
    pub async fn set_cart(&self, token: &str, variant_id: Uuid, quantity: i64) -> TestResponse {
        self.request(
            Method::PUT,
            "/cart/items",
            Some(json!({ "variant_id": variant_id, "quantity": quantity })),
            Some(token),
        )
        .await
    }

    /// POST /checkout with `method`.
    pub async fn checkout(&self, token: &str, method: &str) -> TestResponse {
        self.request(
            Method::POST,
            "/checkout",
            Some(json!({ "method": method })),
            Some(token),
        )
        .await
    }
}
