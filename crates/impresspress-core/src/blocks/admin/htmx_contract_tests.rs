//! Every htmx control an admin page emits, sent the request htmx sends for
//! it, and held to what htmx does with the answer.
//!
//! Two halves of one contract live in different places. The page decides the
//! request: a `<form hx-post>` is submitted as
//! `application/x-www-form-urlencoded` with the fields a browser serializes
//! (no encoding extension ships with the chrome), and a bare `hx-post` /
//! `hx-delete` button sends an empty body. The handler decides the answer,
//! and htmx swaps a 2xx answer into the page unless the control says
//! `hx-swap="none"` — so an answer that is not HTML is inserted into the page
//! as its own source text. Neither half can see the other: a handler that
//! parses JSON only answers every form submit 400, and one that answers JSON
//! replaces the table the operator was looking at with `{"message": …}`.
//! Both have shipped.
//!
//! [`page_link_tests`](super::page_link_tests) proves each control's URL lands
//! on a declared row. This module fires each one. It renders every page in
//! [`PAGES`], finds every element carrying `hx-post`/`hx-put`/`hx-patch`/
//! `hx-delete`, builds the body htmx would send — a form's fields serialized
//! the way a browser serializes them, plus what an operator types into the
//! fields the page leaves empty ([`OPERATOR_INPUT`]) — and dispatches it
//! through the owning block's real `handle()`. The answer must be a 2xx, and
//! HTML wherever it is swapped.
//!
//! A source scan cannot do this job. Half the controls build their URL at
//! render time (`hx-post={"/b/admin/users/" (id) "/disable"}`), so the
//! literal a scan would pair with a route table is not in the source; and the
//! handler a route names delegates its parsing (`ops::*`,
//! `settings_form::save_settings`, `contracts::…::from_form`), so what it
//! accepts is not visible at the dispatch arm either. Rendering the page and
//! posting its bytes reads both halves from the only place they are both
//! true.

use std::collections::BTreeSet;

use wafer_run::{Block as _, InputStream, Message};

use super::{
    page_link_tests::{seeded_ctx, PAGES},
    AdminBlock, ROUTES,
};
use crate::{
    blocks::auth_ui::AuthUiBlock,
    endpoint_match,
    test_support::{admin_msg, anon_msg, output_html, TestContext},
};

/// htmx attribute → the action the request arrives as.
const MUTATING_ATTRS: &[(&str, &str)] = &[
    ("hx-post=\"", "create"),
    ("hx-put=\"", "update"),
    ("hx-patch=\"", "update"),
    ("hx-delete=\"", "delete"),
];

/// What an operator types or picks for a field the page renders empty.
///
/// A form field the page leaves blank is the operator's to fill; posting it
/// blank would test the handler's "required" refusal, not the form. Every
/// entry is a value the page itself would accept. `resource` is the one field
/// no operator types: it is a hidden input the grant modal's script fills
/// from the three selects above it, and this is the value that script
/// produces for "the probe block's `things` table".
const OPERATOR_INPUT: &[(&str, &str)] = &[
    ("name", "probe-typed-name"),
    ("key", "PROBE_TYPED_SETTING"),
    ("grantee", "impresspress/probe"),
    ("resource", "impresspress__probe__things"),
];

/// One element carrying a mutating htmx attribute.
#[derive(Debug)]
pub(super) struct Control {
    /// The action the request arrives as (`create` for `hx-post`, …).
    pub(super) action: &'static str,
    /// The attribute's URL, `&amp;` unescaped.
    pub(super) url: String,
    /// The element's start tag, for failure messages.
    pub(super) tag: String,
    /// `hx-swap`, when the element declares one.
    pub(super) swap: Option<String>,
    /// The form's inner HTML, when the element is a `<form>`.
    pub(super) form: Option<String>,
}

impl Control {
    /// Whether htmx inserts a 2xx answer into the page.
    fn swaps(&self) -> bool {
        self.swap.as_deref() != Some("none")
    }

    /// `(action, element name, field names)` — what identifies this control
    /// across two renders of the same page over two fixtures, whose seeded
    /// ids (and so URLs) differ.
    fn signature(&self) -> String {
        let name = tag_name(&self.tag);
        let fields: Vec<String> = self
            .form
            .as_deref()
            .map(|inner| {
                serialize_form(inner)
                    .into_iter()
                    .map(|field| field.name)
                    .collect()
            })
            .unwrap_or_default();
        format!("{} <{name}> {fields:?}", self.action)
    }
}

