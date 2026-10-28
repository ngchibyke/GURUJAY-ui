//! Hard limits shared by request validation and the database CHECK constraints
//! (`migrations/0007_carry_forward.sql` repeats these literals; a test keeps them in step).

/// Largest unit price (product base price or variant override): NGN 10m in kobo.
pub const MAX_PRICE_KOBO: i64 = 1_000_000_000;
/// Largest stock level of a single variant.
pub const MAX_STOCK: i32 = 100_000;
/// Largest quantity of one cart line (enforced when carts are added).
pub const MAX_LINE_QUANTITY: i32 = 100;
/// Largest checkout total in kobo (NGN 50m).
pub const MAX_CHECKOUT_TOTAL_KOBO: i64 = 5_000_000_000;
