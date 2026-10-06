//! The TOML configuration file. Never holds database settings (those come from the libpq
//! environment, see [`crate::db`]).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/feed-index/config.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// The feed list (OPML 2.0).
    pub opml: PathBuf,
    /// Sent in the User-Agent: `feed-index/<version> (+<contact>)`.
    pub contact: Option<String>,
    /// Minutes between fetches of a feed without a `poll_minutes` attribute (5..=1440).
    pub default_poll_minutes: i32,
    /// Requests in flight at once (1..=16).
    pub concurrency: usize,
    /// A response body larger than this is abandoned (`too_large`).
    pub max_feed_bytes: u64,
    /// Limit for one whole exchange: connect, redirects and body.
    pub timeout_seconds: u64,
    /// `feeds.fetches` rows older than this are deleted at the end of each run.
    pub fetch_history_days: i32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            opml: PathBuf::from("/etc/feed-index/feeds.opml"),
            contact: None,
            default_poll_minutes: 60,
            concurrency: 4,
            max_feed_bytes: 10_485_760,
            timeout_seconds: 30,
            fetch_history_days: 90,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("config file {}", path.display()))
    }

    /// For `probe`: the defaults when the file does not exist, otherwise as [`Config::load`].
    pub fn load_or_default(path: &Path) -> Result<Config> {
        match std::fs::metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            _ => Self::load(path),
        }
    }

    pub fn parse(text: &str) -> Result<Config> {
        // toml's Display quotes the offending line; report the position and the bare message.
        let config: Config = toml::from_str(text).map_err(|e| {
            let at = e
                .span()
                .map(|s| {
                    let line = text[..s.start].matches('\n').count() + 1;
                    format!(" at line {line}")
                })
                .unwrap_or_default();
            anyhow::anyhow!("invalid configuration{at}: {}", e.message())
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !(5..=1440).contains(&self.default_poll_minutes) {
            bail!("default_poll_minutes must be between 5 and 1440");
        }
        if !(1..=16).contains(&self.concurrency) {
            bail!("concurrency must be between 1 and 16");
        }
        if self.max_feed_bytes == 0 {
            bail!("max_feed_bytes must be greater than 0");
        }
        if self.timeout_seconds == 0 {
            bail!("timeout_seconds must be greater than 0");
        }
        if self.fetch_history_days <= 0 {
            bail!("fetch_history_days must be greater than 0");
        }
        if let Some(c) = &self.contact
            && (c.trim().is_empty() || c.chars().any(char::is_control))
        {
            bail!("contact must be non-empty and contain no control characters");
        }
        Ok(())
    }

    pub fn user_agent(&self) -> String {
        let version = crate::FETCHER_VERSION;
        match &self.contact {
            Some(c) => format!("feed-index/{version} (+{})", c.trim()),
            None => format!("feed-index/{version}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example() {
        let c = Config::parse(include_str!("../example.toml")).unwrap();
        assert_eq!(c.opml, Path::new("/etc/feed-index/feeds.opml"));
        assert_eq!(c.default_poll_minutes, 60);
        assert_eq!(c.concurrency, 4);
        assert_eq!(c.max_feed_bytes, 10_485_760);
        assert_eq!(c.timeout_seconds, 30);
        assert_eq!(c.fetch_history_days, 90);
        assert!(
            c.user_agent()
                .ends_with(" (+https://example.org/feed-index)")
        );
    }

    #[test]
    fn empty_file_is_all_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.default_poll_minutes, 60);
        assert_eq!(
            c.user_agent(),
            format!("feed-index/{}", crate::FETCHER_VERSION)
        );
    }

    #[test]
    fn rejects_unknown_keys_with_line_number_and_hides_values() {
        let err = Config::parse("concurrency = 2\npassword = \"s3cret\"\n").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("line 2"), "{msg}");
        assert!(!msg.contains("s3cret"), "{msg}");
    }

    #[test]
    fn validates_ranges() {
        for bad in [
            "default_poll_minutes = 4",
            "default_poll_minutes = 1441",
            "concurrency = 0",
            "concurrency = 17",
            "max_feed_bytes = 0",
            "timeout_seconds = 0",
            "fetch_history_days = 0",
            "contact = \"\"",
            "contact = \"a\\nb\"",
        ] {
            assert!(Config::parse(bad).is_err(), "{bad}");
        }
        for ok in [
            "default_poll_minutes = 5",
            "default_poll_minutes = 1440",
            "concurrency = 16",
        ] {
            assert!(Config::parse(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn missing_file_gives_defaults_for_probe() {
        let c = Config::load_or_default(Path::new("/nonexistent/feed-index.toml")).unwrap();
        assert_eq!(c.concurrency, 4);
        assert!(Config::load(Path::new("/nonexistent/feed-index.toml")).is_err());
    }
}
