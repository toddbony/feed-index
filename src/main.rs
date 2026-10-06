use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use feed_index::config::{self, Config};
use feed_index::{db, fetch, opml, probe, status};

/// Poll RSS/Atom feeds listed in an OPML file and index their entries into PostgreSQL.
///
/// Database settings come from the libpq environment only (PGHOST, PGPORT, PGDATABASE,
/// PGUSER, PGSSLMODE, and PGPASSFILE for the password).
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Configuration file [default: /etc/feed-index/config.toml].
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply the embedded schema migrations and exit.
    Migrate,
    /// Migrate, then sync the OPML feed list into feeds.feeds.
    Sync,
    /// Migrate, sync, and fetch the feeds that are due.
    Fetch {
        /// Only this feed id (repeatable).
        #[arg(long = "feed", value_name = "ID")]
        feeds: Vec<i32>,
        /// Fetch even feeds that are not due yet.
        #[arg(long)]
        force: bool,
    },
    /// One line per active feed: last success, last new entry, failures, entries.
    Status {
        /// Mark feeds with no new entry for this many days.
        #[arg(long, default_value_t = 14, value_parser = clap::value_parser!(i32).range(1..))]
        quiet_days: i32,
    },
    /// Without a database: fetch and parse feeds as `fetch` would, and print what was found.
    Probe {
        /// Feed URLs.
        #[arg(required_unless_present = "opml", conflicts_with = "opml")]
        urls: Vec<String>,
        /// Probe every feed in this OPML file instead.
        #[arg(long, value_name = "FILE")]
        opml: Option<PathBuf>,
        /// Also print the first entries' titles and text (for human review).
        #[arg(long)]
        show: bool,
    },
}

/// Appended after RUST_LOG, so they win whatever the operator asks for.
const FORCED_DIRECTIVES: &[&str] = &[
    // Parsers and renderers that see feed content may log it (html5ever traces every token).
    "html5ever=off",
    "markup5ever=off",
    "html2text=off",
    "feed_rs=off",
    "quick_xml=off",
    // The HTTP stack logs full request URLs.
    "reqwest=off",
    "hyper=off",
    "hyper_util=off",
    "h2=off",
    "tower_http=off",
    "rustls=off",
    // sqlx logs malformed pgpass lines verbatim, which could expose a password.
    "sqlx_postgres::options::pgpass=error",
];

fn init_logging() {
    let mut filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,sqlx::postgres::notice=warn"));
    for d in FORCED_DIRECTIVES {
        filter = filter.add_directive(d.parse().expect("static directive"));
    }
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    // A panic payload can quote document text (e.g. a string-slicing panic); log the location
    // only.
    std::panic::set_hook(Box::new(|info| {
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        tracing::error!("panic at {at} (message suppressed)");
    }));
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging();
    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|rt| rt.block_on(run(cli.command, cli.config.as_deref())));
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            // `{:#}` prints the context chain via Display; Debug could include server DETAIL.
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Returns Ok(false) when the command ran but must report failure.
async fn run(command: Command, explicit_config: Option<&Path>) -> Result<bool> {
    let config_path = explicit_config.unwrap_or(Path::new(config::DEFAULT_PATH));
    match command {
        Command::Migrate => {
            let options = db::connect_options()?;
            let pool = db::connect(&options, 2).await?;
            db::migrate(&pool).await?;
            tracing::info!("migrations applied");
            Ok(true)
        }
        Command::Sync => {
            // Configuration and feed list are read in full before anything is changed.
            let config = Config::load(config_path)?;
            let feeds = opml::load(&config.opml, config.default_poll_minutes)?;
            let options = db::connect_options()?;
            let pool = db::connect(&options, 2).await?;
            let report = fetch::sync_command(&options, &pool, &feeds).await?;
            println!("{}", report.summary_line());
            Ok(true)
        }
        Command::Fetch { feeds: ids, force } => {
            let config = Config::load(config_path)?;
            let feeds = opml::load(&config.opml, config.default_poll_minutes)?;
            let options = db::connect_options()?;
            let pool = db::connect(&options, config.concurrency as u32 + 2).await?;
            let fetch_options = fetch::FetchOptions {
                force,
                feed_ids: ids,
            };
            match fetch::run(&options, &pool, &config, &feeds, &fetch_options).await? {
                None => Ok(true),
                Some(report) => {
                    for line in report.summary_lines() {
                        println!("{line}");
                    }
                    Ok(report.errors == 0)
                }
            }
        }
        Command::Status { quiet_days } => {
            let options = db::connect_options()?;
            let pool = db::connect(&options, 2).await?;
            let rows = status::feeds(&pool, quiet_days).await?;
            print!("{}", status::render(&rows, quiet_days));
            Ok(true)
        }
        Command::Probe { urls, opml, show } => {
            // Works without configuration: the defaults, unless a file exists or is named.
            let config = match explicit_config {
                Some(path) => Config::load(path)?,
                None => Config::load_or_default(config_path)?,
            };
            let urls = match opml {
                Some(path) => opml::load(&path, config.default_poll_minutes)?
                    .into_iter()
                    .map(|f| f.xml_url)
                    .collect(),
                None => urls,
            };
            probe::run(&config, urls, show).await
        }
    }
}
