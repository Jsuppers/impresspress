//! Data Table (Phase 1)

use std::borrow::Cow;

use maud::{html, Markup};

/// One column declaration for `data_table`.
pub struct TableCol<'a> {
    pub label: &'a str,
    pub width: Option<&'a str>, // CSS width, e.g. "160px" or "30%"
}

/// One row of a [`DataTable`].
///
/// The cells are the row's *inner* markup, one `Markup` per column; the
/// component owns the `<td>` and carries what it is given through verbatim.
/// That is deliberate and is what lets a caller keep something inside a cell
/// that must survive the migration — the visual-baseline suite's mask hooks
/// (`<time>`, `<span data-volatile-metric>`) are the worked example — without
/// the component growing a general per-cell attribute affordance.
///
/// The three optional pieces exist because administration's tables needed
/// them and nothing in the cell markup could express them:
///
/// - [`id`](TableRow::id) — an htmx swap target. The users table swaps one
///   row's `outerHTML` after an enable/disable.
/// - [`classes`](TableRow::classes) — extra classes on the `<tr>` itself,
///   for a row that carries a page-local behaviour or style
///   (`.expand-row` on the network page).
/// - [`after`](TableRow::after) — markup emitted immediately after the row's
///   `</tr>`, still inside the `<tbody>`. A row whose detail is loaded
///   lazily into a second, full-width `<tr>` needs one; the component does
///   not know what that second row contains, so the caller writes it.
pub struct TableRow {
    cells: Vec<Markup>,
    id: Option<String>,
    classes: String,
    after: Option<Markup>,
}

impl TableRow {
    /// A plain row: one cell of inner markup per column, no id, no extra
    /// classes, nothing after it.
    pub fn new(cells: Vec<Markup>) -> Self {
        TableRow {
            cells,
            id: None,
            classes: String::new(),
            after: None,
        }
    }

    /// The `<tr>`'s `id`, emitted before `class`. An htmx `hx-target` needs
    /// one; nothing else should.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Extra classes appended after the row's own `data-table__row` classes,
    /// space-separated in the order given.
    pub fn classes(mut self, classes: impl Into<String>) -> Self {
        self.classes = classes.into();
        self
    }

    /// Markup emitted immediately after this row's `</tr>`, inside the same
    /// `<tbody>`. The caller writes a complete `<tr>`; the component only
    /// places it.
    pub fn after(mut self, markup: Markup) -> Self {
        self.after = Some(markup);
        self
    }

    /// Render this row on its own, against `columns`, with no surrounding
    /// table — the `<tr>` plus whatever [`after`](TableRow::after) puts behind
    /// it, exactly as [`DataTable::render`] would have emitted them.
    ///
    /// This exists for the htmx `outerHTML` swap of a single row: the
    /// administration users table replaces one row after an enable or a
    /// disable, and the replacement has to carry the same classes and the same
    /// `data-label` cells as the row it replaces. Rendering it through the
    /// component is what stops the two from drifting.
    pub fn render(self, columns: &[TableCol<'_>]) -> Markup {
        self.render_in(columns, None)
    }

    /// The one row renderer. `href`, when set, appends the row-link chevron
    /// cell and puts the row in its `--linked` state.
    fn render_in(self, columns: &[TableCol<'_>], href: Option<String>) -> Markup {
        let TableRow {
            cells,
            id,
            classes,
            after,
        } = self;
        let row_class = row_class(&classes, href.is_some());
        html! {
            tr id=[id] class=(row_class) {
                @for (j, cell) in cells.into_iter().enumerate() {
                    td data-label=(columns.get(j).map(|c| c.label).unwrap_or("")) { (cell) }
                }
                @if let Some(h) = href {
                    td .data-table__row-href { a href=(h) aria-label="Open" { "›" } }
                }
            }
            @if let Some(after) = after { (after) }
        }
    }
}

/// The shared data table.
///
/// Sticky header, mobile card-collapse, optional row link. Each `<td>`
/// carries `data-label="{column label}"` so the mobile card-collapse CSS
/// (`.data-table td::before { content: attr(data-label) }`, the PR #75
/// responsive fix) labels every stacked cell automatically. Cells are matched
/// to columns positionally; a cell beyond the declared columns (shouldn't
/// happen) gets an empty label.
///
/// `rows` is a `Vec` because the component needs to know whether it is empty:
/// an empty table renders the `empty` slot in place of the whole table, header
/// included.
///
/// [`data_table`] is the four-argument shorthand and delegates here.
pub struct DataTable<'a> {
    columns: &'a [TableCol<'a>],
    rows: Vec<TableRow>,
    row_href: Option<Box<dyn Fn(usize) -> Option<String> + 'a>>,
    empty: Markup,
    head: bool,
}

