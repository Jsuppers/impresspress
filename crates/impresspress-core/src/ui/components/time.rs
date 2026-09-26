//! Timestamps as the tables show them.

use maud::{html, Markup};

/// An RFC 3339 timestamp as a `<time>` element: the full stored value in
/// `datetime` for machines, [`crate::util::format_timestamp`]'s minute-precision
/// UTC text for people (`2026-05-06 10:00`, not `2026-05-06T10:00:00.123456789Z`).
///
/// A `<time>` element because it is one, and because the visual-baseline suite
/// masks `time`: a value that differs on every run must be one.
pub fn timestamp(rfc3339: &str) -> Markup {
    html! { time datetime=(rfc3339) { (crate::util::format_timestamp(rfc3339)) } }
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamp_keeps_the_raw_value_and_shows_the_humanised_one() {
        assert_eq!(
            super::timestamp("2026-05-06T10:00:00.123456789Z").into_string(),
            r#"<time datetime="2026-05-06T10:00:00.123456789Z">2026-05-06 10:00</time>"#
        );
    }
}
