use serde_json::json;
use server::audit::record;
use sqlx::PgPool;
use uuid::Uuid;

#[sqlx::test]
async fn audit_log_is_append_only(pool: PgPool) {
    let mut conn = pool.acquire().await.unwrap();
    let subject = Uuid::new_v4();
    record(
        &mut conn,
        None,
        "test.created",
        "thing",
        Some(subject),
        json!({ "k": "v" }),
    )
    .await
    .expect("record");

    let (action, detail): (String, serde_json::Value) =
        sqlx::query_as("SELECT action, detail FROM audit_log WHERE subject_id = $1")
            .bind(subject)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(action, "test.created");
    assert_eq!(detail, json!({ "k": "v" }));

    let update = sqlx::query("UPDATE audit_log SET action = 'x'")
        .execute(&pool)
        .await;
    assert!(update.is_err(), "UPDATE must be rejected");
    let delete = sqlx::query("DELETE FROM audit_log").execute(&pool).await;
    assert!(delete.is_err(), "DELETE must be rejected");
    let truncate = sqlx::query("TRUNCATE audit_log").execute(&pool).await;
    assert!(truncate.is_err(), "TRUNCATE must be rejected");

    let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM audit_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}
