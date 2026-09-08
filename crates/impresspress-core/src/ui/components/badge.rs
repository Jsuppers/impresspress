//! Badge — single source of truth for the small status pill.

use maud::{html, Markup};

/// Color variant for [`badge`]. Typed so call sites pick a variant by name
/// rather than passing a class string; [`status_badge`] is the convenience
/// that derives the variant from a status string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeVariant {
    Success,
    Primary,
    Danger,
    Warning,
    Info,
    Secondary,
    /// The five tone variants below are the shared colour set the block-detail
    /// modal uses for HTTP methods and auth levels. They are named after the
    /// colour rather than after a meaning because two unrelated enums share
    /// them — see the comment above `.badge--tone-brand` in `badge.css`.
    ToneBrand,
    ToneGreen,
    ToneAmber,
    ToneRed,
    ToneSlate,
}

impl BadgeVariant {
    /// Every variant, in stylesheet order. `badge_variants_cover_every_colour_class_in_the_stylesheet`
    /// asserts this list renders exactly the colour classes `badge.css` defines,
    /// so a class added there without a variant fails the build.
    pub const ALL: &'static [BadgeVariant] = &[
        BadgeVariant::Success,
        BadgeVariant::Primary,
        BadgeVariant::Danger,
        BadgeVariant::Warning,
        BadgeVariant::Info,
        BadgeVariant::Secondary,
        BadgeVariant::ToneBrand,
        BadgeVariant::ToneGreen,
        BadgeVariant::ToneAmber,
        BadgeVariant::ToneRed,
        BadgeVariant::ToneSlate,
    ];

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

    pub(crate) fn class(self) -> &'static str {
        match self {
            BadgeVariant::Success => "badge-success",
            BadgeVariant::Primary => "badge-primary",
            BadgeVariant::Danger => "badge-danger",
            BadgeVariant::Warning => "badge-warning",
            BadgeVariant::Info => "badge-info",
            BadgeVariant::Secondary => "badge-secondary",
            BadgeVariant::ToneBrand => "badge--tone-brand",
            BadgeVariant::ToneGreen => "badge--tone-green",
            BadgeVariant::ToneAmber => "badge--tone-amber",
            BadgeVariant::ToneRed => "badge--tone-red",
            BadgeVariant::ToneSlate => "badge--tone-slate",
        }
    }
}

/// A badge that carries more than a colour and a plain text label.
///
/// Call sites reach for this when the pill needs a utility class
/// (`.text-11`, `.mr-1`), a `title`, or markup content (`"v" (version)`).
/// [`badge`] is the plain-text shorthand and delegates here, so there is still
/// exactly one place that emits `<span class="badge …">`.
pub struct Badge<'a> {
    variant: BadgeVariant,
    classes: &'a str,
    title: Option<&'a str>,
}

impl<'a> Badge<'a> {
    /// A badge of `variant` with no utility classes and no `title`.
    pub fn new(variant: BadgeVariant) -> Self {
        Badge {
            variant,
            classes: "",
            title: None,
        }
    }

    /// Utility classes appended after the variant class, space-separated in
    /// the order given — the same order the hand-written markup used.
    pub fn classes(mut self, classes: &'a str) -> Self {
        self.classes = classes;
        self
    }

    /// The pill's `title` attribute, emitted after `class`.
    pub fn title(mut self, title: &'a str) -> Self {
        self.title = Some(title);
        self
    }

    /// Render the pill around `content`.
    pub fn render(self, content: Markup) -> Markup {
        // One `class` value built by hand rather than two maud class
        // shorthands: an empty `.("")` would leave a trailing space in the
        // attribute and change the rendered bytes.
        let class = if self.classes.is_empty() {
            self.variant.class().to_string()
        } else {
            format!("{} {}", self.variant.class(), self.classes)
        };
        html! {
            span .badge .(class) title=[self.title] { (content) }
        }
    }
}

/// Render a colored badge pill for an explicit variant. The plain-text
/// shorthand for [`Badge`]; [`status_badge`] delegates here.
pub fn badge(variant: BadgeVariant, label: &str) -> Markup {
    Badge::new(variant).render(html! { (label) })
}