impl<'a> DataTable<'a> {
    /// An empty table over `columns`: no rows, no row link, an empty
    /// empty-slot, header shown.
    pub fn new(columns: &'a [TableCol<'a>]) -> Self {
        DataTable {
            columns,
            rows: Vec::new(),
            row_href: None,
            empty: html! {},
            head: true,
        }
    }

    /// The rows, in render order.
    pub fn rows(mut self, rows: Vec<TableRow>) -> Self {
        self.rows = rows;
        self
    }

    /// Make each row link to a destination, by row index. Adds the trailing
    /// chevron cell and the `--linked` hover affordance.
    pub fn row_href(mut self, href: impl Fn(usize) -> Option<String> + 'a) -> Self {
        self.row_href = Some(Box::new(href));
        self
    }

    /// What to render when there are no rows.
    pub fn empty(mut self, empty: Markup) -> Self {
        self.empty = empty;
        self
    }

    /// Suppress the `<thead>`. The column labels are still declared and still
    /// reach every `<td>`'s `data-label`, so the mobile card-collapse keeps
    /// naming its cells — this only drops the header band. For a table that
    /// reads as a two-column list rather than as a grid (the administration
    /// dashboard's "Recent Users" card).
    pub fn headless(mut self) -> Self {
        self.head = false;
        self
    }

    /// Render the table.
    pub fn render(self) -> Markup {
        if self.rows.is_empty() {
            return html! { div .data-table__empty { (self.empty) } };
        }
        let columns = self.columns;
        let head = self.head;
        let row_href = self.row_href;
        html! {
            div .data-table {
                table {
                    @if head {
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
                    }
                    tbody {
                        @for (i, row) in self.rows.into_iter().enumerate() {
                            (row.render_in(columns, row_href.as_ref().and_then(|f| f(i))))
                        }
                    }
                }
            }
        }
    }
}

/// The `<tr>`'s class list: the base row class, `--linked` when the row is a
/// link, then whatever the caller added. Borrowed for the two unadorned
/// shapes, which is every row products renders.
fn row_class(extra: &str, linked: bool) -> Cow<'_, str> {
    let base = if linked {
        "data-table__row data-table__row--linked"
    } else {
        "data-table__row"
    };
    if extra.is_empty() {
        Cow::Borrowed(base)
    } else {
        Cow::Owned(format!("{base} {extra}"))
    }
}

