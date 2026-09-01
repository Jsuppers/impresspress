//! Modal

use maud::{html, Markup};

use crate::ui::icons;

/// Render a modal container (hidden by default).
pub fn modal(id: &str, title: &str, body: Markup) -> Markup {
    html! {
        div .modal-overlay id=(id) hidden
            onclick="if(event.target===this)closeModal(this.id)"
        {
            div .modal {
                div .modal-header {
                    h3 .modal-title { (title) }
                    button .modal-close onclick={"closeModal('" (id) "')"} {
                        (icons::x())
                    }
                }
                div .modal-body {
                    (body)
                }
            }
        }
    }
}
