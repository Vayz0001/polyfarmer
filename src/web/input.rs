//! Parsing and validation of every value that arrives from a form.
//!
//! Rule: input is untrusted and must be rejected with a message, never allowed to
//! reach arithmetic that can panic. Concretely this module guards against
//! * multibyte input where code assumed ASCII (`"7日"` used to panic a slice),
//! * absurd magnitudes that overflow `chrono::Duration` / `Decimal` maths
//!   (`"99999999999999d"`, `"79228162514264337593543950335"`),
//! * unbounded strings that end up in caches and logs.
//!
//! Every numeric field has an explicit, documented bound.

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Hard cap on one order's USD size. Far above any sensible farming order; its
/// job is keeping downstream `size / price` (≤ 1e6 / 0.0001 = 1e10 shares) far
/// from `Decimal` overflow.
pub const MAX_ORDER_SIZE_USD: Decimal = dec!(1000000);
/// Cap on "depth ahead of me" (USD).
pub const MAX_MIN_DEPTH_USD: Decimal = dec!(100000000);
/// Largest distance below the best bid, in cents.
pub const MAX_DISTANCE_CENTS: Decimal = dec!(50);
/// Largest auto-pause threshold, in cents.
pub const MAX_VOLATILITY_CENTS: Decimal = dec!(100);
/// Valid bid prices, in cents (Polymarket's finest tick is 0.01¢ = 0.0001).
pub const MIN_PRICE_CENTS: Decimal = dec!(0.01);
pub const MAX_PRICE_CENTS: Decimal = dec!(99.99);
/// Longest numeric string we will even try to parse.
const MAX_NUMBER_LEN: usize = 32;
/// Longest free-text search we keep (it becomes a cache key).
pub const MAX_SEARCH_LEN: usize = 200;
/// Longest pagination cursor accepted.
pub const MAX_CURSOR_LEN: usize = 512;
/// Longest market slug accepted.
pub const MAX_SLUG_LEN: usize = 200;
/// Expiry bounds, in seconds.
const MIN_EXPIRY_SECS: i64 = 60;
const MAX_EXPIRY_SECS: i64 = 366 * 86_400;

/// Parse a plain decimal (no exponent), bounded in length.
fn decimal(s: &str) -> Option<Decimal> {
    let t = s.trim();
    if t.is_empty() || t.len() > MAX_NUMBER_LEN {
        return None;
    }
    t.parse::<Decimal>().ok()
}

/// A bid price in cents, returned in price units (18 → 0.18).
pub fn price_cents(s: &str) -> Result<Decimal, String> {
    match decimal(s) {
        Some(c) if (MIN_PRICE_CENTS..=MAX_PRICE_CENTS).contains(&c) => Ok(c / dec!(100)),
        _ => Err(format!("Enter a price between {MIN_PRICE_CENTS} and {MAX_PRICE_CENTS}¢.")),
    }
}

/// An order size in USD (> 0, ≤ [`MAX_ORDER_SIZE_USD`]).
pub fn order_size_usd(s: &str) -> Result<Decimal, String> {
    match decimal(s) {
        Some(v) if v > dec!(0) && v <= MAX_ORDER_SIZE_USD => Ok(v),
        _ => Err(format!("Order size must be a dollar amount between 0 and {MAX_ORDER_SIZE_USD}.")),
    }
}

/// "Depth ahead of me" in USD — `MarketConfig::min_depth_between` is USDC
/// notional (the quoter compares Σ price·size). Blank = 0 (no requirement); `$`
/// and thousands separators are tolerated.
pub fn depth_usd(s: &str) -> Result<Decimal, String> {
    let t = s.trim().trim_start_matches('$').replace(',', "");
    if t.trim().is_empty() {
        return Ok(dec!(0));
    }
    match decimal(&t) {
        Some(v) if v >= dec!(0) && v <= MAX_MIN_DEPTH_USD => Ok(v),
        _ => Err(format!("Min depth must be a dollar amount between 0 and {MAX_MIN_DEPTH_USD}.")),
    }
}

