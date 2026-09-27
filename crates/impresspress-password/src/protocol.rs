//! The call between the main Cloudflare Worker and the password-hasher
//! Worker, and the names both sides and the deploy tooling share.
//!
//! On Cloudflare, password hashing and verification do not run in the Worker
//! that serves requests. They run in a Durable Object
//! ([`DURABLE_OBJECT_CLASS`]) exported by a second, small Worker — the
//! password-hasher Worker — which the main Worker reaches through a Durable
//! Object binding ([`BINDING`]) naming that Worker's script. A Worker request
//! on the Free plan is documented at 10 ms of CPU, and argon2id at OWASP's
//! recommended cost (19 MiB, 2 iterations) takes about 50-130 ms of it in
//! wasm32; a Durable Object request has a far larger allowance. The class
//! lives in a Worker of its own because Cloudflare generates no version
//! preview URLs for a Worker that implements a Durable Object, and the main
//! Worker's deploy funnel prepares and verifies each version through one.
//!
//! # One request, one answer
//!
//! The main Worker sends one JSON [`Request`] per operation as the body of a
//! `POST` to a Durable Object stub, and the object answers one JSON
//! [`Response`] with status 200 — including for a wrong password, a stored
//! hash it cannot check and a pepper fault, which are answers, not transport
//! failures. Any other status, an unreadable body, or an answer that does not
//! fit the operation means the hasher could not be asked, and the main
//! Worker reports the operation as failed. It never hashes or verifies a
//! password itself instead: a fallback would write hashes at whatever cost the
//! Worker can afford, silently.
//!
//! # Compatibility across a release
//!
//! `impresspress deploy` deploys the password-hasher Worker FIRST, then takes
//! the main Worker through its prepare/verify/promote funnel, and every
//! version of the main Worker — a preview being verified, the live one, a
//! rollback target — talks to whichever hasher is deployed at that moment.
//! So a hasher must answer the requests of the main Worker release before it,
//! not just its own. The rule:
//!
//! - A change to [`Request`] or [`Response`] that an older peer cannot read
//!   bumps [`PROTOCOL_VERSION`].
//! - The hasher answers every version from [`OLDEST_ACCEPTED_VERSION`] to
//!   [`PROTOCOL_VERSION`], in the version it was asked in; the release that
//!   bumps the version keeps [`OLDEST_ACCEPTED_VERSION`] at the previous one
//!   (a compile-time assertion below holds it there), and a later release may
//!   raise it once no deployed main Worker sends the old one.
//! - A version outside that range is answered [`Outcome::UnsupportedVersion`],
//!   which the main Worker reports as the hasher being unavailable.

use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};
use wafer_core::interfaces::crypto::service::CryptoError;

/// The Durable Object binding the main Worker reaches the hasher through.
pub const BINDING: &str = "IMPRESSPRESS_PASSWORD_HASHER";

/// The Durable Object class the password-hasher Worker exports. The
/// `durable_object` module's struct carries this name; a compile-time check
/// there keeps the two equal.
pub const DURABLE_OBJECT_CLASS: &str = "ImpresspressPasswordHasher";

/// What the deploy tooling appends to the main Worker's name to name the
/// password-hasher Worker when `impresspress.toml` does not name it.
pub const WORKER_NAME_SUFFIX: &str = "-password-hasher";

/// The main Worker var holding how many Durable Object instances hashing is
/// spread across. Written into the generated config from `impresspress.toml`'s
/// `[cloudflare.password_hasher].shards`.
pub const SHARDS_VAR: &str = "IMPRESSPRESS_PASSWORD_HASHER_SHARDS";

/// Instances hashing is spread across when [`SHARDS_VAR`] is unset.
///
/// A Durable Object instance runs one request at a time, and a hash costs
/// about 75-130 ms of CPU there, so one instance caps the whole deployment at
/// roughly ten logins a second and queues a burst behind itself. Instances
/// cost nothing while idle.
pub const DEFAULT_SHARDS: u32 = 8;

