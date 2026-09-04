//! Empty State

pub fn empty_state(
    icon: maud::Markup,
    title: &str,
    body: &str,
    action: Option<maud::Markup>,
) -> maud::Markup {
    use maud::html;
    html! {
        div .empty {
            div .empty__icon { (icon) }
            h3 .empty__title { (title) }
            p .empty__body { (body) }
            @if let Some(a) = action { div .empty__action { (a) } }
        }
    }
}