/// Distance below the best bid, entered in cents, returned in price units.
pub fn distance_cents(s: &str) -> Result<Decimal, String> {
    match decimal(s) {
        Some(c) if c > dec!(0) && c <= MAX_DISTANCE_CENTS => Ok(c / dec!(100)),
        _ => Err(format!("Distance must be between 0 and {MAX_DISTANCE_CENTS} cents.")),
    }
}

/// Optional auto-pause threshold in cents (blank = disabled), in price units.
pub fn volatility_cents(s: &str) -> Result<Option<Decimal>, String> {
    if s.trim().is_empty() {
        return Ok(None);
    }
    match decimal(s) {
        Some(c) if c > dec!(0) && c <= MAX_VOLATILITY_CENTS => Ok(Some(c / dec!(100))),
        _ => Err(format!("Auto-pause threshold must be between 0 and {MAX_VOLATILITY_CENTS} cents.")),
    }
}

/// `"<digits><unit>"` with unit s/m/h/d (any case), e.g. `7d`, `4h`, `30m`;
/// `never` (a far-future sentinel — `MarketConfig::expires_at` isn't optional);
/// blank = 7 days. Between 1 minute and 366 days.
///
/// Built from checked integer maths and bounds-tested *before* any `chrono`
/// call, so no input can overflow `Duration`/`DateTime` (both panic on overflow).
pub fn expiry(s: &str) -> Result<DateTime<Utc>, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("never") {
        return Ok(Utc::now() + Duration::days(36_500));
    }
    let s = if s.is_empty() { "7d" } else { s };
    let invalid = || "Invalid expiry — use a number and a unit, e.g. 7d, 4h or 30m.".to_string();

    // Split off the LAST CHARACTER (not byte: it may be multibyte).
    let mut chars = s.chars();
    let unit = chars.next_back().ok_or_else(invalid)?.to_ascii_lowercase();
    let digits = chars.as_str();
    if digits.is_empty() || digits.len() > 9 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let n: i64 = digits.parse().map_err(|_| invalid())?; // ≤ 999,999,999
    let per_unit: i64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        _ => return Err("Invalid expiry unit — use s, m, h or d.".to_string()),
    };
    let secs = n * per_unit; // ≤ 8.7e13: fits i64 comfortably
    if !(MIN_EXPIRY_SECS..=MAX_EXPIRY_SECS).contains(&secs) {
        return Err("Expiry must be between 1 minute and 1 year.".to_string());
    }
    Ok(Utc::now() + Duration::seconds(secs))
}

/// A search string, cut to [`MAX_SEARCH_LEN`] characters.
pub fn search_text(q: &str) -> String {
    q.trim().chars().take(MAX_SEARCH_LEN).collect()
}

/// A pagination cursor, or `None` if absent / implausibly long.
pub fn cursor(c: Option<&str>) -> Option<String> {
    c.filter(|c| !c.is_empty() && c.len() <= MAX_CURSOR_LEN).map(str::to_string)
}

