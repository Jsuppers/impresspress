//! Stat Card

use maud::{html, Markup};

/// Dashboard stat tile: icon, uppercase label, value, optional sparkline.
pub fn stat_card(label: &str, value: &str, icon: Markup, spark: Option<Markup>) -> Markup {
    html! {
        div .stat-card {
            div .stat-header {
                div .stat-icon { (icon) }
                @if let Some(s) = spark { div .stat-spark { (s) } }
            }
            div .stat-label { (label) }
            div .stat-value { (value) }
        }
    }
}
