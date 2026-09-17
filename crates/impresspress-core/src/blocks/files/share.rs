use wafer_core::clients::{crypto, storage as store};
use wafer_run::{context::Context, Message, OutputStream};

use super::repo;
use crate::{
    blocks::{
        crud,
        rate_limit::{check_rate_limit, RateLimit, RateLimitOutcome, UserRateLimiter},
    },
    http::{err_forbidden, err_internal, err_internal_no_cause},
    util::hex_encode,
};

/// Bytes of entropy in a share token. 256 bits of CSPRNG output is what
/// makes the token unguessable, which is the whole of its secrecy: it is a
/// bearer credential naming one row, not a signed assertion about it.
const SHARE_TOKEN_BYTES: usize = 32;

/// Mint the opaque token that addresses a share link.
///
/// The token says nothing — not the bucket, not the key, not a lifetime.
/// The share row it selects carries the expiry and the access cap, and
/// [`handle_direct_access`] enforces both from that row, so a link lives
/// exactly as long as the share its owner created and there is no second
/// clock to disagree with it.
pub async fn generate_share_token(ctx: &dyn Context) -> Result<String, OutputStream> {
    crypto::random_bytes(ctx, SHARE_TOKEN_BYTES)
        .await
        .map(|bytes| hex_encode(&bytes))
        .map_err(|e| err_internal("Token generation failed", e))
}

