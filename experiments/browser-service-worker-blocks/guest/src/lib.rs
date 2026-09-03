//! Minimal WAFER ABI-v1 guest with no Cargo dependencies or procedural macros.
//!
//! Rubrc currently supports `wasm32-wasip1`, but not external crates or proc
//! macros. Keeping this spike to `std` tests the smallest useful overlap
//! between Rubrc's present compiler surface and WAFER's stable JSON ABI.

// One declared endpoint: the sandbox refuses a guest that declares none
// (`endpoints-empty`) — a block serves its prefix through the endpoints it
// declares, and the declared `auth` is what the router enforces for the
// route. The path is under the prefix the sandbox derives from the renamed
// `site/hello`; under the guest's own name it is refused earlier, by
// `name-mismatch`, which is what the e2e's step 4 pins.
const INFO: &[u8] = br#"{
  "name":"browser/hello",
  "version":"0.1.0",
  "interface":"handler@v1",
  "summary":"A no-dependency block compiled for wasm32-wasip1",
  "endpoints":[{"method":"GET","path":"/b/hello/","summary":"Greets the visitor","auth":"public"}]
}"#;

const RESPONSE: &[u8] = br#"{
  "action":"Respond",
  "response":{
    "data":[72,101,108,108,111,32,102,114,111,109,32,97,32,98,114,111,119,115,101,114,45,99,111,109,112,105,108,101,100,32,87,65,70,69,82,32,98,108,111,99,107,33],
    "meta":[{"key":"content-type","value":"text/plain; charset=utf-8"}]
  },
  "error":null,
  "message":null
}"#;

const LIFECYCLE_OK: &[u8] = br#"{"Ok":null}"#;

fn pack(bytes: &'static [u8]) -> i64 {
    ((bytes.as_ptr() as u32 as i64) << 32) | bytes.len() as i64
}

/// Allocate guest memory for the host's request/lifecycle frame.
#[no_mangle]
pub extern "C" fn __wafer_alloc(size: i32) -> i32 {
    let allocation = vec![0_u8; size.max(0) as usize].into_boxed_slice();
    Box::leak(allocation).as_mut_ptr() as i32
}

/// No `__wafer_abi_version` export: absence deliberately negotiates ABI v1
/// (JSON), allowing this block to avoid serde/rmp and their Cargo graph.
#[no_mangle]
pub extern "C" fn __wafer_info() -> i64 {
    pack(INFO)
}

#[no_mangle]
pub extern "C" fn __wafer_handle(_message_ptr: i32, _message_len: i32) -> i64 {
    pack(RESPONSE)
}

#[no_mangle]
pub extern "C" fn __wafer_lifecycle(_event_ptr: i32, _event_len: i32) -> i64 {
    pack(LIFECYCLE_OK)
}