/// The accepted range of [`SHARDS_VAR`].
pub const SHARDS_RANGE: RangeInclusive<u32> = 1..=64;

/// Read [`SHARDS_VAR`]: unset is [`DEFAULT_SHARDS`]; anything else must be a
/// whole number in [`SHARDS_RANGE`].
pub fn parse_shards(raw: Option<&str>) -> Result<u32, String> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_SHARDS);
    };
    let shards = raw
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("{SHARDS_VAR} is {raw:?}; it must be a whole number"))?;
    validate_shards(shards)
}

/// Check a shard count against [`SHARDS_RANGE`].
pub fn validate_shards(shards: u32) -> Result<u32, String> {
    if SHARDS_RANGE.contains(&shards) {
        Ok(shards)
    } else {
        Err(format!(
            "{SHARDS_VAR} is {shards}; it must be between {} and {}",
            SHARDS_RANGE.start(),
            SHARDS_RANGE.end()
        ))
    }
}

/// The name of the Durable Object instance a request goes to, picked by
/// `random` (any value, e.g. four random bytes) among `shards` instances.
/// Names are stable (`shard-0` … `shard-{shards - 1}`), so the instances are
/// reused rather than created per request.
pub fn shard_name(shards: u32, random: u32) -> String {
    format!("shard-{}", random % shards.max(1))
}

/// The protocol version this build sends and answers in by default.
pub const PROTOCOL_VERSION: u32 = 1;

/// The oldest version the hasher answers. See the module docs.
pub const OLDEST_ACCEPTED_VERSION: u32 = 1;

// The hasher keeps answering the previous release's main Worker.
const _: () = assert!(
    OLDEST_ACCEPTED_VERSION <= PROTOCOL_VERSION
        && OLDEST_ACCEPTED_VERSION + 1 >= PROTOCOL_VERSION
        && OLDEST_ACCEPTED_VERSION >= 1
);

/// The versions the hasher answers.
pub const ACCEPTED_VERSIONS: RangeInclusive<u32> = OLDEST_ACCEPTED_VERSION..=PROTOCOL_VERSION;

/// What the main Worker asks the hasher. No `Debug`: it carries a password.
#[derive(Serialize, Deserialize)]
pub struct Request {
    /// The protocol version the request is written in.
    pub version: u32,
    /// The operation.
    pub operation: Operation,
}

/// One hashing operation. No `Debug`: it carries a password.
#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    /// Hash a new password.
    Hash {
        /// The password.
        password: String,
    },
    /// Check a password against a stored hash of any scheme the crypto
    /// primitives recognise.
    Verify {
        /// The password.
        password: String,
        /// The stored hash.
        hash: String,
    },
}

impl Request {
    /// A request at [`PROTOCOL_VERSION`].
    pub fn new(operation: Operation) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            operation,
        }
    }

    /// The JSON body sent to the Durable Object.
    pub fn to_body(&self) -> String {
        serde_json::to_string(self).expect("a request of strings always serializes")
    }
}

/// What the hasher answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// The version the answer is written in: the request's, when the hasher
    /// accepts it, and the hasher's own [`PROTOCOL_VERSION`] otherwise.
    pub version: u32,
    /// The outcome.
    pub outcome: Outcome,
}

/// How an operation went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// `hash` succeeded.
    Hashed {
        /// The new hash.
        hash: String,
    },
    /// `verify`: the password matches.
    Verified,
    /// `verify`: the password is wrong.
    Mismatch,
    /// `verify`: the stored hash is one the hasher cannot check
    /// (`CryptoError::MalformedHash`).
    MalformedHash {
        /// The crypto primitives' message.
        message: String,
    },
    /// The hasher's pepper configuration stands in the way
    /// (`CryptoError::Pepper`): a key it does not hold, a pepper it requires,
    /// or pepper secrets that do not parse.
    Pepper {
        /// What is wrong, naming the variable; never key material.
        message: String,
    },
    /// The primitives failed for another reason.
    Failed {
        /// The primitives' message.
        message: String,
    },
    /// The request's version is outside the range this hasher answers.
    UnsupportedVersion {
        /// The oldest version this hasher answers.
        oldest: u32,
        /// The newest version this hasher answers.
        newest: u32,
    },
    /// The request could not be read.
    BadRequest {
        /// What was wrong with it; never the password.
        message: String,
    },
}

