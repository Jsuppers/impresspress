//! The dev-sandbox plans use these producer APIs; a wrong pin fails here,
//! naming the missing item, instead of deep inside a block.
#[test]
fn producer_surface_is_pinned() {
    // `generate_webmcp_selected` takes `impl Fn(...)`, so a bare function
    // pointer can't infer the type parameter — call it with concrete
    // arguments instead to prove the whole signature (including
    // `ToolSelection`) resolves in-tree.
    let (_manifest, _refused) = wafer_core::discovery::generate_webmcp_selected(
        &[] as &[wafer_block::types::BlockInfo],
        wafer_block::types::AuthLevel::Public,
        |_block: &wafer_block::types::BlockInfo, ep: &wafer_block::types::BlockEndpoint| ep.auth,
        &[] as &[wafer_core::discovery::ToolSelection],
    );
    let _ = wafer_block::wire::database::EnsureTableRequest {
        table: wafer_block::wire::database::TableDef {
            name: String::new(),
            columns: vec![],
            indexes: vec![],
            primary_key: vec![],
            unique_keys: vec![],
        },
    };
    assert_eq!(wafer_block::abi::HOST_CODEC_JSON, 1);

    let caps = wafer_block::BlockCapabilities {
        schema: true,
        ..wafer_block::BlockCapabilities::none()
    };
    assert!(caps.schema);

    assert_eq!(wafer_block::wrap::SCHEMA_RESOURCE, "__schema__");
}

/// The producer surface the phase-4 adapter work consumes, pinned at the bump
/// so a later pin move fails here by name rather than mid-refactor.
///
/// Every item below arrived in the four upstream PRs this rev carries (#328,
/// #330, #331, #332). Naming one is not adopting it — the call sites land in
/// the PRs that follow.
///
/// Not pinned here: `PasswordScheme` / `Argon2JwtCryptoService::with_password_scheme`
/// and `primitives::{pbkdf2_hash, pbkdf2_verify}`, because `wafer-block-crypto`
/// is not a dependency of `impresspress-core`. Their consumers are
/// `impresspress-browser` and `impresspress-cloudflare`, which pin them when
/// they take the dependency.
#[test]
fn phase_four_producer_surface_is_pinned() {
    // #330: `AuthLevel` derives `Ord`, so the variant order *is* the
    // strictness ladder and the consumer's private `auth_rank` in
    // `endpoint_match.rs` becomes deletable.
    use wafer_block::types::AuthLevel;
    assert!(AuthLevel::Public < AuthLevel::Authenticated);
    assert!(AuthLevel::Authenticated < AuthLevel::Admin);

    // #330: the equality-filter predicate on the wire type, so every vector
    // backend answers a query the same way. An entry with no metadata
    // satisfies only the empty filter.
    let filter = wafer_block::wire::vector::MetadataFilter::default();
    assert!(filter.matches(None));

    // #330: the shared body of a `ConfigSource`, so the resolution rules
    // cannot drift between sources.
    // It takes `impl Fn`, so a fn-pointer annotation cannot name it; call it
    // with concrete arguments instead, the way `generate_webmcp_selected` is
    // named above.
    let resolved: wafer_run::EnvBlockConfig =
        wafer_run::resolve_declared("impresspress/files", &[], |_key: &str| None)
            .expect("no declared keys resolves to an empty config");
    assert_eq!(resolved.get("ANYTHING"), None);

    // #328: one decode policy for SQL result rows, so a value written through
    // `create` reads back the same shape on every backend (B25).
    use wafer_core::interfaces::database::codec;
    assert_eq!(
        codec::decode_text_value("{\"a\":1}"),
        serde_json::json!({"a": 1})
    );
    assert_eq!(
        codec::decode_text_value("not json"),
        serde_json::json!("not json")
    );
    let _: fn(serde_json::Value) -> wafer_core::interfaces::database::service::Record =
        codec::record_from_json_row;

    // #328: the three defaulted `DbExec` operations. Named as function items
    // so the signatures resolve without an impl in this crate.
    fn _db_exec_defaults<T: wafer_core::interfaces::database::exec::DbExec>() {
        let _ = T::ensure_schema_table;
        let _ = T::run_schema_table_ddl;
        let _ = T::create_many;
    }

    // #328: the forwarder macro and the `async_trait` re-export its generated
    // `impl` needs. `macro_rules!` has no value form, so existence is pinned
    // by naming the path; the KV-cache decorator writes the ledger in PR 6.
    #[allow(unused_imports)]
    use wafer_core::{forward_database_service, wafer_async_trait};

    // #330: `fuse` discards the fused RRF score, which is why the browser
    // adapter re-implemented RRF instead of calling it.
    let _ = wafer_core::interfaces::vector::fuse_scored;
    let _: f32 = wafer_core::interfaces::vector::DEFAULT_RRF_K;

    // #331: static block registration works on wasm32. `WAFER_STATIC_BLOCKS`
    // is empty on every target where `linkme` works, so the call site needs
    // no `cfg`.
    let _: fn(
        &mut wafer_run::Wafer,
        &[&wafer_block::StaticBlockRegistration],
    ) -> Result<(), wafer_run::RuntimeError> = wafer_run::Wafer::register_static_blocks;
}
