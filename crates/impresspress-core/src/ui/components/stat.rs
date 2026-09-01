//! Stat Card

use maud::{html, Markup};

/// Render a stat card.
pub fn stat_card(label: &str, value: &str, icon: Markup) -> Markup {
    html! {
        div .stat-card {
            div .stat-header {
                div .stat-content {
                    div .stat-label { (label) }
                    div .stat-value { (value) }
                }
                div .stat-icon { (icon) }
            }
        }
    }
}
