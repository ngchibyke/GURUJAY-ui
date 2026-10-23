use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{audit, crypto::FieldCipher, error::AppError};

pub const MAX_STORES_PER_USER: i64 = 2;
const SERVICE_STATE: &str = "FCT";
const MAX_SLUG_ATTEMPTS: u32 = 1000;
/// Slugs that collide with static routes (e.g. `/stores/mine`); never allocated as-is.
const RESERVED_SLUGS: &[&str] = &["mine", "new", "search", "id"];

#[derive(Debug, Clone, Deserialize)]
pub struct NewStore {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub pickup_address: String,
    pub pickup_state: String,
    pub delivery_courier: bool,
    pub delivery_self: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct StorePatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub pickup_address: Option<String>,
    pub pickup_state: Option<String>,
    pub delivery_courier: Option<bool>,
    pub delivery_self: Option<bool>,
}

/// A store as stored, with the pickup address still encrypted.
#[derive(Debug, sqlx::FromRow)]
pub struct Store {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub pickup_address_enc: Vec<u8>,
    pub pickup_state: String,
    pub delivery_courier: bool,
    pub delivery_self: bool,
    pub status: String,
    pub created_at: OffsetDateTime,
}

const COLUMNS: &str = "id, owner_id, name, slug, description, pickup_address_enc, pickup_state, \
                       delivery_courier, delivery_self, status::text AS status, created_at";

/// The public shape: never carries the pickup address.
#[derive(Debug, Serialize)]
pub struct PublicStore {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub description: String,
    pub status: String,
    pub pickup_state: String,
    pub delivery_courier: bool,
    pub delivery_self: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

/// The owner-only shape: the public shape plus the decrypted pickup address.
#[derive(Debug, Serialize)]
pub struct OwnerStore {
    #[serde(flatten)]
    pub store: PublicStore,
    pub pickup_address: String,
}

impl Store {
    pub fn public(&self) -> PublicStore {
        PublicStore {
            id: self.id,
            name: self.name.clone(),
            slug: self.slug.clone(),
            description: self.description.clone(),
            status: self.status.clone(),
            pickup_state: self.pickup_state.clone(),
            delivery_courier: self.delivery_courier,
            delivery_self: self.delivery_self,
            created_at: self.created_at,
        }
    }

    pub fn owner_view(&self, cipher: &FieldCipher) -> Result<OwnerStore, AppError> {
        Ok(OwnerStore { store: self.public(), pickup_address: cipher.decrypt(&self.pickup_address_enc)? })
    }
}

fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() { "store".to_string() } else { out.to_string() }
}

fn validate_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(AppError::Validation("name must be 1 to 80 characters".into()));
    }
    Ok(name.to_string())
}

fn validate_description(description: &str) -> Result<(), AppError> {
    if description.chars().count() > 2000 {
        return Err(AppError::Validation("description must be at most 2000 characters".into()));
    }
    Ok(())
}

fn validate_address(address: &str) -> Result<String, AppError> {
    let address = address.trim();
    if address.is_empty() || address.chars().count() > 300 {
        return Err(AppError::Validation("pickup_address must be 1 to 300 characters".into()));
    }
    Ok(address.to_string())
}

fn validate_state(state: &str) -> Result<(), AppError> {
    if state != SERVICE_STATE {
        return Err(AppError::Unprocessable(
            "outside_service_area",
            "Stores must currently be located in the FCT".into(),
        ));
    }
    Ok(())
}

fn validate_delivery(courier: bool, own: bool) -> Result<(), AppError> {
    if !courier && !own {
        return Err(AppError::Validation("at least one delivery method is required".into()));
    }
    Ok(())
}

pub async fn create_store(
    db: &PgPool,
    cipher: &FieldCipher,
    owner: Uuid,
    input: NewStore,
) -> Result<Store, AppError> {
    validate_state(&input.pickup_state)?;
    let name = validate_name(&input.name)?;
    validate_description(&input.description)?;
    let address = validate_address(&input.pickup_address)?;
    validate_delivery(input.delivery_courier, input.delivery_self)?;
    let address_enc = cipher.encrypt(&address);

    let mut tx = db.begin().await.context("begin")?;
    // Serialise concurrent creates for the same user (lock order: users row first).
    sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id = $1 FOR UPDATE")
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .context("lock user")?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM stores WHERE owner_id = $1 AND deleted_at IS NULL")
        .bind(owner)
        .fetch_one(&mut *tx)
        .await
        .context("count stores")?;
    if count >= MAX_STORES_PER_USER {
        return Err(AppError::Conflict("store_limit_reached"));
    }

    let base = slugify(&name);
    let mut store = None;
    for n in 1..=MAX_SLUG_ATTEMPTS {
        if n == 1 && RESERVED_SLUGS.contains(&base.as_str()) {
            continue;
        }
        let slug = if n == 1 { base.clone() } else { format!("{base}-{n}") };
        // The unique index decides; a taken slug (even a deleted store's) yields no row.
        let inserted: Option<Store> = sqlx::query_as(&format!(
            "INSERT INTO stores (owner_id, name, slug, description, pickup_address_enc, pickup_state, \
                                 delivery_courier, delivery_self) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (slug) DO NOTHING RETURNING {COLUMNS}"
        ))
        .bind(owner)
        .bind(&name)
        .bind(&slug)
        .bind(&input.description)
        .bind(&address_enc)
        .bind(&input.pickup_state)
        .bind(input.delivery_courier)
        .bind(input.delivery_self)
        .fetch_optional(&mut *tx)
        .await
        .context("insert store")?;
        if inserted.is_some() {
            store = inserted;
            break;
        }
    }
    let store = store.ok_or_else(|| anyhow::anyhow!("could not allocate a store slug"))?;
    audit::record(&mut tx, Some(owner), "store.created", "store", Some(store.id), json!({})).await?;
    tx.commit().await.context("commit")?;
    Ok(store)
}

