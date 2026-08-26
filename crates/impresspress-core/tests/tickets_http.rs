#![cfg(feature = "block-tickets")]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use impresspress_core::{
    blocks::tickets::{abuse, config, models::TicketTypeInput, repo, service, TicketsBlock},
    test_support::{anon_msg, collect_or_panic, TestContext},
};
use serde_json::{json, Value};
use wafer_core::{
    clients::database as db,
    interfaces::network::service::{NetworkError, NetworkService, Request, Response},
};
use wafer_run::{Block, InputStream, Message};

#[derive(Clone)]
struct TurnstileNetwork {
    hostname: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

#[async_trait]
impl NetworkService for TurnstileNetwork {
    async fn do_request(&self, request: &Request) -> Result<Response, NetworkError> {
        self.requests.lock().unwrap().push(request.clone());
        Ok(Response {
            status_code: 200,
            headers: Default::default(),
            body: serde_json::to_vec(&json!({
                "success": true,
                "action": "ticket_submit",
                "hostname": self.hostname,
            }))
            .unwrap(),
        })
    }
}

struct HttpResponse {
    status: u16,
    headers: std::collections::HashMap<String, String>,
    body: Vec<u8>,
}

async fn call(
    block: &TicketsBlock,
    ctx: &dyn wafer_run::context::Context,
    msg: Message,
    body: Vec<u8>,
) -> HttpResponse {
    let response =
        collect_or_panic(block.handle(ctx, msg, InputStream::from_bytes(body)).await).await;
    let mut status = 200;
    let mut headers = std::collections::HashMap::new();
    for meta in response.meta {
        if meta.key == "resp.status" {
            status = meta.value.parse().unwrap_or(200);
        } else if let Some(name) = meta.key.strip_prefix("resp.header.") {
            headers.insert(name.to_ascii_lowercase(), meta.value);
        }
    }
    HttpResponse {
        status,
        headers,
        body: response.body,
    }
}

fn public_post(body: &Value) -> (Message, Vec<u8>) {
    let bytes = serde_json::to_vec(body).unwrap();
    let mut msg = anon_msg("create", "/b/tickets/api/submissions");
    msg.set_meta("http.header.content-type", "application/json");
    msg.set_meta("http.header.accept", "application/json");
    msg.set_meta("http.header.host", "example.test");
    msg.set_meta("http.header.origin", "https://example.test");
    msg.set_meta("http.header.sec-fetch-site", "same-origin");
    msg.set_meta("http.header.content-length", bytes.len().to_string());
    msg.set_meta("req.client.ip", "203.0.113.42");
    (msg, bytes)
}

async fn ready_context(
    hostname: &str,
    title: &str,
) -> (TestContext, db::Record, Arc<Mutex<Vec<Request>>>) {
    let mut ctx = TestContext::with_tickets().await;
    for (key, value) in [
        (config::PUBLIC_ENABLED, "true"),
        (config::TURNSTILE_SITE_KEY, "site-key"),
        (config::TURNSTILE_SECRET_KEY, "turnstile-secret"),
        (config::IDENTITY_SECRET, "independent-identity-secret"),
        (config::IDENTITY_MAX, "3"),
        (config::IDENTITY_WINDOW, "3600"),
        (config::GLOBAL_MAX, "100"),
        (config::GLOBAL_WINDOW, "3600"),
    ] {
        ctx.set_config(key, value);
    }
    let kind = service::create_type(
        &ctx,
        TicketTypeInput {
            key: "incorrect-information".into(),
            title: title.into(),
            description: "Report information that appears wrong.".into(),
            guidance: "Include the page and reliable supporting evidence.".into(),
            default_priority: "normal".into(),
            escalation_kind: "none".into(),
            public_visible: true,
            requires_contact: false,
            requests_evidence: true,
            active: true,
            sort_order: 10,
        },
    )
    .await
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let network: Arc<dyn Block> = Arc::new(wafer_core::service_blocks::network::NetworkBlock::new(
        Arc::new(TurnstileNetwork {
            hostname: hostname.into(),
            requests: requests.clone(),
        }),
    ));
    ctx.register_block("wafer-run/network", network);
    (ctx, kind, requests)
}

fn valid_submission(type_id: &str) -> Value {
    json!({
        "type_id": type_id,
        "subject": "Opening hours appear to be incorrect",
        "description": "The listing says 8am but the official venue page currently says 9am.",
        "source_path": "/activities/example",
        "subject_type": "activity",
        "subject_id": "example-1",
        "evidence_url": "https://venue.example/hours",
        "reporter_email": "",
        "reporter_wants_reply": false,
        "form_token": abuse::issue_form_token(
            "independent-identity-secret",
            abuse::now_secs(),
        ),
        "turnstile_token": "single-use-test-token",
        "website": "",
    })
}

#[tokio::test]
async fn public_submission_is_protected_deduplicated_and_contains_no_network_identity() {
    let (ctx, kind, requests) = ready_context("example.test", "Incorrect information").await;
    let block = TicketsBlock::new();
    let payload = valid_submission(&kind.id);

    let (msg, body) = public_post(&payload);
    let first = call(&block, &ctx, msg, body).await;
    assert_eq!(
        first.status,
        201,
        "{}",
        String::from_utf8_lossy(&first.body)
    );
    assert_eq!(
        first.headers.get("cache-control").map(String::as_str),
        Some("no-store")
    );
    assert_eq!(
        first.headers.get("x-robots-tag").map(String::as_str),
        Some("noindex, nofollow")
    );
    let first_json: Value = serde_json::from_slice(&first.body).unwrap();

    let (msg, body) = public_post(&payload);
    let duplicate = call(&block, &ctx, msg, body).await;
    assert_eq!(duplicate.status, 201);
    let duplicate_json: Value = serde_json::from_slice(&duplicate.body).unwrap();
    assert_eq!(duplicate_json["reference"], first_json["reference"]);
    assert_eq!(db::count(&ctx, repo::TICKETS, &[]).await.unwrap(), 1);
    assert_eq!(db::count(&ctx, repo::EVENTS, &[]).await.unwrap(), 1);

    let ticket = db::list_all(&ctx, repo::TICKETS, vec![])
        .await
        .unwrap()
        .pop()
        .unwrap();
    let stored = serde_json::to_string(&ticket).unwrap();
    assert!(!stored.contains("203.0.113.42"));
    assert!(!stored.contains("single-use-test-token"));
    assert!(!stored.contains("turnstile-secret"));

    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let turnstile_body = String::from_utf8_lossy(requests[0].body.as_deref().unwrap_or_default());
    assert!(turnstile_body.contains("response=single-use-test-token"));
    assert!(turnstile_body.contains("remoteip=203.0.113.42"));
}

#[tokio::test]
async fn form_escapes_type_copy_and_is_unavailable_without_readiness() {
    let default_ctx = TestContext::with_tickets().await;
    let block = TicketsBlock::new();
    let mut msg = anon_msg("retrieve", "/b/tickets/submit");
    msg.set_meta("http.header.accept", "text/html");
    let unavailable = call(&block, &default_ctx, msg, Vec::new()).await;
    assert_eq!(unavailable.status, 200);
    let unavailable_html = String::from_utf8(unavailable.body).unwrap();
    assert!(unavailable_html.contains("Reporting is temporarily unavailable"));
    assert!(!unavailable_html.contains("challenges.cloudflare.com/turnstile"));

    let (ctx, _, _) = ready_context("example.test", "<script>alert(1)</script>").await;
    service::create_type(
        &ctx,
        TicketTypeInput {
            key: "legal-contact".into(),
            title: "Copyright or trademark concern".into(),
            description: "Report a rights concern.".into(),
            guidance: "Include the affected material and your relationship to it.".into(),
            default_priority: "urgent".into(),
            escalation_kind: "legal".into(),
            public_visible: true,
            requires_contact: true,
            requests_evidence: true,
            active: true,
            sort_order: 20,
        },
    )
    .await
    .unwrap();
    let mut msg = anon_msg("retrieve", "/b/tickets/submit");
    msg.set_meta("http.header.accept", "text/html");
    let ready = call(&block, &ctx, msg, Vec::new()).await;
    let html = String::from_utf8(ready.body).unwrap();
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!html.contains("<script>alert(1)</script>"));
    assert!(html.contains("challenges.cloudflare.com/turnstile/v0/api.js"));
    assert!(html.contains(r#"data-requires-contact="true""#));
    assert!(html.contains("email.required = requiresContact"));
    assert!(html.contains("consent.required = requiresContact"));
    assert!(html.contains("Evidence URL (recommended)"));
    assert!(!html.contains("turnstile-secret"));
    assert!(!html.contains("independent-identity-secret"));
}

#[tokio::test]
async fn origin_body_limit_and_turnstile_hostname_fail_closed_without_writes() {
    let (ctx, kind, requests) = ready_context("wrong.example", "Incorrect information").await;
    let block = TicketsBlock::new();
    let payload = valid_submission(&kind.id);

    let (mut cross_site, body) = public_post(&payload);
    cross_site.set_meta("http.header.sec-fetch-site", "cross-site");
    assert_eq!(call(&block, &ctx, cross_site, body).await.status, 403);
    assert!(requests.lock().unwrap().is_empty());

    let (mut oversized, _) = public_post(&payload);
    oversized.set_meta("http.header.content-length", (16 * 1024 + 1).to_string());
    assert_eq!(
        call(&block, &ctx, oversized, b"{}".to_vec()).await.status,
        413
    );
    assert!(requests.lock().unwrap().is_empty());

    let (msg, body) = public_post(&payload);
    assert_eq!(call(&block, &ctx, msg, body).await.status, 400);
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert_eq!(db::count(&ctx, repo::TICKETS, &[]).await.unwrap(), 0);
}
