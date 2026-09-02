use wafer_run::{
    context::Context, Block, ErrorCode, InputStream, LifecycleEvent, LifecycleType, Message,
    OutputStream, WaferError, WasmiBlock,
};

#[derive(Clone)]
struct NoServices;

#[async_trait::async_trait]
impl Context for NoServices {
    async fn call_block(
        &self,
        _name: &str,
        _message: Message,
        _input: InputStream,
    ) -> OutputStream {
        OutputStream::error(WaferError::new(
            ErrorCode::Unimplemented,
            "the proof block has no host services",
        ))
    }

    fn is_cancelled(&self) -> bool {
        false
    }

    fn config_get(&self, _key: &str) -> Option<&str> {
        None
    }

    fn clone_arc(&self) -> std::sync::Arc<dyn Context> {
        std::sync::Arc::new(self.clone())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let artifact = std::env::args()
        .nth(1)
        .ok_or("usage: verify-browser-compiled-wafer-block <guest.wasm>")?;
    let bytes = std::fs::read(&artifact)?;
    let block = WasmiBlock::load_from_bytes(&bytes)?;

    let info = block.info();
    assert_eq!(info.name, "browser/hello");
    assert_eq!(info.interface, "handler@v1");

    block
        .lifecycle(
            &NoServices,
            LifecycleEvent {
                event_type: LifecycleType::Init,
                data: Vec::new(),
            },
        )
        .await
        .map_err(|error| format!("guest lifecycle failed: {error}"))?;

    let response = block
        .handle(
            &NoServices,
            Message::new("http.request"),
            InputStream::empty(),
        )
        .await
        .collect_buffered()
        .await
        .map_err(|terminal| format!("guest did not respond: {terminal:?}"))?;

    let body = String::from_utf8(response.body)?;
    assert_eq!(body, "Hello from a browser-compiled WAFER block!");
    println!("loaded {} from {}: {body}", info.name, artifact);
    Ok(())
}