pub async fn handle_direct_access(
    ctx: &dyn Context,
    msg: &Message,
    limiter: &UserRateLimiter,
) -> OutputStream {
    // `{token}` in `GET /b/storage/direct/{token}`, as the block's route
    // table bound it.
    let token = match crud::path_var(msg, "token", "Missing share token") {
        Ok(value) => value,
        Err(response) => return response,
    };

    // Rate-limit per remote IP before doing any work — `/storage/direct/*` is
    // public (no auth required) so without this an attacker can enumerate
    // valid tokens / amplify DOS by issuing many lookups. Identity key falls
    // back to "unknown" if the platform layer can't expose a remote IP.
    let identity = {
        let addr = msg.remote_addr();
        if addr.is_empty() {
            "unknown".to_string()
        } else {
            addr.to_string()
        }
    };
    match check_rate_limit(limiter, ctx, &identity, "share_direct", RateLimit::API_READ).await {
        RateLimitOutcome::Limited(r) => return r,
        // Allowed headers can't be attached to a binary file response here —
        // accept this as a known limitation; the platform layer would need
        // streaming-meta middleware to inject them.
        RateLimitOutcome::Allowed(_) | RateLimitOutcome::Disabled => {}
    }

    // The token is the whole credential: 256 random bits addressing one row,
    // so a lookup miss is the only "wrong token" there is. A lookup that
    // FAILED is not a miss — an outage on the shares table is a 500, not a
    // 404 telling the recipient their link was revoked.
    let share = match repo::shares::find_by_token(ctx, token).await {
        Ok(share) => share,
        Err(e) => return crud::db_error(e, "Share not found or expired", "Share lookup failed"),
    };

    // Check expiry against the row, which is the only place a share's
    // lifetime is recorded. `ShareRow::expires_at` is `None` for a share
    // that never expires (a SQL NULL or a stored empty string, which mean
    // the same thing here). A stored expiry we cannot parse is refused, not
    // waved through: the owner set an expiry, and an unreadable one cannot
    // be shown to be in the future.
    if let Some(expires) = share.expires_at.as_deref() {
        let Ok(exp_time) = chrono::DateTime::parse_from_rfc3339(expires) else {
            tracing::error!(
                share_id = %share.id,
                expires_at = %expires,
                "share row carries an unparseable expires_at",
            );
            return err_forbidden("Share link has expired");
        };
        if exp_time < chrono::Utc::now() {
            return err_forbidden("Share link has expired");
        }
    }

    let bucket = share.bucket.as_str();
    let key = share.key.as_str();

    if bucket.is_empty() || key.is_empty() {
        return err_internal_no_cause("Invalid share data");
    }

    // Resolve the object BEFORE spending an access. `get_stream` resolves the
    // `ObjectInfo` header eagerly, so a share whose object is missing fails
    // here — without burning one of a capped share's accesses on a request
    // that serves nothing.
    let stream = match store::get_stream(ctx, bucket, key).await {
        Ok(stream) => stream,
        Err(e) => return crud::db_error(e, "File not found", "Storage error"),
    };

    // Spend the access. The cap lives inside the UPDATE's WHERE clause:
    //   UPDATE shares SET access_count = access_count + 1
    //   WHERE id = ? AND access_count < max_access_count
    // so at most one updater wins per row and rowcount 0 ⇒ cap reached; two
    // concurrent accesses cannot both pass a `max_access_count = 1` share.
    // `None` (no cap) reaches `increment_access_count_capped` as 0, which is
    // the "unlimited" sentinel its filter is written against.
    //
    // This statement IS the cap check, so a failure refuses the download:
    // serving anyway would serve past the cap, and there is no earlier check
    // to fall back on.
    let max = share.max_access_count.unwrap_or(0);
    match repo::shares::increment_access_count_capped(ctx, &share.id, max).await {
        Ok(true) => {}
        Ok(false) => return err_forbidden("Share link access limit reached"),
        Err(e) => return err_internal("Share access accounting failed", e),
    }

    // The audit trail, unlike the counter, is not load-bearing for the cap:
    // a lost log row is logged and the download proceeds.
    if let Err(e) =
        repo::shares::log_access(ctx, &share.id, msg.remote_addr(), msg.header("User-Agent")).await
    {
        tracing::warn!("Failed to log share access: {e}");
    }

    // Serve the file — stream the body straight from storage (R2
    // `get_streaming` on CF) rather than buffering the whole object in the
    // isolate. The leading meta carries the streaming opt-in marker +
    // content-type + download headers so the pipeline and platform adapter
    // take the streaming response path (see `crate::streaming`).
    //
    // This route is unauthenticated and its bytes are whatever an uploader
    // chose, so the headers come from [`super::serving`] — the same builder
    // the authenticated download uses. That is what keeps an uploaded page
    // from executing on this origin when its owner sends someone the link.
    //
    // No local fallback for a backend that reports no type: the empty string
    // is not a media type, so `serving` substitutes `application/octet-stream`
    // for it exactly as it does for a type it cannot read.
    let leading = super::serving::user_object_leading_meta(
        &stream.info().content_type.clone(),
        key,
        &[("Cache-Control", "private, max-age=3600")],
    );
    crate::streaming::stream_download(stream, leading)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;
    use wafer_core::clients::storage as store;

    use super::{super::test_support::share_ctx, *};
    use crate::{
        blocks::rate_limit::UserRateLimiter,
        test_support::{anon_msg, output_is_error, FailingDbOpContext, TestContext},
    };

    /// A token as the block mints them: opaque hex, asserting nothing about
    /// the share it addresses.
    const OPAQUE_TOKEN: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    /// Seed one share row verbatim, so a test controls the token, the expiry
    /// and the cap the handler will read.
    async fn seed_share(ctx: &TestContext, fields: &[(&str, serde_json::Value)]) -> String {
        let mut data: HashMap<String, serde_json::Value> = HashMap::new();
        data.insert("token".into(), json!(OPAQUE_TOKEN));
        data.insert("bucket".into(), json!("photos"));
        data.insert("key".into(), json!("a.png"));
        data.insert("created_by".into(), json!("alice"));
        data.insert("created_at".into(), json!(crate::util::now_rfc3339()));
        data.insert("access_count".into(), json!(0));
        for (k, v) in fields {
            data.insert((*k).to_string(), v.clone());
        }
        repo::shares::seed(ctx, data).await.expect("seed share").id
    }

    /// `GET /b/storage/direct/{token}` as the public link is fetched.
    async fn direct_access(ctx: &dyn Context, token: &str) -> OutputStream {
        let mut msg = anon_msg("retrieve", &format!("/b/storage/direct/{token}"));
        msg.set_meta("req.param.token", token);
        handle_direct_access(ctx, &msg, &UserRateLimiter::default()).await
    }

    /// Drain a served response into the bytes the recipient receives.
    async fn served_bytes(out: OutputStream) -> Vec<u8> {
        let mut body = Vec::new();
        let mut events = out;
        while let Some(evt) = futures::StreamExt::next(&mut events).await {
            match evt {
                wafer_block::stream::StreamEvent::Chunk(bytes) => body.extend_from_slice(&bytes),
                wafer_block::stream::StreamEvent::Error(e) => {
                    panic!("the share link errored instead of serving: {}", e.message)
                }
                _ => {}
            }
        }
        body
    }

    /// The share row is the only clock on a link.
    ///
    /// The token addresses a row and says nothing else; a row whose
    /// `expires_at` is a year out is served, whatever any credential-side
    /// lifetime might once have claimed. This is the day-31 failure of a
    /// year-long share: the row reads active and the link says "not found".
    #[tokio::test]
    async fn a_live_share_row_is_served_whatever_its_token_claims() {
        let ctx = share_ctx("photos", "alice").await;
        let stored: &[u8] = b"PNG\x89bytes-that-must-come-back";
        store::put(&ctx, "photos", "a.png", stored, "image/png")
            .await
            .expect("seed the object being shared");
        let a_year_out = (chrono::Utc::now() + chrono::Duration::days(365)).to_rfc3339();
        seed_share(&ctx, &[("expires_at", json!(a_year_out))]).await;

        let body = served_bytes(direct_access(&ctx, OPAQUE_TOKEN).await).await;

        assert_eq!(
            body, stored,
            "a share whose row is live must serve, however old the link is"
        );
    }

    /// An expiry that cannot be read is not an absent expiry.
    ///
    /// The owner set one; a value the handler cannot parse cannot be shown
    /// to be in the future, so the link is refused rather than served
    /// forever.
    #[tokio::test]
    async fn an_unreadable_expiry_is_not_a_share_that_never_expires() {
        let ctx = share_ctx("photos", "alice").await;
        store::put(&ctx, "photos", "a.png", b"secret", "image/png")
            .await
            .expect("seed the object being shared");
        seed_share(&ctx, &[("expires_at", json!("next tuesday"))]).await;

        assert!(
            output_is_error(direct_access(&ctx, OPAQUE_TOKEN).await, "PermissionDenied").await,
            "an expiry the handler cannot parse must refuse the link, not serve it forever"
        );
    }

    /// A share lookup that FAILED is not a share that does not exist.
    ///
    /// Answering 404 during an outage tells the recipient their link was
    /// revoked, and tells the owner nothing.
    #[tokio::test]
    async fn a_share_lookup_outage_is_not_a_missing_share() {
        let ctx = share_ctx("photos", "alice").await;
        store::put(&ctx, "photos", "a.png", b"bytes", "image/png")
            .await
            .expect("seed the object being shared");
        seed_share(&ctx, &[]).await;
        let failing = FailingDbOpContext::new(ctx, vec![("database.list", repo::shares::TABLE)]);

        assert!(
            output_is_error(direct_access(&failing, OPAQUE_TOKEN).await, "Internal").await,
            "an outage on the shares table must be a 500, not `Share not found`"
        );
    }

    /// A request that serves nothing must not spend one of a capped share's
    /// accesses.
    ///
    /// The share below allows exactly one access and its object is missing.
    /// Once the object is there, that one access must still be available.
    #[tokio::test]
    async fn a_request_that_serves_nothing_does_not_spend_an_access() {
        let ctx = share_ctx("photos", "alice").await;
        let id = seed_share(&ctx, &[("max_access_count", json!(1))]).await;

        let out = direct_access(&ctx, OPAQUE_TOKEN).await;
        assert!(
            output_is_error(out, "NotFound").await,
            "a share whose object is gone must fail"
        );
        assert_eq!(
            repo::shares::find_by_id(&ctx, &id)
                .await
                .expect("share row")
                .access_count,
            0,
            "nothing was served, so no access was spent"
        );

        let stored: &[u8] = b"the-bytes";
        store::put(&ctx, "photos", "a.png", stored, "image/png")
            .await
            .expect("store the object the share points at");
        let body = served_bytes(direct_access(&ctx, OPAQUE_TOKEN).await).await;
        assert_eq!(
            body, stored,
            "the one permitted access must still be available"
        );
    }

    /// An access that cannot be recorded is not served.
    ///
    /// The increment IS the cap check — it is the statement that refuses the
    /// access past `max_access_count` — so serving when it fails serves past
    /// the cap, and does it silently.
    #[tokio::test]
    async fn an_access_that_cannot_be_recorded_is_not_served() {
        let ctx = share_ctx("photos", "alice").await;
        store::put(&ctx, "photos", "a.png", b"paid-for-bytes", "image/png")
            .await
            .expect("seed the object being shared");
        seed_share(&ctx, &[("max_access_count", json!(1))]).await;
        let failing = FailingDbOpContext::new(
            ctx,
            vec![("database.increment_field_where", repo::shares::TABLE)],
        );

        assert!(
            output_is_error(direct_access(&failing, OPAQUE_TOKEN).await, "Internal").await,
            "an unrecordable access must refuse the download, not serve past the cap"
        );
    }
}
