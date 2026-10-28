//! Pure kobo arithmetic: platform cut, payout fees and proportional splits.
//! Limits live in `limits.rs`; this module only does the maths.

use crate::error::AppError;

/// Payout fee schedule in kobo. The base fee is `small_fee` up to and including
/// `threshold`, and `large_fee` above it. On top of that, `stamp_duty_kobo` is
/// added when the payout is at or above `stamp_duty_threshold`.
///
/// The stamp duty (flat NGN 50 on transfers of NGN 10,000 or more from 1 Jan
/// 2026) is configurable because its source is news reports, not the statute;
/// confirm the rule and who bears it with a tax adviser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeSchedule {
    pub small_fee: i64,
    pub large_fee: i64,
    pub threshold: i64,
    pub stamp_duty_kobo: i64,
    pub stamp_duty_threshold: i64,
}

impl Default for FeeSchedule {
    fn default() -> Self {
        Self {
            small_fee: 3_500,
            large_fee: 5_000,
            threshold: 5_000_000,
            stamp_duty_kobo: 5_000,
            stamp_duty_threshold: 1_000_000,
        }
    }
}

/// Sums amounts, failing with `amount_too_large` on i64 overflow.
pub fn checked_sum(items: &[i64]) -> Result<i64, AppError> {
    items
        .iter()
        .try_fold(0i64, |acc, &x| acc.checked_add(x))
        .ok_or_else(|| AppError::Unprocessable("amount_too_large", "amount is too large".into()))
}

/// `items_total * bps / 10_000`, rounded half-up. Uses i128 so it never
/// overflows or panics; negative inputs are a programming error and count as 0.
/// The result saturates at `i64::MAX`.
pub fn cut_kobo(items_total: i64, bps: i32) -> i64 {
    debug_assert!(items_total >= 0 && bps >= 0, "negative cut input");
    let t = i128::from(items_total.max(0));
    let b = i128::from(bps.max(0));
    let cut = (t * b + 5_000) / 10_000;
    i64::try_from(cut).unwrap_or(i64::MAX)
}

/// Fee for paying out `payout_before_fee`: `small_fee` when it is at or below
/// the threshold (inclusive), otherwise `large_fee`; plus `stamp_duty_kobo`
/// when it is at or above `stamp_duty_threshold`. The seller bears this fee.
/// Example with defaults: 5_000_000 -> 3_500 + 5_000 = 8_500.
pub fn payout_fee_kobo(payout_before_fee: i64, s: &FeeSchedule) -> i64 {
    let base = if payout_before_fee <= s.threshold {
        s.small_fee
    } else {
        s.large_fee
    };
    let duty = if payout_before_fee >= s.stamp_duty_threshold {
        s.stamp_duty_kobo
    } else {
        0
    };
    base.saturating_add(duty)
}

/// Splits `total` across `weights` by the largest-remainder method, so the
/// parts always sum to `total` (for `total >= 0`).
///
/// - Empty weights give an empty result.
/// - All-zero weights split equally, leftover kobo going to the earliest indices.
/// - Ties in remainders go to the earlier index.
/// - Negative inputs are a programming error (`debug_assert!`) and count as 0.
pub fn split_proportional(total: i64, weights: &[i64]) -> Vec<i64> {
    debug_assert!(
        total >= 0 && weights.iter().all(|&w| w >= 0),
        "negative split input"
    );
    if weights.is_empty() {
        return Vec::new();
    }
    let total = i128::from(total.max(0));
    let mut w: Vec<i128> = weights.iter().map(|&w| i128::from(w.max(0))).collect();
    if w.iter().sum::<i128>() == 0 {
        w.iter_mut().for_each(|x| *x = 1);
    }
    let sum: i128 = w.iter().sum();

    let mut parts: Vec<i128> = w.iter().map(|&x| total * x / sum).collect();
    let rems: Vec<i128> = w.iter().map(|&x| total * x % sum).collect();
    let leftover = total - parts.iter().sum::<i128>();

    let mut order: Vec<usize> = (0..w.len()).collect();
    // Stable sort: equal remainders keep index order.
    order.sort_by(|&a, &b| rems[b].cmp(&rems[a]));
    for &i in order.iter().take(leftover as usize) {
        parts[i] += 1;
    }
    parts.into_iter().map(|p| p as i64).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cut_rounds_half_up() {
        assert_eq!(cut_kobo(1_000_000, 500), 50_000);
        assert_eq!(cut_kobo(1_001, 500), 50);
        assert_eq!(cut_kobo(1_010, 500), 51);
        assert_eq!(cut_kobo(1_050, 500), 53); // 52.5 rounds up
    }

    #[test]
    fn cut_never_overflows() {
        assert_eq!(cut_kobo(i64::MAX, 10_000), i64::MAX);
        assert_eq!(cut_kobo(i64::MAX, i32::MAX), i64::MAX);
        assert_eq!(cut_kobo(0, 500), 0);
    }

    #[test]
    fn payout_fee_boundary_is_inclusive() {
        let s = FeeSchedule::default();
        // small fee 3_500 + stamp duty 5_000
        assert_eq!(payout_fee_kobo(5_000_000, &s), 8_500);
        // large fee 5_000 + stamp duty 5_000
        assert_eq!(payout_fee_kobo(5_000_001, &s), 10_000);
        assert_eq!(payout_fee_kobo(0, &s), 3_500);
    }

    #[test]
    fn stamp_duty_applies_from_threshold() {
        let s = FeeSchedule::default();
        assert_eq!(payout_fee_kobo(999_999, &s), 3_500);
        assert_eq!(payout_fee_kobo(1_000_000, &s), 8_500);
    }

    #[test]
    fn split_largest_remainder() {
        let parts = split_proportional(100, &[1, 1, 1]);
        assert_eq!(parts, vec![34, 33, 33]);
        assert_eq!(parts.iter().sum::<i64>(), 100);
    }

    #[test]
    fn split_weighted_sums_to_total() {
        let parts = split_proportional(1_000, &[3, 7, 11]);
        assert_eq!(parts.iter().sum::<i64>(), 1_000);
        assert_eq!(split_proportional(10, &[1, 3]), vec![3, 7]);
    }

    #[test]
    fn split_edge_cases() {
        assert!(split_proportional(100, &[]).is_empty());
        assert_eq!(split_proportional(100, &[0, 0, 0]), vec![34, 33, 33]);
        assert_eq!(split_proportional(0, &[1, 2]), vec![0, 0]);
        assert_eq!(split_proportional(5, &[0, 1]), vec![0, 5]);
        let big = split_proportional(i64::MAX, &[i64::MAX, i64::MAX, 1]);
        assert_eq!(
            big.iter().map(|&x| i128::from(x)).sum::<i128>(),
            i128::from(i64::MAX)
        );
    }

    #[test]
    fn checked_sum_detects_overflow() {
        assert_eq!(checked_sum(&[1, 2, 3]).unwrap(), 6);
        assert_eq!(checked_sum(&[]).unwrap(), 0);
        assert!(matches!(
            checked_sum(&[i64::MAX, 1]),
            Err(AppError::Unprocessable("amount_too_large", _))
        ));
    }
}