pub async fn list_mine(db: &PgPool, owner: Uuid) -> Result<Vec<Store>, AppError> {
    Ok(sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM stores WHERE owner_id = $1 AND deleted_at IS NULL ORDER BY created_at, id"
    ))
    .bind(owner)
    .fetch_all(db)
    .await
    .context("list stores")?)
}

/// Active stores are public; the owner can also see their own store in any status.
pub async fn get_by_slug(db: &PgPool, slug: &str, viewer: Option<Uuid>) -> Result<Store, AppError> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM stores \
         WHERE slug = $1 AND deleted_at IS NULL AND (status = 'active' OR owner_id = $2)"
    ))
    .bind(slug)
    .bind(viewer)
    .fetch_optional(db)
    .await
    .context("get store")?
    .ok_or(AppError::NotFound)
}

pub async fn update_store(
    db: &PgPool,
    cipher: &FieldCipher,
    owner: Uuid,
    id: Uuid,
    patch: StorePatch,
) -> Result<Store, AppError> {
    let mut tx = db.begin().await.context("begin")?;
    let current = lock_owned(&mut tx, owner, id).await?;

    if let Some(state) = &patch.pickup_state {
        validate_state(state)?;
    }
    let name = match &patch.name {
        Some(n) => validate_name(n)?,
        None => current.name.clone(),
    };
    let description = patch.description.clone().unwrap_or_else(|| current.description.clone());
    validate_description(&description)?;
    let address_enc = match &patch.pickup_address {
        Some(a) => cipher.encrypt(&validate_address(a)?),
        None => current.pickup_address_enc.clone(),
    };
    let state = patch.pickup_state.clone().unwrap_or_else(|| current.pickup_state.clone());
    let courier = patch.delivery_courier.unwrap_or(current.delivery_courier);
    let own = patch.delivery_self.unwrap_or(current.delivery_self);
    validate_delivery(courier, own)?;

    let store: Store = sqlx::query_as(&format!(
        "UPDATE stores SET name = $2, description = $3, pickup_address_enc = $4, pickup_state = $5, \
                           delivery_courier = $6, delivery_self = $7 \
         WHERE id = $1 RETURNING {COLUMNS}"
    ))
    .bind(id)
    .bind(&name)
    .bind(&description)
    .bind(&address_enc)
    .bind(&state)
    .bind(courier)
    .bind(own)
    .fetch_one(&mut *tx)
    .await
    .context("update store")?;
    audit::record(&mut tx, Some(owner), "store.updated", "store", Some(id), json!({})).await?;
    tx.commit().await.context("commit")?;
    Ok(store)
}

pub async fn delete_store(db: &PgPool, owner: Uuid, id: Uuid) -> Result<(), AppError> {
    let mut tx = db.begin().await.context("begin")?;
    lock_owned(&mut tx, owner, id).await?;
    sqlx::query("UPDATE stores SET deleted_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .context("soft delete store")?;
    audit::record(&mut tx, Some(owner), "store.deleted", "store", Some(id), json!({})).await?;
    tx.commit().await.context("commit")?;
    Ok(())
}

/// Locks a live store row owned by `owner`; anything else is NotFound (existence is not revealed).
async fn lock_owned(tx: &mut Transaction<'_, Postgres>, owner: Uuid, id: Uuid) -> Result<Store, AppError> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM stores WHERE id = $1 AND owner_id = $2 AND deleted_at IS NULL FOR UPDATE"
    ))
    .bind(id)
    .bind(owner)
    .fetch_optional(&mut **tx)
    .await
    .context("lock store")?
    .ok_or(AppError::NotFound)
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugs_are_hyphenated_lowercase() {
        assert_eq!(slugify("Mama  Put!"), "mama-put");
        assert_eq!(slugify("  --Abuja's Best--  "), "abuja-s-best");
        assert_eq!(slugify("!!!"), "store");
    }
}
