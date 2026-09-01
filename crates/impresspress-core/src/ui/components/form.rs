//! Search Input

use maud::{html, Markup};

use crate::ui::icons;

/// Render a search input with htmx-powered search.
/// If `current_value` is non-empty, shows a "Results for X" banner with a clear button.
pub fn search_input(name: &str, placeholder: &str, hx_get: &str, hx_target: &str) -> Markup {
    search_input_with_value(name, placeholder, hx_get, hx_target, "")
}

/// Search input with a pre-filled value and results banner.
pub fn search_input_with_value(
    name: &str,
    placeholder: &str,
    hx_get: &str,
    hx_target: &str,
    current_value: &str,
) -> Markup {
    html! {
        @if !current_value.is_empty() {
            div .flex .items-center .gap-2 .mb-2 style="font-size:0.875rem" {
                span .text-muted { "Results for " }
                span .font-semibold { "\"" (current_value) "\"" }
                a .btn .btn-ghost .btn-sm
                    href=(hx_get)
                    hx-get=(hx_get)
                    hx-target=(hx_target)
                { (icons::x()) " Clear" }
            }
        }
        div .search-input {
            span .search-input-icon { (icons::search()) }
            input .form-input
                type="search"
                name=(name)
                placeholder=(placeholder)
                value=(current_value)
                hx-get=(hx_get)
                hx-trigger="input changed delay:300ms, search"
                hx-target=(hx_target)
                autocomplete="off";
        }
    }
}
