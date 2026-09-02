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
            name: String::new(), columns: vec![], indexes: vec![], primary_key: vec![], unique_keys: vec![],
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
