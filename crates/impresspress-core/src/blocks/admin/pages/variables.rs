use maud::{html, Markup};
use wafer_run::{context::Context, InputStream, Message, OutputStream};

use crate::{
    blocks::admin::ops,
    http::{err_internal, err_not_found},
    platform_state::variables,
    ui::{
        self,
        components::{self, Badge, BadgeVariant},
        icons,
    },
    util::parse_form_body,
};

/// Render JUST the variables settings body. The parent `settings_page`
/// handler wraps this in the form-less `tabbed_page` shell — this body's
/// Add-Variable modal renders its own `<form hx-post="/b/admin/variables">`
/// (and the htmx-loaded edit modal its own `<form hx-put=...>`), which is
/// only valid because the shell contributes no outer `<form>` to nest in.
pub async fn settings_body(ctx: &dyn Context, msg: &Message) -> Markup {
    let tab = msg.query("tab");
    let active_tab = if tab == "all" { "all" } else { "blocks" };

    html! {
        div .mb-3 {
            button .btn .btn--primary .btn--sm data-action="modal-open" data-modal-target="create-var" {
                (icons::plus()) " Add Variable"
            }
        }

        (components::tab_navigation(vec![
            components::Tab {
                active: active_tab == "blocks",
                href: "/b/admin/settings/variables",
                label: "By Block",
                icon: Some(icons::package()),
            },
            components::Tab {
                active: active_tab == "all",
                href: "/b/admin/settings/variables?tab=all",
                label: "All Variables",
                icon: Some(icons::file_text()),
            },
        ]))

        div #variables-content {
            @if active_tab == "all" {
                (config_all_tab(ctx).await)
            } @else {
                (config_by_block_tab(ctx).await)
            }
        }

        // Create variable modal
        (components::modal("create-var", "Add Variable", html! {
            form hx-post="/b/admin/variables" hx-target="#variables-content" {
                div .form-group {
                    label .form-label .required for="var-key" { "Key" }
                    input .form-input type="text" #var-key name="key" placeholder="e.g. MY_SETTING" required;
                }
                div .form-group {
                    label .form-label for="var-value" { "Value" }
                    input .form-input type="text" #var-value name="value" placeholder="Value";
                }
                div .form-group {
                    label .form-label for="var-desc" { "Description" }
                    input .form-input type="text" #var-desc name="description" placeholder="Optional description";
                }
                div .form-group {
                    label .form-checkbox {
                        // Hidden first, checkbox second: `parse_form_body` keeps
                        // the last value for a repeated key, so a checked box
                        // posts `1` and an unchecked one still posts an explicit
                        // `0` rather than nothing. Checked by default — masking
                        // is the safe side to be wrong on.
                        input type="hidden" name="sensitive" value="0";
                        input type="checkbox" name="sensitive" value="1" checked;
                        " Sensitive (mask value in UI)"
                    }
                }
                div .form-actions {
                    button .btn .btn--secondary type="button" data-action="modal-close" data-modal-target="create-var" { "Cancel" }
                    button .btn .btn--primary type="submit" { "Create" }
                }
            }
        }))

        // Edit variable modal (content loaded dynamically via htmx)
        div .modal-overlay #edit-var-modal-overlay hidden data-modal-dismiss
        {
            div .modal {
                div #edit-var-modal {}
            }
        }
    }
}

/// Full settings page for variables — used by mutation handlers that need to
/// re-render the complete page after a create/update. Delegates to the
/// canonical `settings_page` so both call paths share one composition.
async fn variables_page(ctx: &dyn Context, msg: &Message) -> OutputStream {
    super::settings::settings_page(ctx, msg, "variables").await
}

/// How a variable's value cell should render. SEC-060: the masking decision
/// is made once, by `ops::is_sensitive_key`, so every table agrees on it.
enum ValueState {
    /// Sensitive value present — show the mask.
    Masked,
    /// Non-sensitive value present — show it verbatim.
    Plain(String),
    /// No value stored (block-config tables distinguish this from empty).
    NotSet,
}

impl ValueState {
    /// Resolve the value cell from a key + raw value + sensitive flag, applying
    /// the SEC-060 key rule (suffix OR declaration) via
    /// `ops::is_sensitive_key`. `track_unset`
    /// controls whether an empty value renders as `(not set)` (block-config
    /// tables) or as an empty `code` cell (flat DB-record tables).
    fn resolve(key: &str, value: &str, sensitive_flag: i64, track_unset: bool) -> Self {
        let sensitive = ops::is_sensitive_key(key, sensitive_flag);
        if track_unset && value.is_empty() {
            ValueState::NotSet
        } else if sensitive {
            ValueState::Masked
        } else {
            ValueState::Plain(value.to_string())
        }
    }
}

/// The data needed to render one variable table row. Built per-section, then
/// handed to [`var_row`] so the masking/edit-button/warning markup lives in
/// one place.
struct VarRow<'a> {
    key: &'a str,
    /// Friendly name shown under the key (block-config tables only).
    name: Option<&'a str>,
    value: ValueState,
    /// Declared default / auto-generate state (block-config tables only).
    default: Option<&'a str>,
    auto_generate: bool,
    description: &'a str,
    warning: &'a str,
    /// Whether to render the "Default" column cell (block-config tables).
    show_default: bool,
    /// Whether this row offers the delete control.
    ///
    /// Explicit rather than derived from [`ValueState`], because only the
    /// caller knows whether a stored row exists: an unowned variable always
    /// has one, a declared `ConfigVar` only when someone has overridden it,
    /// and a declared var with no override has nothing to delete. Shared
    /// (`WAFER_RUN_SHARED__*`) keys are never deletable —
    /// `ops::delete_variable` refuses them, so a button there would only ever
    /// produce an error.
    deletable: bool,
}