/// Every element in `html` that carries a mutating htmx attribute, in
/// document order.
pub(super) fn mutating_controls(html: &str) -> Vec<Control> {
    let mut found: Vec<(usize, Control)> = Vec::new();
    for (attr, action) in MUTATING_ATTRS {
        for (pos, _) in html.match_indices(attr) {
            let start = html[..pos].rfind('<').expect("an attribute sits in a tag");
            let end = pos + html[pos..].find('>').expect("the tag is closed");
            let tag = &html[start..=end];
            let url = attr_value(tag, &attr[..attr.len() - 2]).expect("the attribute has a value");
            let form = (tag_name(tag) == "form").then(|| {
                let inner = &html[end + 1..];
                inner[..inner.find("</form>").expect("the form is closed")].to_string()
            });
            found.push((
                start,
                Control {
                    action,
                    url,
                    tag: tag.to_string(),
                    swap: attr_value(tag, "hx-swap"),
                    form,
                },
            ));
        }
    }
    found.sort_by_key(|(start, _)| *start);
    found.into_iter().map(|(_, control)| control).collect()
}

/// The element name of a start tag (`<form hx-post=…>` → `form`).
fn tag_name(tag: &str) -> &str {
    let name = &tag[1..];
    &name[..name
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(name.len())]
}

/// The unescaped value of `name="…"` in a start tag. Matched only after
/// whitespace, so `name` does not find `data-name`.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let mut from = 0;
    while let Some(i) = tag[from..].find(&needle).map(|i| i + from) {
        if tag[..i].ends_with(char::is_whitespace) {
            let rest = &tag[i + needle.len()..];
            return Some(unescape(&rest[..rest.find('"')?]));
        }
        from = i + needle.len();
    }
    None
}

/// Whether a start tag carries the boolean attribute `name`.
fn has_bool_attr(tag: &str, name: &str) -> bool {
    tag.match_indices(name).any(|(i, _)| {
        tag[..i].ends_with(char::is_whitespace)
            && tag[i + name.len()..]
                .starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/' || c == '=')
    })
}

/// Undo maud's attribute and text escaping.
fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// One field a browser submits.
#[derive(Debug)]
pub(super) struct Field {
    pub(super) name: String,
    pub(super) value: String,
    /// The control carries `required`, so the browser refuses to submit it
    /// empty.
    pub(super) required: bool,
}

/// A form's fields, serialized the way a browser builds its form data set.
///
/// The rules that decide what a submit carries: a `disabled` control is never
/// submitted; a checkbox or radio is submitted only when `checked`, as `on`
/// when it has no value; buttons are not fields; a `<select>` submits its
/// `selected` option, or else its first one that is not disabled, and
/// nothing when the chosen option is itself disabled (the "Select a block…"
/// placeholder); a `<textarea>` submits its text. Document order is kept,
/// since the handlers' `parse_form_body` keeps the last value of a repeated
/// name — the hidden-then-checkbox pattern the variable modals rely on.
pub(super) fn serialize_form(inner: &str) -> Vec<Field> {
    let mut starts: Vec<(usize, &str)> = ["<input", "<textarea", "<select"]
        .iter()
        .flat_map(|open| inner.match_indices(open).map(|(i, _)| (i, *open)))
        .collect();
    starts.sort_unstable();

    let mut fields = Vec::new();
    for (i, open) in starts {
        let tag = &inner[i..=i + inner[i..].find('>').expect("the tag is closed")];
        let Some(name) = attr_value(tag, "name") else {
            continue;
        };
        if has_bool_attr(tag, "disabled") {
            continue;
        }
        let required = has_bool_attr(tag, "required");
        let value = match open {
            "<input" => {
                let kind = attr_value(tag, "type").unwrap_or_else(|| "text".to_string());
                match kind.as_str() {
                    "submit" | "button" | "reset" | "image" | "file" => continue,
                    "checkbox" | "radio" if !has_bool_attr(tag, "checked") => continue,
                    "checkbox" | "radio" => {
                        attr_value(tag, "value").unwrap_or_else(|| "on".to_string())
                    }
                    _ => attr_value(tag, "value").unwrap_or_default(),
                }
            }
            "<textarea" => {
                let body = &inner[i + tag.len()..];
                unescape(&body[..body.find("</textarea>").expect("the textarea is closed")])
            }
            _ => {
                let body = &inner[i + tag.len()..];
                let body = &body[..body.find("</select>").expect("the select is closed")];
                let options: Vec<(&str, &str)> = body
                    .split("<option")
                    .skip(1)
                    .map(|option| {
                        let end = option.find('>').expect("the option tag is closed");
                        let text = &option[end + 1..];
                        (
                            &option[..=end],
                            &text[..text.find("</option>").unwrap_or(text.len())],
                        )
                    })
                    .collect();
                let chosen = options
                    .iter()
                    .find(|(tag, _)| has_bool_attr(tag, "selected"))
                    .or_else(|| {
                        options
                            .iter()
                            .find(|(tag, _)| !has_bool_attr(tag, "disabled"))
                    });
                match chosen {
                    Some((tag, _)) if has_bool_attr(tag, "disabled") => continue,
                    Some((tag, text)) => {
                        attr_value(&format!(" {tag}"), "value").unwrap_or_else(|| unescape(text))
                    }
                    None => continue,
                }
            }
        };
        fields.push(Field {
            name,
            value,
            required,
        });
    }
    fields
}