/// The prefix of every error that means the hasher could not be asked or
/// did not answer the question; see [`unavailable`].
pub const UNAVAILABLE_PREFIX: &str = "password hasher unavailable: ";

/// The error an operation fails with when the hasher could not answer it.
///
/// `CryptoError::Other`, which the crypto block reports as an internal error,
/// so a login answers 503 (`impresspress_core::blocks::auth::check_password`)
/// and nothing about the stored credential is inferred from it.
pub fn unavailable(reason: impl std::fmt::Display) -> CryptoError {
    CryptoError::Other(format!("{UNAVAILABLE_PREFIX}{reason}"))
}

impl Response {
    /// Read a response body the hasher sent with status 200.
    pub fn from_body(body: &[u8]) -> Result<Self, CryptoError> {
        serde_json::from_slice(body)
            .map_err(|e| unavailable(format!("its answer could not be read: {e}")))
    }

    /// The answer to a [`Operation::Hash`], as the crypto service returns it.
    pub fn into_hash(self) -> Result<String, CryptoError> {
        match self.into_answer()? {
            Outcome::Hashed { hash } => Ok(hash),
            other => Err(unexpected("hash", &other)),
        }
    }

    /// The answer to a [`Operation::Verify`], as the crypto service returns
    /// it.
    pub fn into_verify(self) -> Result<(), CryptoError> {
        match self.into_answer()? {
            Outcome::Verified => Ok(()),
            Outcome::Mismatch => Err(CryptoError::PasswordMismatch),
            Outcome::MalformedHash { message } => Err(CryptoError::MalformedHash(message)),
            other => Err(unexpected("verify", &other)),
        }
    }

    /// The outcome, with the ones every operation shares already turned into
    /// errors. An answer in a version this build does not speak is refused
    /// before its outcome is trusted.
    fn into_answer(self) -> Result<Outcome, CryptoError> {
        if !ACCEPTED_VERSIONS.contains(&self.version) {
            return Err(unavailable(format!(
                "it answered in protocol version {}, and this Worker reads {}..={}",
                self.version, OLDEST_ACCEPTED_VERSION, PROTOCOL_VERSION
            )));
        }
        match self.outcome {
            Outcome::Pepper { message } => Err(CryptoError::Pepper(message)),
            Outcome::Failed { message } => Err(CryptoError::HashError(message)),
            Outcome::UnsupportedVersion { oldest, newest } => Err(unavailable(format!(
                "it answers protocol versions {oldest}..={newest}, and this Worker sent \
                 {PROTOCOL_VERSION}"
            ))),
            Outcome::BadRequest { message } => {
                Err(unavailable(format!("it refused the request: {message}")))
            }
            other => Ok(other),
        }
    }
}