/// Build one variable table row's cells, in column order: key (+ optional
/// name), value cell (masked per SEC-060), optional default column,
/// description (+ optional warning), and the edit button. Shared by all four
/// variable tables so the masking policy and edit affordance can't drift
/// between them. The `<td>`s around these belong to `components::data_table`.
fn var_row(row: &VarRow) -> Vec<Markup> {
    let mut cells = vec![
        html! {
            span .font-medium .text-13 {
                code { (row.key) }
                @if let Some(name) = row.name {
                    @if !name.is_empty() {
                        br;
                        span .text-muted .text-xs { (name) }
                    }
                }
            }
        },
        html! {
            span .text-13 {
                @match &row.value {
                    ValueState::Masked => code { "********" },
                    ValueState::Plain(v) => code { (v) },
                    ValueState::NotSet => span .text-muted { "(not set)" },
                }
            }
        },
    ];
    if row.show_default {
        cells.push(html! {
            span .text-xs {
                @match row.default {
                    Some(d) if !d.is_empty() => code .text-muted { (d) },
                    _ => @if row.auto_generate {
                        (Badge::new(BadgeVariant::Info).classes("text-11").render(html! { "auto-generated" }))
                    },
                }
            }
        });
    }
    cells.push(html! {
        span .text-xs {
            (row.description)
            @if !row.warning.is_empty() {
                div .var-warning-note {
                    "Warning: " (row.warning)
                }
            }
        }
    });
    // Both controls share the final cell: the cells are columns against
    // `VAR_COLUMNS`, so a conditional extra cell would misalign every row
    // that has no delete control against every row that does.
    cells.push(html! {
        div .flex .gap-1 {
            button .btn .btn--sm .btn--ghost
                hx-get={"/b/admin/variables/" (row.key) "/edit"}
                hx-target="#edit-var-modal"
                hx-swap="innerHTML"
                title="Edit"
                aria-label=(format!("Edit {}", row.key))
            { (icons::edit()) }
            @if row.deletable {
                (delete_button(row.key))
            }
        }
    });
    cells
}

/// Build and render one row for a declared [`ConfigVar`] (the shared + per-block
/// tables): pulls the stored value + sensitive flag from `var_map`, falling
/// back to the var's declared sensitivity when no DB row exists, and shows the
/// declared default / auto-generate badge.
fn config_var_row(
    var: &wafer_run::ConfigVar,
    var_map: &std::collections::HashMap<String, (String, i64)>,
) -> Vec<Markup> {
    let (db_value, sensitive_flag) = var_map
        .get(&var.key)
        .map(|(v, s)| (v.as_str(), *s))
        .unwrap_or(("", var.is_sensitive() as i64));
    var_row(&VarRow {
        key: &var.key,
        name: Some(&var.name),
        value: ValueState::resolve(&var.key, db_value, sensitive_flag, true),
        default: Some(&var.default),
        auto_generate: var.auto_generate,
        description: &var.description,
        warning: &var.warning,
        show_default: true,
        // Never, in the per-block tables. A row here exists because a block
        // DECLARES the key, not because the database does — so removing the
        // stored override must leave the row in place showing its default,
        // and this control's `outerHTML` swap would instead delete the row
        // from the table, stranding the declared key with nothing to edit
        // until a reload. "Reset to default" is a different affordance and
        // wants its own handler; the flat and unowned tables are where the
        // rows that can really be removed live.
        deletable: false,
    })
}

/// Render a titled card wrapping a variable table. `show_default` selects the
/// column list carrying the "Default" column, to match [`var_row`]'s cells.
fn var_table(header: Markup, show_default: bool, rows: Vec<Vec<Markup>>) -> Markup {
    let columns: &[components::TableCol<'static>] = if show_default {
        &VAR_COLUMNS_WITH_DEFAULT
    } else {
        &VAR_COLUMNS
    };
    html! {
        div .card .mt-4 {
            (header)
            div .card__body {
                (components::data_table::<fn(usize) -> Option<String>>(
                    columns,
                    rows,
                    None,
                    html! {},
                ))
            }
        }
    }
}

/// The variable tables' columns, in the two shapes [`var_row`] emits. The
/// last column is the one that only carries the edit control; it keeps the
/// 50px width the old `th .w-50` gave it.
const VAR_COLUMNS: [components::TableCol<'static>; 4] = [
    components::TableCol {
        label: "Key",
        width: None,
    },
    components::TableCol {
        label: "Value",
        width: None,
    },
    components::TableCol {
        label: "Description",
        width: None,
    },
    components::TableCol {
        label: "",
        width: Some("50px"),
    },
];

const VAR_COLUMNS_WITH_DEFAULT: [components::TableCol<'static>; 5] = [
    components::TableCol {
        label: "Key",
        width: None,
    },
    components::TableCol {
        label: "Value",
        width: None,
    },
    components::TableCol {
        label: "Default",
        width: None,
    },
    components::TableCol {
        label: "Description",
        width: None,
    },
    components::TableCol {
        label: "",
        width: Some("50px"),
    },
];

