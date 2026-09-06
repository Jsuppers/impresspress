//! GET / PATCH /b/auth/api/me — relocated from auth/login.rs in Task 5.

use wafer_run::{context::Context, InputStream, Message, OutputStream};

use crate::{
    blocks::{
        auth::{
            helpers::get_user_roles,
            repo::users::{self, UserRow},
        },
        auth_ui::contracts::{MeResponse, MeUser, UpdateMeRequest},
        errors::{error_response, ErrorCode},
    },
    http::{err_bad_request, err_internal, err_not_found, ok_json},
};

/// The one projection both handlers share. `PATCH` used to build its own
/// flat `json!` object while `GET` returned `{user: {...}}`, so the two
/// paths published different shapes for the same resource and only one of
/// them had a schema. Routing both through here means the read and the
/// write path cannot drift on what they say about the caller.
fn me_response(user: UserRow, roles: Vec<String>) -> MeResponse {
    MeResponse {
        user: MeUser {
            id: user.id,
            email: user.email,
            name: user.display_name,
            roles,
            created_at: user.created_at,
            avatar_url: user.avatar_url.unwrap_or_default(),
        },
    }
}

pub async fn handle_get(ctx: &dyn Context, msg: &Message) -> OutputStream {
    let user_id = msg.user_id();
    if user_id.is_empty() {
        return error_response(ErrorCode::NotAuthenticated, "Not authenticated");
    }
    let Ok(Some(user)) = users::find_by_id(ctx, user_id).await else {
        return err_not_found("User not found");
    };
    let roles = match get_user_roles(ctx, user_id).await {
        Ok(r) => r,
        Err(e) => return err_internal("Failed to resolve user roles", e),
    };
    ok_json(&me_response(user, roles))
}

pub async fn handle_update(ctx: &dyn Context, msg: &Message, input: InputStream) -> OutputStream {
    let user_id = msg.user_id();
    if user_id.is_empty() {
        return error_response(ErrorCode::NotAuthenticated, "Not authenticated");
    }

    let raw = input.collect_to_bytes().await;
    let body: UpdateMeRequest = match serde_json::from_slice(&raw) {
        Ok(b) => b,
        Err(e) => return err_bad_request(&format!("Invalid body: {e}")),
    };

    // `name` dual-writes display_name + the legacy name alias inside
    // update_profile.
    match users::update_profile(
        ctx,
        user_id,
        body.name.as_deref(),
        body.avatar_url.as_deref(),
    )
    .await
    {
        Ok(user) => {
            let roles = match get_user_roles(ctx, user_id).await {
                Ok(r) => r,
                Err(e) => return err_internal("Failed to resolve user roles", e),
            };
            ok_json(&me_response(user, roles))
        }
        Err(e) => err_internal("Update failed", e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        blocks::auth::repo::users::NewUser,
        test_support::{anon_msg, auth_msg, output_is_error, output_json, TestContext},
    };

    async fn seed_user(ctx: &dyn Context) -> UserRow {
        users::insert(
            ctx,
            NewUser {
                email: "ada@example.com".to_string(),
                display_name: "Ada".to_string(),
                avatar_url: None,
                role: "user".to_string(),
                email_verified: false,
                verification_token_hash: None,
            },
        )
        .await
        .expect("seed user")
    }

    fn body(json: serde_json::Value) -> InputStream {
        InputStream::from_bytes(serde_json::to_vec(&json).unwrap())
    }

    /// `PATCH` used to return a flat user object while `GET` returned
    /// `{user: {...}}`. Both now go through `me_response`, so the update
    /// response must be exactly what a subsequent `GET` returns.
    #[tokio::test]
    async fn update_returns_the_same_envelope_as_get() {
        let ctx = TestContext::with_auth().await;
        let user = seed_user(&ctx).await;

        let updated = output_json(
            handle_update(
                &ctx,
                &auth_msg("update", "/b/auth/api/me", &user.id),
                body(serde_json::json!({
                    "name": "Ada Updated",
                    "avatar_url": "https://example.com/a.png"
                })),
            )
            .await,
        )
        .await;
        let fetched =
            output_json(handle_get(&ctx, &auth_msg("retrieve", "/b/auth/api/me", &user.id)).await)
                .await;

        assert_eq!(updated["user"]["name"], serde_json::json!("Ada Updated"));
        assert_eq!(
            updated["user"]["avatar_url"],
            serde_json::json!("https://example.com/a.png")
        );
        assert_eq!(
            updated, fetched,
            "PATCH and GET must publish the same projection of the same row"
        );
        assert!(
            updated.get("id").is_none(),
            "the flat pre-fix shape must not survive: {updated}"
        );
    }

    /// The typed body replaces a `HashMap` peek that silently treated a
    /// non-string `name` as absent. The published schema says `string`, so
    /// the handler must refuse what the schema refuses.
    #[tokio::test]
    async fn update_rejects_a_body_the_schema_rejects() {
        let ctx = TestContext::with_auth().await;
        let user = seed_user(&ctx).await;

        let out = handle_update(
            &ctx,
            &auth_msg("update", "/b/auth/api/me", &user.id),
            body(serde_json::json!({"name": 42})),
        )
        .await;
        assert!(output_is_error(out, "InvalidArgument").await);
    }

    #[tokio::test]
    async fn update_requires_a_signed_in_caller() {
        let ctx = TestContext::with_auth().await;
        let out = handle_update(
            &ctx,
            &anon_msg("update", "/b/auth/api/me"),
            body(serde_json::json!({"name": "x"})),
        )
        .await;
        assert!(output_is_error(out, "Unauthenticated").await);
    }
}
