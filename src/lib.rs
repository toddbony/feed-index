//! Poll RSS/Atom/JSON feeds listed in an OPML file and index their entries into PostgreSQL.
//!
//! Feed documents are untrusted input. Nothing here logs entry titles, links or content, or
//! full feed URLs: log lines carry feed ids, hosts, status codes, outcomes, counts and
//! durations only.

pub mod config;
pub mod db;
pub mod entry;
pub mod fetch;
pub mod http;
pub mod links;
pub mod opml;
pub mod probe;
pub mod status;
pub mod sync;
pub mod text;

/// Written to `entries.fetcher_version` and `fetch_runs.fetcher_version`, and sent in the
/// User-Agent.
pub const FETCHER_VERSION: &str = env!("CARGO_PKG_VERSION");