/// The "All Variables" tab's columns — a flatter listing than the per-block
/// tables, with an explicitly labelled actions column.
const ALL_VAR_COLUMNS: [components::TableCol<'static>; 4] = [
    components::TableCol {
        label: "Key",
        width: None,
    },
    components::TableCol {
        label: "Value",
        width: None,
    },
    components::TableCol {
        label: "Description",
        width: None,
    },
    components::TableCol {
        label: "Actions",
        width: None,
    },
];

/// The delete control, shared by every table that offers one so the affordance
/// and the confirm text cannot drift between them.
///
/// `closest tr` rather than a row id: these tables render through
/// `components::TableRow`, and only the flat "All Variables" tab gives its
/// rows ids. An empty response body is what removes the row.
fn delete_button(key: &str) -> Markup {
    html! {
        button .btn .btn--sm .btn--danger
            hx-delete={"/b/admin/variables/" (key)}
            hx-target="closest tr"
            hx-swap="outerHTML"
            hx-confirm={"Delete " (key) "? This cannot be undone."}
            title="Delete"
            aria-label=(format!("Delete {key}"))
        { (icons::trash()) }
    }
}

/// Whether the page offers a delete control for `key`, given the set of
/// shared vars this build still declares.
///
/// Mirrors `ops::delete_variable`'s refusals exactly, so the page never
/// renders a button that could only produce an error: the JWT signing secret
/// is never deletable, and a shared var is deletable only once it is no longer
/// declared (nothing re-seeds a stale row).
fn key_is_deletable(key: &str, declared_shared: &std::collections::HashSet<String>) -> bool {
    key != crate::blocks::auth::JWT_SECRET_KEY && !declared_shared.contains(key)
}

/// The shared keys this build declares, for [`key_is_deletable`]. Built once
/// per render rather than per row — `shared_config_vars()` allocates.
fn declared_shared_keys() -> std::collections::HashSet<String> {
    crate::config_vars::shared_config_vars()
        .into_iter()
        .map(|v| v.key)
        .collect()
}

/// "All Variables" tab -- flat table of all config variables from the DB.
async fn config_all_tab(ctx: &dyn Context) -> Markup {
    let settings = variables::list_all(ctx).await;
    let declared_shared = declared_shared_keys();

    html! {
        @match &settings {
            Ok(rows) => {
                @let table_rows: Vec<components::TableRow> = rows.iter().map(|row| {
                    let key = row.key.as_str();
                    let description = row.description.as_str();
                    let warning = row.warning.as_str();
                    // SEC-060: mask via the shared rule, not the `sensitive`
                    // flag alone.
                    let masked = ops::is_sensitive_key(key, i64::from(row.sensitive));
                    components::TableRow::new(vec![
                        html! { span .font-medium { (key) } },
                        html! {
                            @if masked {
                                code { "********" }
                            } @else {
                                code { (row.value) }
                            }
                        },
                        html! {
                            @if !description.is_empty() {
                                span .text-muted { (description) }
                            }
                            @if !warning.is_empty() {
                                div .text-warning-strong .text-xs .mt-1 {
                                    (ui::icons::triangle_alert()) (warning)
                                }
                            }
                        },
                        html! {
                            div .flex .gap-1 {
                                button .btn .btn--sm .btn--ghost
                                    hx-get={"/b/admin/variables/" (key) "/edit"}
                                    hx-target="#edit-var-modal"
                                    hx-swap="innerHTML"
                                    title="Edit"
                                    aria-label=(format!("Edit {key}"))
                                { (icons::edit()) }
                                // The flat listing offers the same control as
                                // the Unowned table: this is where an operator
                                // scanning for a legacy key actually looks, and
                                // two tabs disagreeing about whether a row can
                                // be removed is its own defect.
                                @if key_is_deletable(key, &declared_shared) {
                                    (delete_button(key))
                                }
                            }
                        },
                    ])
                    .id(format!("var-row-{key}"))
                }).collect();

                (components::DataTable::new(&ALL_VAR_COLUMNS)
                    .rows(table_rows)
                    .empty(html! { p .text-center .text-muted { "No variables are set." } })
                    .render())
            }
            Err(e) => {
                div .login-error { "Failed to load variables: " (e.message) }
            }
        }
    }
}

