//! Data Table (Phase 1)

/// One column declaration for `data_table`.
pub struct TableCol<'a> {
    pub label: &'a str,
    pub width: Option<&'a str>, // CSS width, e.g. "160px" or "30%"
}

/// `data_table` — caller passes pre-rendered cell markup per row.
/// Sticky header. Optional row-link via `row_href` closure.
///
/// `rows` is a Vec because we need to know if it's empty. If empty, an
/// `empty_state` is rendered in place of the table body.
///
/// Each `<td>` carries `data-label="{column label}"` so the mobile
/// card-collapse CSS (`.data-table td::before { content: attr(data-label) }`,
/// the PR #75 responsive fix) labels every stacked cell automatically. Cells
/// are matched to columns positionally; a cell beyond the declared columns
/// (shouldn't happen) gets an empty label.
pub fn data_table<'a, F>(
    columns: &[TableCol<'a>],
    rows: Vec<Vec<maud::Markup>>,
    row_href: Option<F>,
    empty: maud::Markup,
) -> maud::Markup
where
    F: Fn(usize) -> Option<String>,
{
    use maud::html;
    if rows.is_empty() {
        return html! { div .data-table__empty { (empty) } };
    }
    html! {
        div .data-table {
            table {
                thead { tr {
                    @for col in columns {
                        @match col.width {
                            // Caller-declared, per-instance column width -- a
                            // genuine runtime value, so it's handed to CSS as
                            // a custom property rather than a literal inline
                            // width declaration.
                            Some(w) => th .data-table__col-w style=(format!("--col-width:{w}")) { (col.label) },
                            None => th { (col.label) },
                        }
                    }
                } }
                tbody {
                    @for (i, cells) in rows.into_iter().enumerate() {
                        @let href = row_href.as_ref().and_then(|f| f(i));
                        tr .(if href.is_some() { "data-table__row data-table__row--linked" } else { "data-table__row" }) {
                            @for (j, cell) in cells.into_iter().enumerate() {
                                td data-label=(columns.get(j).map(|c| c.label).unwrap_or("")) { (cell) }
                            }
                            @if let Some(h) = href {
                                td .data-table__row-href { a href=(h) aria-label="Open" { "›" } }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::components::empty_state;

    #[test]
    fn data_table_empty_renders_empty_slot() {
        let cols = [TableCol {
            label: "Name",
            width: None,
        }];
        let empty = empty_state(
            maud::html! { "📭" },
            "No users",
            "Invite someone to get started.",
            None,
        );
        let s =
            data_table::<fn(usize) -> Option<String>>(&cols, Vec::new(), None, empty).into_string();
        assert!(s.contains("data-table__empty"));
        assert!(s.contains("No users"));
        assert!(!s.contains("<tbody>"));
    }

    #[test]
    fn data_table_with_rows_renders_thead_and_tbody() {
        let cols = [
            TableCol {
                label: "Name",
                width: Some("200px"),
            },
            TableCol {
                label: "Role",
                width: None,
            },
        ];
        let rows = vec![
            vec![maud::html! { "alice" }, maud::html! { "admin" }],
            vec![maud::html! { "bob" }, maud::html! { "user" }],
        ];
        let s = data_table::<fn(usize) -> Option<String>>(
            &cols,
            rows,
            None,
            empty_state(maud::html! {}, "", "", None),
        )
        .into_string();
        assert!(s.contains("<thead>"));
        assert!(s.contains("<tbody>"));
        assert!(s.contains("alice"));
        assert!(s.contains(r#"style="--col-width:200px""#));
    }

    #[test]
    fn data_table_row_href_renders_link_cell() {
        let cols = [TableCol {
            label: "Name",
            width: None,
        }];
        let rows = vec![vec![maud::html! { "alice" }]];
        let s = data_table(
            &cols,
            rows,
            Some(|i: usize| Some(format!("/users/{i}"))),
            empty_state(maud::html! {}, "", "", None),
        )
        .into_string();
        assert!(s.contains(r#"href="/users/0""#));
        assert!(s.contains("data-table__row--linked"));
    }
}