/// `data_table` — caller passes pre-rendered cell markup per row.
/// Sticky header. Optional row-link via `row_href` closure.
///
/// The four-argument shorthand for [`DataTable`], for the majority of call
/// sites whose rows carry no id, no extra classes and no follow-up row.
pub fn data_table<'a, F>(
    columns: &[TableCol<'a>],
    rows: Vec<Vec<maud::Markup>>,
    row_href: Option<F>,
    empty: maud::Markup,
) -> maud::Markup
where
    F: Fn(usize) -> Option<String>,
{
    let mut table = DataTable::new(columns)
        .rows(rows.into_iter().map(TableRow::new).collect())
        .empty(empty);
    if let Some(f) = row_href {
        table = table.row_href(f);
    }
    table.render()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::{components::empty_state, icons};

    #[test]
    fn data_table_empty_renders_empty_slot() {
        let cols = [TableCol {
            label: "Name",
            width: None,
        }];
        let empty = empty_state(
            icons::inbox(),
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

    /// The shorthand is the builder with nothing set, so its bytes must not
    /// have moved — every products call site goes through it.
    #[test]
    fn shorthand_and_builder_render_the_same_bytes() {
        let cols = [
            TableCol {
                label: "Name",
                width: None,
            },
            TableCol {
                label: "Role",
                width: None,
            },
        ];
        let cells = || {
            vec![
                vec![maud::html! { "alice" }, maud::html! { "admin" }],
                vec![maud::html! { "bob" }, maud::html! { "user" }],
            ]
        };
        let shorthand =
            data_table::<fn(usize) -> Option<String>>(&cols, cells(), None, html! {}).into_string();
        let builder = DataTable::new(&cols)
            .rows(cells().into_iter().map(TableRow::new).collect())
            .render()
            .into_string();
        assert_eq!(shorthand, builder);
        assert!(shorthand.contains(r#"<div class="data-table">"#));
        assert!(shorthand.contains(r#"<tr class="data-table__row">"#));
    }

    #[test]
    fn row_id_and_classes_reach_the_tr() {
        let cols = [TableCol {
            label: "Key",
            width: None,
        }];
        let s = DataTable::new(&cols)
            .rows(vec![TableRow::new(vec![html! { "k" }])
                .id("var-row-K")
                .classes("expand-row")])
            .render()
            .into_string();
        assert!(
            s.contains(r#"<tr id="var-row-K" class="data-table__row expand-row">"#),
            "{s}"
        );
    }

    #[test]
    fn row_after_markup_follows_the_row_inside_the_tbody() {
        let cols = [TableCol {
            label: "Key",
            width: None,
        }];
        let s = DataTable::new(&cols)
            .rows(vec![TableRow::new(vec![html! { "k" }]).after(
                html! { tr .detail-rows hidden { td colspan="1" { "detail" } } },
            )])
            .render()
            .into_string();
        assert!(
            s.contains(
                r#"</tr><tr class="detail-rows" hidden><td colspan="1">detail</td></tr></tbody>"#
            ),
            "{s}"
        );
    }

    /// The single-row htmx swap has to be byte-identical to the row the table
    /// itself emits, or the swapped-in row loses its classes and its
    /// `data-label` cells the moment it is replaced.
    #[test]
    fn standalone_row_matches_the_row_the_table_emits() {
        let cols = [
            TableCol {
                label: "Email",
                width: None,
            },
            TableCol {
                label: "Created",
                width: None,
            },
        ];
        let row = || {
            TableRow::new(vec![html! { "a@example.com" }, html! { "2026-01-01" }]).id("user-row-1")
        };
        let in_table = DataTable::new(&cols)
            .rows(vec![row()])
            .render()
            .into_string();
        let standalone = row().render(&cols).into_string();
        assert!(in_table.contains(&standalone), "{in_table} !⊇ {standalone}");
        assert!(
            standalone.starts_with(r#"<tr id="user-row-1" class="data-table__row">"#),
            "{standalone}"
        );
        assert!(standalone.ends_with("</tr>"), "{standalone}");
    }

    #[test]
    fn headless_drops_the_thead_but_keeps_the_cell_labels() {
        let cols = [TableCol {
            label: "Email",
            width: None,
        }];
        let s = DataTable::new(&cols)
            .rows(vec![TableRow::new(vec![html! { "a@example.com" }])])
            .headless()
            .render()
            .into_string();
        assert!(!s.contains("<thead>"), "{s}");
        assert!(s.contains(r#"data-label="Email""#), "{s}");
    }
}
