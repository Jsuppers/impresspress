//! The path variables the block's route table binds for the storage API:
//! bucket name (`{name}`) and object key (`{key...}`, may contain `/`).

use wafer_run::Message;

/// The bucket name bound by the table's `{name}` segment; empty when the
/// request matched no row that has one.
pub(super) fn extract_bucket_name(msg: &Message) -> String {
    msg.var("name").to_string()
}

/// The object key bound by the table's `{key...}` rest segment (slashes
/// preserved, percent-decoded); empty when the request matched no such row.
pub(super) fn extract_object_key(msg: &Message) -> String {
    msg.var("key").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a message carrying `path` on `req.resource` and, optionally, the
    /// matcher-bound `{name}`/`{key}` path vars in `req.param.*`.
    fn msg_with(path: &str, params: &[(&str, &str)]) -> Message {
        let mut m = Message::new("test");
        m.set_meta("req.resource", path);
        for (k, v) in params {
            m.set_meta(format!("req.param.{k}"), *v);
        }
        m
    }

    #[test]
    fn test_extract_bucket_name_from_param() {
        // Router-populated path var wins (the normal dispatch path).
        let m = msg_with(
            "/b/storage/api/buckets/my-bucket/objects",
            &[("name", "my-bucket")],
        );
        assert_eq!(extract_bucket_name(&m), "my-bucket");
    }

    #[test]
    fn test_extract_object_key_from_param() {
        // Rest param preserves embedded slashes.
        let m = msg_with(
            "/b/storage/api/buckets/b/objects/dir/file.txt",
            &[("key", "dir/file.txt")],
        );
        assert_eq!(extract_object_key(&m), "dir/file.txt");
    }

    /// The readers see only what the block's table bound: an unrouted
    /// message binds nothing (the handler then answers "missing bucket name"
    /// / "missing key"), and the same message through the real table binds
    /// both, the key with its slashes.
    #[test]
    fn bucket_and_key_are_bound_by_the_table() {
        use crate::{blocks::files::test_support::routed, test_support::auth_msg};

        let unrouted = auth_msg(
            "retrieve",
            "/b/storage/api/buckets/photos/objects/dir/file.txt",
            "alice",
        );
        assert_eq!(extract_bucket_name(&unrouted), "");
        assert_eq!(extract_object_key(&unrouted), "");

        let bound = routed(unrouted);
        assert_eq!(extract_bucket_name(&bound), "photos");
        assert_eq!(extract_object_key(&bound), "dir/file.txt");

        let bucket_only = routed(auth_msg("delete", "/b/storage/api/buckets/photos", "alice"));
        assert_eq!(extract_bucket_name(&bucket_only), "photos");
        assert_eq!(extract_object_key(&bucket_only), "");
    }
}