/// Render a colored status badge, deriving the color from the status string.
pub fn status_badge(status: &str) -> Markup {
    // The variant is derived from the machine value; the label is humanized
    // so snake_case enums (`partially_refunded`) never leak underscores.
    badge(BadgeVariant::from_status(status), &status.replace('_', " "))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Drop `/* … */` blocks so a class named inside a comment is not read as a
    /// rule. `badge.css` discusses `.badge-primary` and `.badge--tone-green` in
    /// prose, so this is load-bearing, not defensive.
    fn strip_css_comments(css: &str) -> String {
        let mut out = String::with_capacity(css.len());
        let mut rest = css;
        while let Some(start) = rest.find("/*") {
            out.push_str(&rest[..start]);
            match rest[start + 2..].find("*/") {
                Some(end) => rest = &rest[start + 2 + end + 2..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    /// Every `.badge-…` rule in the stylesheet that paints a `background` — the
    /// convention that separates a colour class from a layout modifier
    /// (`.badge--center` sets only `justify-content`) and from the `.badge`
    /// base rule (which paints nothing).
    fn stylesheet_colour_classes() -> BTreeSet<String> {
        let css = strip_css_comments(include_str!("../styles/components/badge.css"));
        let mut found = BTreeSet::new();
        for rule in css.split('}') {
            let Some((selector, body)) = rule.split_once('{') else {
                continue;
            };
            if !body.contains("background") {
                continue;
            }
            for sel in selector.split(',') {
                let sel = sel.trim();
                if let Some(class) = sel.strip_prefix('.') {
                    if class.starts_with("badge-") {
                        found.insert(class.to_string());
                    }
                }
            }
        }
        found
    }

    #[test]
    fn badge_variants_cover_every_colour_class_in_the_stylesheet() {
        // The reason admin pages hand-wrote 39 badge spans: the type offered
        // four colours where `badge.css` paints eleven, so six of the eight
        // classes admin used could not be named through the component. The
        // parity is derived from the stylesheet rather than a second list, so
        // a colour added to `badge.css` fails here until it has a variant.
        let defined = stylesheet_colour_classes();
        let variants: BTreeSet<String> = BadgeVariant::ALL
            .iter()
            .map(|v| v.class().to_string())
            .collect();
        assert_eq!(
            defined,
            variants,
            "BadgeVariant and badge.css disagree; \
             in the stylesheet only: {:?}; in the enum only: {:?}",
            defined.difference(&variants).collect::<Vec<_>>(),
            variants.difference(&defined).collect::<Vec<_>>(),
        );
    }

    /// Every file that still writes a badge pill in maud rather than through
    /// this module, with the number of pills it writes. A ratchet: a file that
    /// is not listed must emit none, and a listed file's count must be exact,
    /// so a migration cannot half-land and a new hand-written badge cannot
    /// appear anywhere. Naming the count as well as the file is what makes it
    /// a gate rather than a note — an entry that only said "this file still
    /// has some" would still pass after a tenth was added.
    ///
    /// `blocks/admin/` is absent because this pull request migrated its 39.
    /// The rest are phase 5 §8 candidates and out of scope here.
    const HAND_WRITTEN_BADGES: &[(&str, usize)] = &[
        ("blocks/files/pages_user/buckets.rs", 2),
        ("blocks/legalpages/pages.rs", 12),
        ("blocks/llm/ui.rs", 10),
        ("blocks/messages/pages.rs", 6),
        ("blocks/products/pages.rs", 6),
        ("blocks/tickets/pages.rs", 4),
        ("blocks/vector/pages_ui.rs", 4),
        // A doc-test fixture for `templates::entity_header`, not a page.
        ("ui/templates.rs", 1),
    ];

    /// Count maud's bare `.badge` class shorthand in `src`: preceded by
    /// whitespace, and not the start of `.badge-success` or `.badge--tone-red`
    /// (those follow the bare class on the same element, so counting them too
    /// would count one pill several times).
    fn hand_written_badges(src: &str) -> usize {
        let bytes = src.as_bytes();
        let mut count = 0;
        for (i, _) in src.match_indices(".badge") {
            let preceded_by_space = i
                .checked_sub(1)
                .is_some_and(|p| bytes[p].is_ascii_whitespace());
            let next = bytes.get(i + ".badge".len()).copied();
            let continues_class =
                next.is_some_and(|c| c == b'-' || c == b'_' || c.is_ascii_alphanumeric());
            if preceded_by_space && !continues_class {
                count += 1;
            }
        }
        count
    }

    #[test]
    fn only_the_declared_files_still_hand_write_badge_markup() {
        let src_root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let expected: std::collections::BTreeMap<&str, usize> =
            HAND_WRITTEN_BADGES.iter().copied().collect();
        let mut found: std::collections::BTreeMap<String, usize> = Default::default();
        for entry in walkdir::WalkDir::new(src_root)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
        {
            let rel = entry
                .path()
                .strip_prefix(src_root)
                .unwrap()
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            // This module is where the markup is supposed to be written.
            if rel == "ui/components/badge.rs" {
                continue;
            }
            let count = hand_written_badges(&std::fs::read_to_string(entry.path()).unwrap());
            if count > 0 {
                found.insert(rel, count);
            }
        }
        let found_refs: std::collections::BTreeMap<&str, usize> =
            found.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        assert_eq!(
            found_refs, expected,
            "hand-written badge markup moved; update HAND_WRITTEN_BADGES only to \
             remove entries or lower counts"
        );
    }

    #[test]
    fn badge_carries_utility_classes_and_a_title_verbatim() {
        // The second reason admin hand-wrote badges: many of them carry a
        // spacing or type-scale utility class, one carries a `title`, and
        // several hold markup rather than a plain label. The rendered bytes
        // must match what the hand-written maud emitted, attribute order
        // included, or the migration moves a visual baseline.
        let rendered = Badge::new(BadgeVariant::Info)
            .classes("text-xs")
            .title("Database backend")
            .render(html! { "SQLite" " · " (3) " tables" });
        assert_eq!(
            rendered.into_string(),
            r#"<span class="badge badge-info text-xs" title="Database backend">SQLite · 3 tables</span>"#
        );
    }

    #[test]
    fn a_bare_badge_carries_no_trailing_space_and_no_title() {
        // Asserted against the literal bytes, not against `badge`: `badge`
        // delegates to the builder, so comparing the two would pass even if
        // both grew a trailing space in the class attribute. The 39 admin
        // pills this replaced emitted exactly this.
        let expected = r#"<span class="badge badge--tone-slate">http</span>"#;
        assert_eq!(
            Badge::new(BadgeVariant::ToneSlate)
                .render(html! { "http" })
                .into_string(),
            expected
        );
        assert_eq!(
            badge(BadgeVariant::ToneSlate, "http").into_string(),
            expected
        );
    }

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
