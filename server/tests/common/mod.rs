#![allow(dead_code)]

use axum::{
    Router,
    body::Body,
    http::{Method, Request, header},
};
use http_body_util::BodyExt;
use serde_json::Value;
use server::{AppState, build_app};
use sqlx::PgPool;
use tower::ServiceExt;

pub struct TestResponse {
    pub status: u16,
    pub json: Value,
}

pub struct TestApp {
    router: Router,
}

impl TestApp {
    pub async fn new(pool: PgPool) -> Self {
        let state = AppState { db: pool };
        Self { router: build_app(state) }
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
}
