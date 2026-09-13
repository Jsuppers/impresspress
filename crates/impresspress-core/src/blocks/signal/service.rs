//! The room store: one row per WebRTC handshake, keyed by a six-character
//! code, holding the host's offer and then the guest's answer.
//!
//! Timestamps are ISO-8601 `%Y-%m-%dT%H:%M:%SZ`, which sorts lexically, which
//! is why the expiry comparison below is a string compare — the same
//! convention `auth/repo/oauth_pkce.rs` uses for its own `expires_at` check.
//! No `exec_raw`/`query_raw`: every read and write goes through the typed
//! `wafer_core::clients::database` client, using the `_count` form of update
//! (`db::update_by_filters_count`, not bare `update_by_filters`) so a
//! filtered write with no PK in hand can tell "updated one row" from
//! "matched nothing" — the same reason every other filtered-update call site
//! in this codebase (`products/repo/{offers,refunds,seller_accounts,
//! purchases}.rs`) uses the `_count` form.

use std::collections::HashMap;

use serde_json::{json, Value};
use wafer_block::db::{Filter, FilterOp};
use wafer_core::clients::database as db;
use wafer_run::{context::Context, ConfigVar, InputType};

pub const TABLE: &str = "impresspress__signal__rooms";

/// Characters a room code is drawn from, and how many of them there are.
/// The pairs that are read wrong out loud or off a screen are absent: no
/// 0/O, no 1/I/L, no U. Mirrored by the client's own generator (spec §18.3).
pub const CODE_ALPHABET: &str = "23456789ABCDEFGHJKMNPQRSTVWXYZ";
pub const CODE_LEN: usize = 6;

/// Config keys, all block-scoped (`{ORG}__{BLOCK}__*`).
pub const TTL_KEY: &str = "IMPRESSPRESS__SIGNAL__ROOM_TTL_SECONDS";
pub const MAX_SDP_KEY: &str = "IMPRESSPRESS__SIGNAL__MAX_SDP_BYTES";
pub const STUN_KEY: &str = "IMPRESSPRESS__SIGNAL__STUN_URLS";
pub const DEFAULT_TTL_SECONDS: i64 = 600;
pub const DEFAULT_MAX_SDP_BYTES: usize = 16_384;
pub const DEFAULT_STUN_URLS: &str = "stun:stun.l.google.com:19302";

/// The three config vars this block reads, for `BlockInfo::config_keys` —
/// so they show up on the admin Variables screen (the block has no admin
/// page of its own to render them on).
pub fn config_vars() -> Vec<ConfigVar> {
    vec![
        ConfigVar::new(
            TTL_KEY,
            "How long a signalling room stays open before it expires and its \
             code is free again",
            &DEFAULT_TTL_SECONDS.to_string(),
        )
        .name("Room TTL (seconds)")
        .input_type(InputType::Number),
        ConfigVar::new(
            MAX_SDP_KEY,
            "Largest SDP blob (offer or answer) a room is allowed to hold",
            &DEFAULT_MAX_SDP_BYTES.to_string(),
        )
        .name("Max SDP size (bytes)")
        .input_type(InputType::Number),
        ConfigVar::new(
            STUN_KEY,
            "Comma-separated STUN server URLs handed to the client's \
             RTCPeerConnection — never compiled into the game itself",
            DEFAULT_STUN_URLS,
        )
        .name("STUN server URLs"),
    ]
}

#[derive(Debug)]
pub enum RoomError {
    /// A live room already holds this code. The host rolls another.
    Taken,
    /// No such room, or it expired (and has been deleted on the way out).
    Gone,
    /// An answer already stands — a third browser may not take a paired room.
    Answered,
    BadCode,
    TooBig,
    Db(String),
}

fn db_err(e: wafer_run::WaferError) -> RoomError {
    RoomError::Db(e.message)
}