/// Order-book grouping step: the market's tick up to one full unit (1.0); any
/// other value (garbage, zero, tiny, huge) falls back to the tick.
pub fn book_group(raw: Option<&str>, tick: Decimal) -> Decimal {
    raw.and_then(decimal).filter(|g| *g >= tick && *g <= dec!(1)).unwrap_or(tick)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_are_bounded_in_cents_and_returned_in_price_units() {
        assert_eq!(price_cents("18").unwrap(), dec!(0.18));
        assert_eq!(price_cents(" 0.01 ").unwrap(), dec!(0.0001));
        assert_eq!(price_cents("99.99").unwrap(), dec!(0.9999));
        for bad in ["", "0", "-5", "100", "99.995", "0.001", "abc", "1e3", "18¢", &"9".repeat(40)] {
            assert!(price_cents(bad).is_err(), "{bad:?}");
        }
        // a vanishingly small price would make size/price overflow Decimal
        assert!(price_cents("0.0000000000000000000000000001").is_err());
    }

    #[test]
    fn order_size_has_a_ceiling() {
        assert_eq!(order_size_usd("100").unwrap(), dec!(100));
        assert_eq!(order_size_usd("1000000").unwrap(), dec!(1000000));
        for bad in ["0", "-1", "1000000.01", "79228162514264337593543950335", "", "x"] {
            assert!(order_size_usd(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn depth_distance_and_volatility_are_bounded() {
        assert_eq!(depth_usd("").unwrap(), dec!(0));
        assert_eq!(depth_usd("$1,250.50").unwrap(), dec!(1250.50));
        assert!(depth_usd("-1").is_err());
        assert!(depth_usd("100000001").is_err());
        assert!(depth_usd("79228162514264337593543950335").is_err());

        assert_eq!(distance_cents("2").unwrap(), dec!(0.02));
        assert!(distance_cents("0").is_err() && distance_cents("50.01").is_err() && distance_cents("x").is_err());

        assert_eq!(volatility_cents("").unwrap(), None);
        assert_eq!(volatility_cents("5").unwrap(), Some(dec!(0.05)));
        assert!(volatility_cents("0").is_err() && volatility_cents("101").is_err());
    }

    #[test]
    fn expiry_accepts_the_documented_forms() {
        let near = |d: DateTime<Utc>, secs: i64| (d - Utc::now()).num_seconds().abs_diff(secs) < 5;
        assert!(near(expiry("7d").unwrap(), 7 * 86400));
        assert!(near(expiry("").unwrap(), 7 * 86400), "blank = 7 days");
        assert!(near(expiry("4H").unwrap(), 4 * 3600), "unit is case-insensitive");
        assert!(near(expiry(" 30m ").unwrap(), 30 * 60));
        assert!(near(expiry("90s").unwrap(), 90));
        assert!(near(expiry("366d").unwrap(), 366 * 86400));
        assert!(expiry("never").unwrap() > Utc::now() + Duration::days(36_000));
        assert!(expiry("NEVER").is_ok());
    }

    #[test]
    fn expiry_rejects_everything_else_without_panicking() {
        for bad in [
            "7日", "日", "é", "7é", "٣d", "💥", "7 d", "d", "7", "-5d", "+5d", "7.5d", "0d", "59s", "367d",
            "400d", "2x", "1e3d", "999999999d", "9999999999d", "99999999999999999999d", "９d",
        ] {
            assert!(expiry(bad).is_err(), "{bad:?} must be rejected (not panic)");
        }
        // 9 digits is the widest accepted number; even then the maths cannot overflow.
        assert!(expiry("999999999d").is_err());
    }

    #[test]
    fn free_text_is_bounded() {
        assert_eq!(search_text("  iran  "), "iran");
        assert_eq!(search_text(&"é".repeat(500)).chars().count(), MAX_SEARCH_LEN);
        assert_eq!(cursor(Some("abc")), Some("abc".to_string()));
        assert_eq!(cursor(Some("")), None);
        assert_eq!(cursor(Some(&"x".repeat(MAX_CURSOR_LEN + 1))), None);
        assert_eq!(cursor(None), None);
    }

    #[test]
    fn book_grouping_falls_back_to_the_tick_for_anything_odd() {
        let tick = dec!(0.001);
        assert_eq!(book_group(Some("0.01"), tick), dec!(0.01));
        assert_eq!(book_group(Some("1"), tick), dec!(1));
        for bad in ["0", "-1", "0.0000000000000000000000000001", "5", "abc", ""] {
            assert_eq!(book_group(Some(bad), tick), tick, "{bad:?}");
        }
        assert_eq!(book_group(None, tick), tick);
    }
}
