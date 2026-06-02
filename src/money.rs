//! Money formatting — one place, so every command renders cents identically.
//! Handles negatives (e.g. credits) too.

pub fn dollars(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let c = cents.abs();
    format!("{sign}${}.{:02}", c / 100, c % 100)
}