/// Current UTC time as an ISO-8601 string with a literal `Z` suffix
/// (`%Y-%m-%dT%H:%M:%SZ`). Sorts lexically, which is what makes the
/// `expires_at` string-compare below correct.
fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `now + secs` in the same format as [`now_iso`]. `secs` may be negative —
/// tests use that to manufacture an already-expired row.
fn iso_plus_seconds(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn map_str(m: &HashMap<String, Value>, key: &str) -> String {
    m.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default()
}

fn code_filter(code: &str) -> Vec<Filter> {
    vec![Filter {
        field: "code".into(),
        operator: FilterOp::Equal,
        value: json!(code),
    }]
}

/// Whether `code` is exactly `CODE_LEN` characters of `CODE_ALPHABET`.
pub fn valid_code(code: &str) -> bool {
    code.chars().count() == CODE_LEN && code.chars().all(|c| CODE_ALPHABET.contains(c))
}

/// A live room's two SDP fields, as read off one row. `answer_sdp` is empty
/// until answered.
struct RoomRow {
    offer_sdp: String,
    answer_sdp: String,
}

/// Read the row for `code`, treating a past `expires_at` as missing —
/// deleting it on the way out, the same discipline `oauth_pkce::take` uses
/// for its own expiry check. `Gone` covers both "no such code" and "expired".
async fn fetch_live(ctx: &dyn Context, code: &str) -> Result<RoomRow, RoomError> {
    let rows = db::list_all(ctx, TABLE, code_filter(code))
        .await
        .map_err(db_err)?;
    let Some(record) = rows.into_iter().next() else {
        return Err(RoomError::Gone);
    };
    let expires_at = map_str(&record.data, "expires_at");
    if expires_at.as_str() < now_iso().as_str() {
        // Present but expired: drop it here so the next reader doesn't pay
        // for the same discovery, exactly as `open_room`'s own sweep would.
        let _ = db::delete_by_filters_count(ctx, TABLE, code_filter(code)).await;
        return Err(RoomError::Gone);
    }
    Ok(RoomRow {
        offer_sdp: map_str(&record.data, "offer_sdp"),
        answer_sdp: map_str(&record.data, "answer_sdp"),
    })
}

/// Put the host's offer up under `code`, which also creates the room.
/// Sweeps rows past their expiry first, so an abandoned room never holds a
/// code hostage and there is no background job to own.
pub async fn open_room(
    ctx: &dyn Context,
    code: &str,
    sdp: &str,
    ttl_secs: i64,
) -> Result<(), RoomError> {
    if !valid_code(code) {
        return Err(RoomError::BadCode);
    }
    let now = now_iso();
    sweep(ctx, &now).await?;
    // The sweep above has already dropped anything expired, so a row still
    // here for this code is live.
    let existing = db::list_all(ctx, TABLE, code_filter(code))
        .await
        .map_err(db_err)?;
    if !existing.is_empty() {
        return Err(RoomError::Taken);
    }
    let mut data: HashMap<String, Value> = HashMap::new();
    data.insert("code".into(), json!(code));
    data.insert("offer_sdp".into(), json!(sdp));
    data.insert("answer_sdp".into(), json!(""));
    data.insert("created_at".into(), json!(now));
    data.insert("expires_at".into(), json!(iso_plus_seconds(ttl_secs)));
    db::create(ctx, TABLE, data).await.map_err(db_err)?;
    Ok(())
}

/// The host's offer. `Gone` when the code is unknown or expired.
pub async fn offer_for(ctx: &dyn Context, code: &str) -> Result<String, RoomError> {
    Ok(fetch_live(ctx, code).await?.offer_sdp)
}

/// Write the guest's answer into an unanswered room. `Gone` if the row was
/// deleted (expiry, or a concurrent `take_answer`) between the read that
/// found it live and the write — `update_by_filters_count`'s returned count
/// is what tells the two apart from a normal success.
pub async fn answer_room(ctx: &dyn Context, code: &str, sdp: &str) -> Result<(), RoomError> {
    if !valid_code(code) {
        return Err(RoomError::BadCode);
    }
    let row = fetch_live(ctx, code).await?;
    if !row.answer_sdp.is_empty() {
        return Err(RoomError::Answered);
    }
    let mut data: HashMap<String, Value> = HashMap::new();
    data.insert("answer_sdp".into(), json!(sdp));
    let count = db::update_by_filters_count(ctx, TABLE, code_filter(code), data)
        .await
        .map_err(db_err)?;
    if count == 0 {
        return Err(RoomError::Gone);
    }
    Ok(())
}

/// The guest's answer, and the room with it: `Ok(None)` while nobody has
/// answered, `Ok(Some(sdp))` once one has — and then the row is deleted, so
/// a handshake is single-use. `Gone` when there is no room at all.
///
/// Deliberately two calls (a read, then an unconditional delete keyed only
/// by `code`) rather than one unconditional take on every poll: consuming on
/// every poll would delete the room the moment the host asks before anyone
/// has answered. A lost race between two polls both reading a fresh answer
/// just means the loser's delete matches nothing — the sdp it already read
/// is still what it returns, and the other side's next poll sees `Gone` (a
/// join that failed rather than one that half-worked).
///
/// The delete is `db::delete_by_filters_count`, not `db::take_by_filters`:
/// `DbExec`'s shared `take_where` (upstream `wafer-core`) issues its
/// `DELETE … RETURNING` through the reader-pool `run_fetch` path rather than
/// `run_execute`, so against a file-backed `SQLiteDatabaseService::open`
/// (every real native deployment — anything with dedicated reader workers,
/// see that crate's own `run_fetch` docs) the delete hits a read-only
/// connection, fails with "attempt to write a readonly database", and that
/// failure is swallowed per-row rather than surfaced — the row silently
/// survives and a poll after the first would hand the answer out again
/// forever instead of 404ing. Confirmed against the real native binary
/// (`cargo build -p impresspress --bin impresspress` + a curl smoke test),
/// not just the in-memory test fixture, which has no reader pool and never
/// exercises the bug. `delete_by_filters_count` is the same call `sweep`
/// already uses and goes through `run_execute`, so it isn't exposed to it.
pub async fn take_answer(ctx: &dyn Context, code: &str) -> Result<Option<String>, RoomError> {
    let row = fetch_live(ctx, code).await?;
    if row.answer_sdp.is_empty() {
        return Ok(None);
    }
    db::delete_by_filters_count(ctx, TABLE, code_filter(code))
        .await
        .map_err(db_err)?;
    Ok(Some(row.answer_sdp))
}

/// Delete every row whose `expires_at < cutoff`. Returns how many.
pub async fn sweep(ctx: &dyn Context, cutoff: &str) -> Result<u64, RoomError> {
    let n = db::delete_by_filters_count(
        ctx,
        TABLE,
        vec![Filter {
            field: "expires_at".into(),
            operator: FilterOp::LessThan,
            value: json!(cutoff),
        }],
    )
    .await
    .map_err(db_err)?;
    Ok(n.max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestContext;

    fn sdp(n: usize) -> String {
        "v=0\r\n".repeat(n)
    }

    #[tokio::test]
    async fn a_room_holds_an_offer_then_an_answer_then_is_gone() {
        let ctx = TestContext::with_signal().await;
        open_room(&ctx, "AB2CD3", &sdp(1), 600).await.expect("open");
        assert_eq!(offer_for(&ctx, "AB2CD3").await.expect("offer"), sdp(1));
        // Nobody has answered yet: waiting, not gone.
        assert!(take_answer(&ctx, "AB2CD3").await.expect("poll").is_none());
        answer_room(&ctx, "AB2CD3", &sdp(2)).await.expect("answer");
        assert_eq!(
            take_answer(&ctx, "AB2CD3").await.expect("take"),
            Some(sdp(2))
        );
        // Single use: the row went with the answer.
        assert!(matches!(
            take_answer(&ctx, "AB2CD3").await,
            Err(RoomError::Gone)
        ));
        assert!(matches!(
            offer_for(&ctx, "AB2CD3").await,
            Err(RoomError::Gone)
        ));
    }

    #[tokio::test]
    async fn a_live_code_cannot_be_opened_twice() {
        let ctx = TestContext::with_signal().await;
        open_room(&ctx, "AB2CD3", &sdp(1), 600).await.expect("open");
        assert!(matches!(
            open_room(&ctx, "AB2CD3", &sdp(1), 600).await,
            Err(RoomError::Taken)
        ));
    }

    #[tokio::test]
    async fn an_expired_code_is_free_again_and_reads_as_gone() {
        let ctx = TestContext::with_signal().await;
        open_room(&ctx, "AB2CD3", &sdp(1), -10).await.expect("open");
        assert!(matches!(
            offer_for(&ctx, "AB2CD3").await,
            Err(RoomError::Gone)
        ));
        // Read dropped it; and even without the read, create sweeps.
        open_room(&ctx, "AB2CD3", &sdp(3), 600)
            .await
            .expect("reopen");
        assert_eq!(offer_for(&ctx, "AB2CD3").await.expect("offer"), sdp(3));
    }

    #[tokio::test]
    async fn a_paired_room_refuses_a_second_answer() {
        let ctx = TestContext::with_signal().await;
        open_room(&ctx, "AB2CD3", &sdp(1), 600).await.expect("open");
        answer_room(&ctx, "AB2CD3", &sdp(2)).await.expect("answer");
        assert!(matches!(
            answer_room(&ctx, "AB2CD3", &sdp(3)).await,
            Err(RoomError::Answered)
        ));
    }

    #[tokio::test]
    async fn answering_a_room_that_never_existed_is_gone_not_a_create() {
        let ctx = TestContext::with_signal().await;
        assert!(matches!(
            answer_room(&ctx, "AB2CD3", &sdp(1)).await,
            Err(RoomError::Gone)
        ));
    }

    #[test]
    fn a_code_is_six_unambiguous_characters() {
        assert!(valid_code("AB2CD3"));
        assert!(!valid_code("AB2CD")); // short
        assert!(!valid_code("AB2CD3X")); // long
        assert!(!valid_code("AB2CD0")); // a zero reads as an O
        assert!(!valid_code("ab2cd3")); // the alphabet is upper case
        assert!(!valid_code("AB2CD/")); // and it is not a path
    }
}