/// The `application/x-www-form-urlencoded` body htmx sends for `control`:
/// empty for a bare button, the form's fields otherwise — with
/// [`OPERATOR_INPUT`] standing in for what the operator fills in.
///
/// Panics on a required field the page leaves empty and the table does not
/// name: a new form should say what an operator puts there, rather than this
/// guard posting it blank and testing the refusal instead of the form.
pub(super) fn request_body(control: &Control) -> Vec<u8> {
    let Some(inner) = control.form.as_deref() else {
        return Vec::new();
    };
    let mut fields = serialize_form(inner);
    for (name, typed) in OPERATOR_INPUT {
        let mut named = fields.iter_mut().filter(|f| f.name == *name).peekable();
        if named.peek().is_none() && inner.contains(&format!(" name=\"{name}\"")) {
            // A field the browser does not submit as rendered — the disabled
            // placeholder option of a select — is submitted once the
            // operator picks something.
            fields.push(Field {
                name: name.to_string(),
                value: typed.to_string(),
                required: true,
            });
            continue;
        }
        for field in named {
            if field.value.is_empty() {
                field.value = typed.to_string();
            }
        }
    }
    if let Some(blank) = fields.iter().find(|f| f.required && f.value.is_empty()) {
        panic!(
            "{} leaves the required field `{}` empty; add what an operator types into it \
             to OPERATOR_INPUT",
            control.tag, blank.name
        );
    }
    let mut body = url::form_urlencoded::Serializer::new(String::new());
    for field in &fields {
        body.append_pair(&field.name, &field.value);
    }
    body.finish().into_bytes()
}

/// A response, rendered the way the HTTP boundary renders it.
#[derive(Debug)]
pub(super) struct Answer {
    pub(super) status: u16,
    pub(super) content_type: String,
    pub(super) body: String,
}

/// Send `control`'s request the way htmx sends it — its action, its URL's
/// query string, `HX-Request: true`, a form-encoded body — through the real
/// `handle()` of the block that owns the path, as the site admin.
pub(super) async fn fire(ctx: &TestContext, control: &Control) -> Answer {
    let (path, query) = control
        .url
        .split_once('?')
        .unwrap_or((control.url.as_str(), ""));
    let mut msg: Message = admin_msg(control.action, path);
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        msg.set_meta(format!("req.query.{name}"), value.as_ref());
    }
    msg.set_meta("http.header.hx-request", "true");
    // htmx sets this on every non-GET request it makes, body or not.
    for key in [
        "http.header.content-type",
        "http.content_type",
        "req.content_type",
    ] {
        msg.set_meta(key, "application/x-www-form-urlencoded");
    }
    let input = InputStream::from_bytes(request_body(control));
    let out = if path.starts_with("/b/admin/") {
        AdminBlock::new().handle(ctx, msg, input).await
    } else if path.starts_with("/b/auth/") {
        AuthUiBlock::new().handle(ctx, msg, input).await
    } else {
        panic!(
            "{} targets {path}: not an admin or auth-ui path",
            control.tag
        );
    };
    let parts = wafer_block::http_codec::collect_http_response(out).await;
    Answer {
        status: parts.status,
        content_type: parts
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.clone())
            .unwrap_or_default(),
        body: String::from_utf8_lossy(&parts.body).into_owned(),
    }
}

