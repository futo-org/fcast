//! SABR (Server ABR) / UMP streaming protocol client.
//!
//! A Rust implementation of the SABR/UMP protocol. Given a [`SabrStreamSpec`]
//! and a [`SabrTransport`], a session drives the SABR request/response loop,
//! parses the UMP stream, and exposes buffered media segments (fMP4 or WebM)
//! per format for a player (e.g. a gstreamer source element) to pull.
//!
//! The pump runs as an async task (tokio) and issues requests via reqwest
//! directly (the crate has a single consumer, so a generic transport trait
//! bought nothing). A [`SabrTransport::canned`] variant replays canned UMP bytes
//! so the protocol logic can be unit-tested without a live server.

pub mod buffer;
pub mod error;
pub mod format;
pub mod http;
pub mod proto;
pub mod segment;
pub mod session;
pub mod spec;
pub mod ump;

pub use buffer::SabrTrackBuffer;
pub use error::{SabrError, SabrResult};
pub use format::{SabrFormat, SabrFormatKey};
pub use http::{SabrBody, SabrTransport};
pub use segment::SabrSegment;
pub use session::{SabrSession, SabrSessionEvent, SabrSessionListener};
pub use spec::{Role, SabrStreamSpec};
pub use ump::{PartType, UmpPart, UmpReader};
