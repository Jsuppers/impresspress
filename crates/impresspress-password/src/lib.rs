//! Password hashing shared by impresspress's targets.
//!
//! - [`pepper`]: the password pepper's settings and how a target parses them.
//! - [`protocol`]: the call between the main Cloudflare Worker and the
//!   password-hasher Worker, and the names both sides and the deploy tooling
//!   share.
//! - [`hasher`]: what the password-hasher Worker does with a request.
//! - `durable_object` (feature `durable-object`): the Durable Object class
//!   that Worker exports.

pub mod hasher;
pub mod pepper;
pub mod protocol;

#[cfg(feature = "durable-object")]
pub mod durable_object;