/// Render one of [`PAGES`] as the admin.
async fn render(ctx: &TestContext, (action, path, query): &super::page_link_tests::Page) -> String {
    let mut msg = admin_msg(action, path);
    for (name, value) in *query {
        msg.set_meta(format!("req.query.{name}"), *value);
    }
    output_html(
        AdminBlock::new()
            .handle(ctx, msg, InputStream::empty())
            .await,
    )
    .await
}

/// The control's row, named the way `served_paths` names it, or the auth-ui
/// request it makes — what the coverage check below counts.
fn row_name(control: &Control) -> String {
    let path = control.url.split('?').next().unwrap_or_default();
    if path.starts_with("/b/admin/") {
        let route = endpoint_match::dispatch(&mut anon_msg(control.action, path), ROUTES)
            .unwrap_or_else(|| panic!("{} targets no admin row", control.tag));
        format!("{route:?}")
    } else {
        format!("auth-ui {} {path}", control.action)
    }
}

/// The guard. Each control is fired against its own fresh fixture — every one
/// is a mutation, and one that deleted the seeded user or role would change
/// what the next one meets — which means rendering its page again there and
/// finding it by [`Control::signature`], since the fixture's ids are new.
#[tokio::test]
async fn every_htmx_control_an_admin_page_emits_is_answered_the_way_it_swaps() {
    let mut fired: BTreeSet<String> = BTreeSet::new();
    for page in PAGES {
        let (ctx, _) = seeded_ctx().await;
        let signatures: Vec<String> = mutating_controls(&render(&ctx, page).await)
            .iter()
            .map(Control::signature)
            .collect();
        for (index, signature) in signatures.iter().enumerate() {
            let (ctx, _) = seeded_ctx().await;
            let controls = mutating_controls(&render(&ctx, page).await);
            let control = &controls[index];
            assert_eq!(
                &control.signature(),
                signature,
                "{page:?} rendered its controls in a different order over a second fixture"
            );

            let answer = fire(&ctx, control).await;
            assert!(
                (200..300).contains(&answer.status),
                "{page:?} emits {}, whose request is answered {}: {}",
                control.tag,
                answer.status,
                answer.body
            );
            if control.swaps() {
                assert!(
                    answer.content_type.starts_with("text/html"),
                    "{page:?} emits {}, which swaps its answer into the page, and the \
                     answer is {} — htmx inserts it as text: {}",
                    control.tag,
                    answer.content_type,
                    answer.body
                );
            }
            fired.insert(row_name(control));
        }
    }

    // The guard is only as good as what the pages rendered. Each of these
    // carries a request body or swaps its answer; a page that stops
    // rendering one must not let the guard pass by firing less.
    for expected in [
        "UserDisable",
        "UserDelete",
        "CreateRole",
        "DeleteRole",
        "RevokeApiKey",
        "auth-ui create /b/auth/api/api-keys",
        "BlockToggle",
        "DatabaseQuery",
        "CreateVariable",
        "UpdateVariable",
        "DeleteVariable",
        "ResetVariableToEnvironment",
        "ResetVariablesPinnedAtUpgrade",
        "CreateWrapGrant",
        "DeleteWrapGrant",
    ] {
        assert!(
            fired.contains(expected),
            "no page fired {expected}; fired: {fired:#?}"
        );
    }
}

/// The serializer the guard's bodies come from submits what a browser
/// submits, and nothing a browser leaves out.
#[test]
fn a_form_serializes_the_way_a_browser_submits_it() {
    let inner = r#"
        <input type="text" name="key" required>
        <input type="text" name="value" value="a &amp; b">
        <input type="text" name="locked" value="x" disabled>
        <input type="hidden" name="sensitive" value="0">
        <input type="checkbox" name="sensitive" value="1" checked>
        <input type="checkbox" name="write" value="on">
        <input type="checkbox" name="bare" checked>
        <select name="grantee" required><option value="" disabled selected>Pick</option><option value="*">All</option></select>
        <select name="kind"><option value="db">DB</option><option value="config" selected>Config</option></select>
        <select name="first"><option>Plain text</option></select>
        <textarea name="query">SELECT 1 &lt; 2;</textarea>
        <button type="submit" name="go">Go</button>
    "#;
    let fields: Vec<(String, String, bool)> = serialize_form(inner)
        .into_iter()
        .map(|f| (f.name, f.value, f.required))
        .collect();
    let expect =
        |name: &str, value: &str, required: bool| (name.to_string(), value.to_string(), required);
    assert_eq!(
        fields,
        vec![
            expect("key", "", true),
            expect("value", "a & b", false),
            expect("sensitive", "0", false),
            expect("sensitive", "1", false),
            expect("bare", "on", false),
            expect("kind", "config", false),
            expect("first", "Plain text", false),
            expect("query", "SELECT 1 < 2;", false),
        ]
    );
}