fn unexpected(operation: &str, outcome: &Outcome) -> CryptoError {
    // Only the kind: a `Hashed` outcome carries a hash, which does not belong
    // in a log line.
    let kind = match outcome {
        Outcome::Hashed { .. } => "hashed",
        Outcome::Verified => "verified",
        Outcome::Mismatch => "mismatch",
        Outcome::MalformedHash { .. } => "malformed_hash",
        Outcome::Pepper { .. } => "pepper",
        Outcome::Failed { .. } => "failed",
        Outcome::UnsupportedVersion { .. } => "unsupported_version",
        Outcome::BadRequest { .. } => "bad_request",
    };
    unavailable(format!(
        "it answered a {operation} request with a {kind} outcome"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_counts_parse_and_default() {
        assert_eq!(parse_shards(None), Ok(DEFAULT_SHARDS));
        assert_eq!(parse_shards(Some(" 3 ")), Ok(3));
        assert_eq!(parse_shards(Some("64")), Ok(64));
        for bad in ["0", "65", "-1", "two", ""] {
            let err = parse_shards(Some(bad)).expect_err(bad);
            assert!(err.contains(SHARDS_VAR), "{err}");
        }
    }

    #[test]
    fn shard_names_stay_within_the_configured_count() {
        let names: std::collections::BTreeSet<String> =
            (0..100u32).map(|random| shard_name(4, random)).collect();
        let expected: std::collections::BTreeSet<String> =
            (0..4).map(|i| format!("shard-{i}")).collect();
        assert_eq!(names, expected);
        assert_eq!(shard_name(1, u32::MAX), "shard-0");
    }

    /// The wire format is a contract with the hasher of the previous and the
    /// next release: pin it.
    #[test]
    fn the_wire_format_is_pinned() {
        assert_eq!(
            Request::new(Operation::Verify {
                password: "pw".into(),
                hash: "$argon2id$x".into(),
            })
            .to_body(),
            r#"{"version":1,"operation":{"op":"verify","password":"pw","hash":"$argon2id$x"}}"#
        );
        assert_eq!(
            Request::new(Operation::Hash {
                password: "pw".into()
            })
            .to_body(),
            r#"{"version":1,"operation":{"op":"hash","password":"pw"}}"#
        );
        let answer = Response {
            version: 1,
            outcome: Outcome::MalformedHash {
                message: "m".into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&answer).unwrap(),
            r#"{"version":1,"outcome":{"kind":"malformed_hash","message":"m"}}"#
        );
    }

    #[test]
    fn outcomes_become_the_crypto_errors_the_auth_block_classifies() {
        let at = |outcome| Response {
            version: PROTOCOL_VERSION,
            outcome,
        };
        assert_eq!(
            at(Outcome::Hashed { hash: "h".into() })
                .into_hash()
                .unwrap(),
            "h"
        );
        at(Outcome::Verified).into_verify().unwrap();
        assert!(matches!(
            at(Outcome::Mismatch).into_verify(),
            Err(CryptoError::PasswordMismatch)
        ));
        assert!(matches!(
            at(Outcome::MalformedHash { message: "m".into() }).into_verify(),
            Err(CryptoError::MalformedHash(m)) if m == "m"
        ));
        for pepper in [
            at(Outcome::Pepper {
                message: "p".into(),
            })
            .into_verify(),
            at(Outcome::Pepper {
                message: "p".into(),
            })
            .into_hash()
            .map(drop),
        ] {
            assert!(matches!(pepper, Err(CryptoError::Pepper(m)) if m == "p"));
        }
    }

    /// Anything that is not an answer to the question asked is the hasher
    /// being unavailable — never a match, a mismatch or a hash.
    #[test]
    fn a_non_answer_is_unavailable() {
        let unavailable_err = |result: Result<(), CryptoError>| match result {
            Err(CryptoError::Other(message)) => {
                assert!(message.starts_with(UNAVAILABLE_PREFIX), "{message}")
            }
            other => panic!("expected the hasher to be unavailable, got {other:?}"),
        };
        let at = |version, outcome| Response { version, outcome };
        unavailable_err(at(1, Outcome::Hashed { hash: "h".into() }).into_verify());
        unavailable_err(at(1, Outcome::Verified).into_hash().map(drop));
        unavailable_err(at(1, Outcome::Mismatch).into_hash().map(drop));
        unavailable_err(
            at(
                1,
                Outcome::UnsupportedVersion {
                    oldest: 2,
                    newest: 3,
                },
            )
            .into_verify(),
        );
        unavailable_err(
            at(
                1,
                Outcome::BadRequest {
                    message: "x".into(),
                },
            )
            .into_verify(),
        );
        // A verdict written in a version this build does not speak is not
        // trusted, even one that says "verified".
        unavailable_err(at(PROTOCOL_VERSION + 1, Outcome::Verified).into_verify());
        unavailable_err(at(0, Outcome::Verified).into_verify());
        unavailable_err(Response::from_body(b"<html>").map(drop));
    }
}