/// "By Block" tab -- groups config variables by owning block with WRAP access info.
async fn config_by_block_tab(ctx: &dyn Context) -> Markup {
    let blocks = ctx.registered_blocks();
    let shared_vars = crate::config_vars::shared_config_vars();

    // Load all variables from DB
    let all_vars = variables::list_all(ctx).await.unwrap_or_default();

    // Build a map of key -> (value, sensitive-flag). The flag is kept as the
    // `i64` `ops::is_sensitive_key` takes so the SEC-060 key rule — the
    // `_SECRET`/`_KEY` suffix or the key's own declaration — can be applied at
    // render time.
    let var_map: std::collections::HashMap<String, (String, i64)> = all_vars
        .iter()
        .map(|row| {
            (
                row.key.clone(),
                (row.value.clone(), i64::from(row.sensitive)),
            )
        })
        .collect();

    // Collect blocks that have config_keys
    let blocks_with_config: Vec<_> = blocks
        .iter()
        .filter(|b| !b.config_keys.is_empty())
        .collect();

    // Collect all known keys (block-declared + shared) to detect unowned DB vars
    let mut known_keys: std::collections::HashSet<String> = std::collections::HashSet::new();
    for block in blocks {
        for ck in &block.config_keys {
            known_keys.insert(ck.key.clone());
        }
    }
    for sv in &shared_vars {
        known_keys.insert(sv.key.clone());
    }

    // Precompute grants keyed by exact resource pattern. The per-block render
    // below used to walk `blocks × grants × config_keys` looking for matches —
    // a cubic loop for every page render. We build a single map up front so
    // the inner template just does an O(1) lookup per config key.
    let mut grants_by_resource: std::collections::HashMap<String, Vec<(&str, bool)>> =
        std::collections::HashMap::new();
    for grant_block in blocks {
        for grant in &grant_block.grants {
            grants_by_resource
                .entry(grant.resource.clone())
                .or_default()
                .push((grant.grantee.as_str(), grant.write));
        }
    }

    html! {
        // Shared variables section
        @if !shared_vars.is_empty() {
            (var_table(
                html! {
                    div .card-header {
                        h3 .card-title {
                            (Badge::new(BadgeVariant::Warning).classes("mr-2").render(html! { "shared" }))
                            " Shared Platform Config"
                        }
                        p .text-muted .text-xs {
                            "Any block can read. Only admin can write."
                        }
                    }
                },
                true,
                shared_vars.iter().map(|var| config_var_row(var, &var_map)).collect(),
            ))
        }

        // Per-block sections
        @for block in &blocks_with_config {
            (var_table(
                html! {
                    div .card-header {
                        h3 .card-title {
                            (Badge::new(BadgeVariant::Info).classes("mr-2").render(html! { (block.name) }))
                            " Configuration"
                        }
                        // Show WRAP access info for this block's config. The
                        // grants are looked up by exact resource pattern via the
                        // `grants_by_resource` map built above — used to be a
                        // cubic `blocks × grants × config_keys` loop per render.
                        p .text-muted .text-xs {
                            "Owner: " code { (block.name) }
                            " \u{2014} Admin can read/write all. "
                            @for ck in &block.config_keys {
                                @for resource in [ck.key.clone(), format!("{}*", ck.key)] {
                                    @if let Some(matches) = grants_by_resource.get(&resource) {
                                        @for (grantee, write) in matches {
                                            @if *grantee != block.name {
                                                (Badge::new(BadgeVariant::Secondary).classes("mr-1 text-11").render(html! {
                                                    (grantee) ": "
                                                    @if *write { "read+write" } @else { "read" }
                                                }))
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                },
                true,
                block.config_keys.iter().map(|var| config_var_row(var, &var_map)).collect(),
            ))
        }

        // Unowned variables section -- keys in DB not declared by any block or shared
        @let unowned_vars: Vec<_> = all_vars.iter()
            .filter(|row| !known_keys.contains(row.key.as_str()))
            .collect();
        @if !unowned_vars.is_empty() {
            (var_table(
                html! {
                    div .card-header {
                        h3 .card-title {
                            (Badge::new(BadgeVariant::Secondary).classes("mr-2").render(html! { "unowned" }))
                            " Unowned Variables"
                        }
                        p .text-muted .text-xs {
                            "Variables in the database not declared by any block. These may be legacy or manually created."
                        }
                    }
                },
                false,
                unowned_vars.iter().map(|row| {
                    let key = row.key.as_str();
                    // SEC-060: mask via the shared rule. `track_unset` is
                    // false here so an empty value renders as an empty
                    // `code` cell, matching the prior flat layout.
                    var_row(&VarRow {
                        key,
                        name: None,
                        value: ValueState::resolve(
                            key,
                            &row.value,
                            i64::from(row.sensitive),
                            false,
                        ),
                        default: None,
                        auto_generate: false,
                        description: &row.description,
                        warning: "",
                        show_default: false,
                        // Every row here exists in the database by definition
                        // — that is what "unowned" means — so these are the
                        // rows an operator needs to be able to remove.
                        //
                        // Only the JWT secret is excluded. A declared shared
                        // var cannot reach this table at all (`known_keys`
                        // covers block-declared AND shared keys, and this
                        // table is what is left over), so a
                        // `WAFER_RUN_SHARED__*` row appearing here is stale by
                        // construction and removable — which is the point.
                        deletable: key != crate::blocks::auth::JWT_SECRET_KEY,
                    })
                }).collect(),
            ))
        }
    }
}

/// POST /b/admin/variables -- create a new variable
pub async fn handle_create_variable(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let bytes = input.collect_to_bytes().await;
    let body = parse_form_body(&bytes);

    let key = body.get("key").map(|s| s.as_str()).unwrap_or("");
    let value = body.get("value").map(|s| s.as_str()).unwrap_or("");
    let description = body.get("description").map(|s| s.as_str());
    // Absent means sensitive, same as the JSON API. The modal always posts an
    // explicit value (a hidden `0` that a checked box overrides with `1`), so
    // "absent" here is a post that bypassed the form, and it fails safe.
    let sensitive = body
        .get("sensitive")
        .map(|value| crate::config_vars::is_truthy(value))
        .unwrap_or(true);

    // Key-required guard, URL/SSRF validation (the SSR path previously had
    // none), audit-log write, and the create live in the shared ops layer.
    if let Err(out) = ops::create_variable(ctx, msg, key, value, None, description, sensitive).await
    {
        return out;
    }

    // Re-render the variables page (htmx will swap #content)
    variables_page(ctx, msg).await
}

/// `GET /b/admin/variables/{key}/edit` -- return modal edit form content.
/// `{key}` is read only as the route table bound it.
pub async fn handle_edit_variable_form(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let var_key = msg.var("key");
    let row = match variables::get_by_key(ctx, var_key).await {
        Ok(Some(row)) => row,
        Ok(None) => return err_not_found("Variable not found"),
        Err(e) => return err_internal("Database error", e),
    };

    let key = row.key;
    let sensitive = row.sensitive;
    let value = row.value;
    let description = row.description;
    let warning = row.warning;
    // A key the storage rule requires cannot be unflagged; show the control as
    // set-and-locked rather than offering a change that would be refused.
    let required_sensitive = crate::config_vars::is_sensitive_for_storage(&key);
    // Mask on the EFFECTIVE flag, not the stored one. A legacy row an older
    // build stored unflagged for a required key — the row
    // `repair_sensitive_flags` exists for, still unrepaired on a deployment
    // that has not rebooted, which on Cloudflare means no `/_deploy/init`
    // since the upgrade — would otherwise render its secret in a plain text
    // input under a hint saying the variable is always sensitive.
    let show_sensitive = sensitive || required_sensitive;

    let markup = html! {
        div .modal-header {
            h3 .modal-title { "Edit Variable" }
            button .modal-close data-action="modal-close" data-modal-target="edit-var-modal-overlay" {
                (icons::x())
            }
        }
        div .modal-body {
            form hx-put={"/b/admin/variables/" (key)} hx-target="#content" {
                div .form-group {
                    label .form-label { "Key" }
                    input .form-input type="text" value=(key) disabled;
                }
                div .form-group {
                    label .form-label for="edit-value" { "Value" }
                    @if show_sensitive {
                        div .value-reveal-wrapper {
                            input .form-input #edit-value
                                type="password"
                                name="value"
                                value=(value);
                            button .btn .btn--ghost .btn--icon .btn-icon-right
                                type="button"
                                data-action="reveal-toggle"
                                data-reveal-target="edit-value"
                                data-reveal-show="Reveal"
                                data-reveal-hide="Hide"
                                title="Reveal"
                                aria-label="Reveal value"
                            { (icons::eye()) }
                        }
                    } @else {
                        input .form-input type="text" #edit-value name="value" value=(value);
                    }
                }
                div .form-group {
                    label .form-label for="edit-desc" { "Description" }
                    input .form-input type="text" #edit-desc name="description" value=(description);
                }
                div .form-group {
                    label .form-checkbox {
                        // A hidden field carries the answer and the checkbox
                        // overrides it, exactly as the Add Variable modal does:
                        // `parse_form_body` keeps the LAST value for a repeated
                        // key, so `sensitive` is always posted and the handler
                        // never has to guess whether the field was on the form.
                        //
                        // A DISABLED checkbox is not serialized — not by
                        // `FormData`, not by htmx's `shouldInclude` — so for a
                        // required key the hidden field must already say `1`.
                        // It said `0` here, under a separate presence marker,
                        // which made every required-sensitive variable
                        // uneditable: the form posted "not sensitive", the ops
                        // guard refused the unflag, and the admin's value or
                        // description edit was dropped with a 400. That hit
                        // `..._OAUTH_GOOGLE_CLIENT_SECRET`,
                        // `..._BOOTSTRAP_ADMIN_PASSWORD` and
                        // `WAFER_RUN__AUTH__JWT_SECRET` — the last of which is
                        // deliberately left rotatable by
                        // `reject_runtime_owned_key`.
                        @if required_sensitive {
                            input type="hidden" name="sensitive" value="1";
                            input type="checkbox" name="sensitive" value="1" checked disabled;
                        } @else {
                            input type="hidden" name="sensitive" value="0";
                            @if show_sensitive {
                                input type="checkbox" name="sensitive" value="1" checked;
                            } @else {
                                input type="checkbox" name="sensitive" value="1";
                            }
                        }
                        span { "Sensitive — mask this value in listings and keep it out of exports" }
                    }
                    @if required_sensitive {
                        p .form-hint {
                            "This variable is always sensitive: its declaration, or its \
                             _SECRET/_KEY name, requires it."
                        }
                    }
                }
                @if !warning.is_empty() {
                    div .var-warning-banner {
                        (ui::icons::triangle_alert()) (warning)
                    }
                }
                div .form-actions {
                    button .btn .btn--secondary type="button" data-action="modal-close" data-modal-target="edit-var-modal-overlay" { "Cancel" }
                    button .btn .btn--primary type="submit" { "Save" }
                }
            }
        }
    };

    ui::html_response_opening_modal(markup, "edit-var-modal-overlay")
}

/// `PUT`/`PATCH /b/admin/variables/{key}` -- update variable value (the row
/// is declared `PATCH`; the edit form sends `PUT`, which maps to the same
/// `update` action). `{key}` is read only as the route table bound it.
pub async fn handle_update_variable(
    ctx: &dyn Context,
    msg: &Message,
    input: InputStream,
) -> OutputStream {
    let var_key = msg.var("key");
    let bytes = input.collect_to_bytes().await;
    let body = parse_form_body(&bytes);

    // Sensitive-empty guard, URL/SSRF validation (the SSR path previously had
    // none), audit-log write, and the upsert live in the shared ops layer.
    let update = ops::VariableUpdate {
        value: body.get("value").map(|s| s.as_str()),
        description: body.get("description").map(|s| s.as_str()),
        // Present whenever the surface offers the control — the edit modal
        // always posts it, hidden field plus checkbox. Absent means the caller
        // is not editing the flag, and the stored one is left alone.
        sensitive: body
            .get("sensitive")
            .map(|value| crate::config_vars::is_truthy(value)),
    };
    if let Err(out) = ops::update_variable(ctx, msg, var_key, update).await {
        return out;
    }

    variables_page(ctx, msg).await
}

/// `DELETE /b/admin/variables/{key}` — the Variables page's row control.
///
/// The page had no delete affordance at all before this: a variable could only
/// be removed by calling `DELETE /b/admin/api/settings/{key}` by hand, which
/// is not a thing an operator can be expected to discover.
///
/// The shared-key guard, the delete and the audit row live in
/// `ops::delete_variable`, shared with that JSON surface, so the two cannot
/// drift on what they refuse.
///
/// Returns EMPTY markup rather than re-rendering the page the way
/// [`handle_update_variable`] does: the control targets `closest tr` with
/// `outerHTML`, so an empty body is what removes the row. Re-rendering the
/// whole page into a `<tr>` would nest a document inside a table row.
pub async fn handle_delete_variable(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let key = msg.var("key");
    if let Err(out) = ops::delete_variable(ctx, msg, key).await {
        return out;
    }
    // The row is gone — but an env-provided or auto-generated key is ALSO in
    // the boot map, which `blocks::config`'s read order falls back to when the
    // table holds no row. For those the value keeps being served and the row
    // is written again on the next boot, so reporting a flat "deleted" would
    // be untrue in exactly the case an operator is most likely to be trying to
    // turn something off.
    let toast = if ctx.config_get(key).is_some() {
        "Variable deleted — a boot-provided value is still in effect"
    } else {
        "Variable deleted"
    };
    ui::html_response_with_toast(html! {}, toast, "success")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{admin_msg, output_html, TestContext};

    /// Serialize a rendered form the way a BROWSER would, so a test posts what
    /// a real submit posts.
    ///
    /// Two rules carry the weight, and both are why an ops-layer test cannot
    /// stand in for this one: a `disabled` control is never serialized (not by
    /// `FormData`, not by htmx's `shouldInclude`), and an unchecked checkbox
    /// posts nothing. A hidden field with the same `name` is what carries the
    /// answer in either case — the pattern both variable modals use.
    fn serialize_form(html: &str) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        for tag in html.split("<input").skip(1) {
            let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
            if tag.contains("disabled") {
                continue;
            }
            let is_checkbox = tag.contains(r#"type="checkbox""#);
            if is_checkbox && !tag.contains("checked") {
                continue;
            }
            let attr = |name: &str| -> Option<String> {
                let pat = format!("{name}=\"");
                let i = tag.find(&pat)? + pat.len();
                let rest = &tag[i..];
                Some(rest[..rest.find('"')?].to_string())
            };
            if let (Some(name), value) = (attr("name"), attr("value")) {
                // Later fields win, matching `parse_form_body`'s last-value rule.
                out.insert(name, value.unwrap_or_default());
            }
        }
        out
    }

    fn urlencode_form(fields: &std::collections::HashMap<String, String>) -> Vec<u8> {
        fields
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    crate::util::urlencode(k),
                    crate::util::urlencode(v)
                )
            })
            .collect::<Vec<_>>()
            .join("&")
            .into_bytes()
    }

    /// Editing a REQUIRED-sensitive variable through the modal must work.
    ///
    /// The modal's Sensitive checkbox is `disabled` for such a key, so a
    /// browser posts nothing for it; the hidden field beside it has to already
    /// say `1`. It said `0` under a separate presence marker, so the form
    /// posted "not sensitive", `update_variable` refused the unflag, and the
    /// admin's value edit was dropped with a 400 — on exactly the keys that
    /// most need the edit path, including `WAFER_RUN__AUTH__JWT_SECRET`, which
    /// `reject_runtime_owned_key` deliberately leaves rotatable.
    ///
    /// Drives the real path: render the modal, serialize it as a browser
    /// would, post that.
    #[tokio::test]
    async fn a_required_sensitive_variable_can_be_edited_through_the_modal() {
        for key in [
            crate::blocks::auth::JWT_SECRET_KEY,
            crate::blocks::auth::config::BOOTSTRAP_ADMIN_PASSWORD_KEY,
        ] {
            let ctx = TestContext::with_admin().await;
            assert!(
                crate::config_vars::is_sensitive_for_storage(key),
                "{key} must be required-sensitive for this test to mean anything"
            );
            variables::insert(
                &ctx,
                variables::NewVariable {
                    key: key.to_string(),
                    value: "old-secret".to_string(),
                    name: String::new(),
                    description: String::new(),
                    warning: String::new(),
                    sensitive: true,
                    updated_by: String::new(),
                    block: variables::block_for_key(key),
                },
            )
            .await
            .expect("seed the row");

            let msg = crate::blocks::admin::test_support::routed(admin_msg(
                "retrieve",
                &format!("/b/admin/variables/{key}/edit"),
            ));
            let html = output_html(handle_edit_variable_form(&ctx, &msg).await).await;

            let mut fields = serialize_form(&html);
            assert_eq!(
                fields.get("sensitive").map(String::as_str),
                Some("1"),
                "a browser must post sensitive=1 for a required key, since the \
                 checkbox is disabled and not serialized: {html}"
            );
            fields.insert("value".to_string(), "rotated-secret".to_string());

            let put = crate::blocks::admin::test_support::routed(admin_msg(
                "update",
                &format!("/b/admin/variables/{key}"),
            ));
            let out = handle_update_variable(
                &ctx,
                &put,
                InputStream::from_bytes(urlencode_form(&fields)),
            )
            .await;
            let _ = output_html(out).await;

            let row = variables::get_by_key(&ctx, key)
                .await
                .expect("get")
                .expect("row");
            assert_eq!(
                row.value, "rotated-secret",
                "the admin's edit to {key} must land, not be dropped by the masking guard"
            );
            assert!(row.sensitive, "and the key stays masked");
        }
    }

    /// A required key whose stored row is still UNFLAGGED must render masked.
    ///
    /// That is the row `repair_sensitive_flags` exists for, seen on a
    /// deployment that has not rebooted since the upgrade — on Cloudflare, one
    /// that has had no `/_deploy/init`. Reading the checkbox and the input type
    /// off the stored flag put the secret in a plain text field directly under
    /// a hint saying the variable is always sensitive.
    #[tokio::test]
    async fn an_unrepaired_required_row_still_renders_masked() {
        let ctx = TestContext::with_admin().await;
        let key = crate::blocks::auth::JWT_SECRET_KEY;

        // `into_row` would flag it on the way in, which is the whole point —
        // this is a row an OLDER build left behind. The fixture lives in
        // `test_support` so no block file names the variables table.
        variables::seed_row_with_flag(&ctx, key, "legacy-secret", 0).await;

        let msg = crate::blocks::admin::test_support::routed(admin_msg(
            "retrieve",
            &format!("/b/admin/variables/{key}/edit"),
        ));
        let html = output_html(handle_edit_variable_form(&ctx, &msg).await).await;

        assert!(
            !html.contains(r#"type="text" name="value""#),
            "an unrepaired required key must not render its secret in a plain text input: {html}"
        );
        assert!(
            html.contains(r#"type="password" name="value""#),
            "it must use the masked input: {html}"
        );
        assert!(
            html.contains(r#"type="checkbox" name="sensitive" value="1" checked disabled"#),
            "and the control must read as set-and-locked, not unchecked: {html}"
        );
    }

    /// The Variables PAGE must mask the same unrepaired row the edit modal
    /// masks.
    ///
    /// `WAFER_RUN_SHARED__AUTH__BOOTSTRAP_ADMIN_PASSWORD` is sensitive by
    /// DECLARATION only — neither `_SECRET` nor `_KEY` — so a row an older
    /// build stored unflagged was rendered in clear in the table while the edit
    /// modal one click away rendered it masked. Whatever source settles the
    /// modal has to settle the table.
    #[tokio::test]
    async fn an_unrepaired_declaration_only_row_is_masked_in_the_table() {
        let ctx = TestContext::with_admin().await;
        let key = crate::blocks::auth::config::BOOTSTRAP_ADMIN_PASSWORD_KEY;
        assert!(
            !crate::config_vars::has_sensitive_suffix(key),
            "the point of this test is a key the suffix rule cannot catch"
        );

        variables::seed_row_with_flag(&ctx, key, "hunter2", 0).await;

        let msg =
            crate::blocks::admin::test_support::routed(admin_msg("retrieve", "/b/admin/variables"));
        let html = output_html(
            crate::blocks::admin::pages::settings::settings_page(&ctx, &msg, "variables").await,
        )
        .await;

        assert!(
            html.contains(key),
            "the row must be on the page at all, or this test proves nothing: {html}"
        );
        assert!(
            !html.contains("hunter2"),
            "the Variables page rendered an unrepaired bootstrap password in clear: {html}"
        );
    }

    /// The icon-only edit button must carry an accessible name derived from
    /// the row key (2026-07-11 review: 49 unlabeled icon buttons on the
    /// Variables page alone).
    #[test]
    fn var_row_edit_button_carries_accessible_name() {
        let cells = var_row(&VarRow {
            key: "WAFER_RUN_SHARED__APP_NAME",
            name: None,
            value: ValueState::Plain("Impresspress".to_string()),
            default: None,
            auto_generate: false,
            description: "App name",
            warning: "",
            show_default: false,
            deletable: false,
        });
        let s = components::TableRow::new(cells)
            .render(&VAR_COLUMNS, None)
            .into_string();
        assert!(
            s.contains(r#"aria-label="Edit WAFER_RUN_SHARED__APP_NAME""#),
            "edit button must expose an aria-label with the row key: {s}"
        );
    }

    fn row_html(key: &str, deletable: bool) -> String {
        let cells = var_row(&VarRow {
            key,
            name: None,
            value: ValueState::Plain("v".to_string()),
            default: None,
            auto_generate: false,
            description: "d",
            warning: "",
            show_default: false,
            deletable,
        });
        components::TableRow::new(cells)
            .render(&VAR_COLUMNS, None)
            .into_string()
    }

    /// A stored row offers a delete control, and it carries an accessible
    /// name for the same reason the edit button does.
    #[test]
    fn a_deletable_row_offers_a_labelled_delete_control() {
        let s = row_html("LEGACY_THING", true);
        assert!(
            s.contains(r#"hx-delete="/b/admin/variables/LEGACY_THING""#),
            "delete control must post to the row's own key: {s}"
        );
        assert!(
            s.contains(r#"aria-label="Delete LEGACY_THING""#),
            "icon-only delete button must expose an aria-label: {s}"
        );
    }

    /// A declared var showing its default has no stored row to delete, and a
    /// shared key is refused server-side — neither may render a control that
    /// could only fail.
    #[test]
    fn a_non_deletable_row_offers_no_delete_control() {
        let s = row_html("WAFER_RUN_SHARED__APP_NAME", false);
        assert!(
            !s.contains("hx-delete"),
            "a non-deletable row must render no delete control: {s}"
        );
        assert!(s.contains("hx-get"), "the edit control is unaffected: {s}");
    }
}

#[cfg(test)]
mod create_form_tests {
    use wafer_run::InputStream;

    use super::*;
    use crate::test_support::{admin_msg, collect_or_panic, TestContext};

    async fn admin_ctx() -> TestContext {
        let ctx = TestContext::new().await;
        crate::blocks::admin::migrations::apply(&ctx)
            .await
            .expect("apply admin migrations");
        ctx
    }

    async fn sensitive_flag(ctx: &dyn Context, key: &str) -> bool {
        variables::get_by_key(ctx, key)
            .await
            .expect("get variable")
            .unwrap_or_else(|| panic!("{key} was not created"))
            .sensitive
    }

    async fn post_form(ctx: &dyn Context, body: &str) {
        let out = handle_create_variable(
            ctx,
            &admin_msg("create", "/admin/variables"),
            InputStream::from_bytes(body.as_bytes().to_vec()),
        )
        .await;
        collect_or_panic(out).await;
    }

    /// The Variables page's create form answers the SAME 409 the JSON API
    /// does for a key that is already stored, AND the operator learns why.
    ///
    /// Both halves matter. Both surfaces drive `ops::create_variable`, so the
    /// status is the half that would notice if this page started reshaping the
    /// refusal into a re-render (an htmx swap of the full page reads as
    /// "created"). The body is the half that makes the 409 worth having: htmx
    /// does not swap a 4xx, so the only thing the operator can see is what the
    /// global `htmx:responseError` listener in `ui/assets/chrome.js` raises as
    /// a toast — and that listener reads `message` out of exactly this
    /// envelope. A 409 whose body said nothing useful would look, to the person
    /// in front of the modal, precisely like the 500 this all started as.
    /// `ui/assets/test/chrome_error_toast.test.mjs` is the listener's half.
    #[tokio::test]
    async fn form_post_with_an_existing_key_answers_conflict() {
        let ctx = admin_ctx().await;
        post_form(&ctx, "key=SITE_MOTTO&value=one").await;

        let msg = admin_msg("create", "/admin/variables");
        let refused = || {
            handle_create_variable(
                &ctx,
                &msg,
                InputStream::from_bytes(b"key=SITE_MOTTO&value=two".to_vec()),
            )
        };
        assert_eq!(
            crate::test_support::output_http_status(refused().await).await,
            409,
        );

        let body = crate::test_support::output_http_json(refused().await).await;
        assert_eq!(body["error"], serde_json::json!("AlreadyExists"));
        let message = body["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("SITE_MOTTO") && message.contains("already exists"),
            "the toast has only this to show the operator: {message:?}",
        );
    }

    /// A form post that says nothing about sensitivity — a curl'd or
    /// hand-built post, or a form that lost its checkbox — fails safe.
    #[tokio::test]
    async fn form_post_without_the_flag_defaults_to_sensitive() {
        let ctx = admin_ctx().await;
        post_form(&ctx, "key=SITE_MOTTO&value=move+fast").await;
        assert!(sensitive_flag(&ctx, "SITE_MOTTO").await);
    }

    #[tokio::test]
    async fn form_post_with_an_explicit_zero_is_not_sensitive() {
        let ctx = admin_ctx().await;
        post_form(&ctx, "key=SITE_MOTTO&value=move+fast&sensitive=0").await;
        assert!(!sensitive_flag(&ctx, "SITE_MOTTO").await);
    }

    /// The modal posts a hidden `sensitive=0` followed by the checkbox's
    /// `sensitive=1` when checked; `parse_form_body` keeps the last value,
    /// which is what makes "unchecked" an explicit answer rather than an
    /// absence.
    #[tokio::test]
    async fn form_post_with_the_checkbox_checked_is_sensitive() {
        let ctx = admin_ctx().await;
        post_form(
            &ctx,
            "key=SITE_MOTTO&value=move+fast&sensitive=0&sensitive=1",
        )
        .await;
        assert!(sensitive_flag(&ctx, "SITE_MOTTO").await);
    }

    /// The create modal is checked by default and always posts an explicit
    /// value, so an admin who unchecks it is making a decision the server
    /// can see, and one who does not is protected.
    #[tokio::test]
    async fn create_modal_posts_the_flag_explicitly_and_is_checked_by_default() {
        let ctx = admin_ctx().await;
        let html = settings_body(&ctx, &admin_msg("retrieve", "/admin/settings"))
            .await
            .into_string();
        assert!(
            html.contains(r#"type="hidden" name="sensitive" value="0""#),
            "the modal must post an explicit 0 when the box is unchecked: {html}"
        );
        assert!(
            html.contains(r#"type="checkbox" name="sensitive" value="1" checked"#),
            "the modal's checkbox must be checked by default: {html}"
        );
    }
}