/// The API-keys tab's Revoke button swaps its answer into
/// `#users-tab-content`. It posted to auth-ui's
/// `PATCH /b/auth/api/api-keys/{id}`, which answers
/// `{"message": "API key revoked"}` — so revoking a key replaced the key table
/// with that JSON as text. Its answer is now the tab, with the key revoked.
#[tokio::test]
async fn revoking_an_api_key_from_the_tab_answers_with_the_tab() {
    use crate::blocks::auth::repo::api_keys;

    let (ctx, seeds) = seeded_ctx().await;
    let html = render(
        &ctx,
        &("retrieve", "/b/admin/users", &[("tab", "api-keys")]),
    )
    .await;
    let controls = mutating_controls(&html);
    let revoke = controls
        .iter()
        .find(|c| c.tag.contains("Revoke this API key?"))
        .expect("the tab renders a Revoke button for the seeded key");

    let answer = fire(&ctx, revoke).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert!(
        answer.content_type.starts_with("text/html"),
        "the answer is swapped into the tab, so it must be HTML; got {}: {}",
        answer.content_type,
        answer.body
    );
    assert!(
        answer.body.contains("ipk_abc"),
        "the answer must be the key table, with the seeded key in it: {}",
        answer.body
    );
    assert!(
        !answer.body.contains("Revoke this API key?"),
        "the re-rendered tab must show the key revoked, with no Revoke button: {}",
        answer.body
    );
    let key = api_keys::find_by_id(&ctx, &seeds.key_id)
        .await
        .expect("read the key")
        .expect("the key row");
    assert!(key.revoked_at.is_some(), "the key must really be revoked");
}

/// The two forms a review reported as posting a form body to a JSON-only
/// handler: the roles tab's Create Role modal and the SQL tab's editor. Both
/// target the block's form routes (`POST /b/admin/iam/roles`,
/// `POST /b/admin/database/query`), not the JSON API rows the report paired
/// them with, and both routes parse form bodies. This pins that with the
/// page's own bytes, and pins what each answer carries — a guard, passing
/// before and after this change by design.
#[tokio::test]
async fn the_create_role_and_sql_forms_post_to_routes_that_read_their_bytes() {
    let (ctx, _) = seeded_ctx().await;

    let html = render(&ctx, &("retrieve", "/b/admin/users", &[("tab", "roles")])).await;
    let create_role = mutating_controls(&html)
        .into_iter()
        .find(|c| c.url == "/b/admin/iam/roles" && c.form.is_some())
        .expect("the roles tab renders the Create Role form");
    assert_eq!(
        String::from_utf8(request_body(&create_role)).expect("utf-8"),
        "name=probe-typed-name&description=",
        "the form posts the fields the page names"
    );
    let answer = fire(&ctx, &create_role).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert!(answer.content_type.starts_with("text/html"), "{answer:?}");
    assert!(
        answer.body.contains("probe-typed-name"),
        "the answer is the roles tab with the new role in it: {}",
        answer.body
    );

    let html = render(&ctx, &("retrieve", "/b/admin/database", &[("tab", "sql")])).await;
    let query = mutating_controls(&html)
        .into_iter()
        .find(|c| c.url == "/b/admin/database/query")
        .expect("the SQL tab renders the editor form");
    assert_eq!(
        String::from_utf8(request_body(&query)).expect("utf-8"),
        "query=SELECT+1%3B",
        "the editor posts its textarea, form-encoded"
    );
    let answer = fire(&ctx, &query).await;
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert!(answer.content_type.starts_with("text/html"), "{answer:?}");
    assert!(
        answer.body.contains("1 row") || answer.body.contains("<table"),
        "the answer is the result grid for the query the form carried: {}",
        answer.body
    );
}
