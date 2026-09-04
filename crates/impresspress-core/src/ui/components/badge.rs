//! Badge — single source of truth for the small status pill.

use maud::{html, Markup};

/// Color variant for [`badge`]. Typed so call sites pick a variant by name
/// rather than passing a class string; [`status_badge`] is the convenience
/// that derives the variant from a status string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeVariant {
    Success,
    Danger,
    Warning,
    Info,
}

impl BadgeVariant {
    /// Map a free-form status string to a variant. Centralizes the
    /// status→color policy in one place (the only implicit mapping, and it's
    /// presentation, not data translation).
    fn from_status(status: &str) -> Self {
        match status.to_lowercase().as_str() {
            "active" | "enabled" | "completed" | "running" => BadgeVariant::Success,
            "inactive" | "disabled" | "stopped" => BadgeVariant::Danger,
            "pending" | "draft" => BadgeVariant::Warning,
            _ => BadgeVariant::Info,
        }
    }

    fn class(self) -> &'static str {
        match self {
            BadgeVariant::Success => "badge-success",
            BadgeVariant::Danger => "badge-danger",
            BadgeVariant::Warning => "badge-warning",
            BadgeVariant::Info => "badge-info",
        }
    }
}

/// Render a colored badge pill for an explicit variant. The single badge
/// renderer — [`status_badge`] delegates here.
pub fn badge(variant: BadgeVariant, label: &str) -> Markup {
    html! {
        span .badge .(variant.class()) { (label) }
    }
}

/// Render a colored status badge, deriving the color from the status string.
pub fn status_badge(status: &str) -> Markup {
    // The variant is derived from the machine value; the label is humanized
    // so snake_case enums (`partially_refunded`) never leak underscores.
    badge(BadgeVariant::from_status(status), &status.replace('_', " "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_renders_variant_class_and_label() {
        let s = badge(BadgeVariant::Success, "Online").into_string();
        assert!(s.contains("badge-success"), "variant class missing: {s}");
        assert!(s.contains(">Online</span>"), "label missing: {s}");
    }

    #[test]
    fn status_badge_delegates_to_badge_with_mapped_variant() {
        // status_badge is the single status-string entry point; it derives a
        // BadgeVariant and renders through the one `badge` function.
        assert!(status_badge("active")
            .into_string()
            .contains("badge-success"));
        assert!(status_badge("disabled")
            .into_string()
            .contains("badge-danger"));
        assert!(status_badge("pending")
            .into_string()
            .contains("badge-warning"));
        // Unknown status falls to the Info variant and keeps the label text.
        let unknown = status_badge("public").into_string();
        assert!(unknown.contains("badge-info"), "default variant: {unknown}");
        assert!(unknown.contains(">public</span>"), "label text: {unknown}");
    }

    #[test]
    fn status_badge_humanizes_snake_case_labels() {
        // Machine enum values must never leak underscores into the UI:
        // `partially_refunded` renders as "partially refunded".
        let partial = status_badge("partially_refunded").into_string();
        assert!(
            partial.contains(">partially refunded</span>"),
            "humanized label: {partial}"
        );
        assert!(
            !partial.contains("partially_refunded"),
            "raw enum: {partial}"
        );
    }
}
