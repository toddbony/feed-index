-- feed-index schema v1
--
-- Indexes RSS/Atom feeds into Postgres: one row per feed, one row per entry, and a record
-- of every fetch. Everything lives in the schema "feeds", including sqlx's migration
-- record (feeds._sqlx_migrations), so this program can share a database with other
-- sqlx-migrated programs without either one's migrator seeing the other's history.
--
-- Every object is schema-qualified here and in the code; nothing depends on search_path.
--
-- This file creates structure only. Feed rows are configuration: the fetcher upserts them
-- from the OPML file named in its config, never from a migration.

CREATE TABLE feeds.feeds (
    feed_id               integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    xml_url               text NOT NULL UNIQUE,
    title                 text NOT NULL,
    folder                text,
    html_url              text,
    poll_minutes          integer NOT NULL CHECK (poll_minutes BETWEEN 5 AND 1440),
    weight                integer NOT NULL DEFAULT 0,
    retired_at            timestamptz,
    declared_title        text,
    resolved_url          text,
    etag                  text,
    last_modified         text,
    next_fetch_at         timestamptz NOT NULL DEFAULT now(),
    last_attempt_at       timestamptz,
    last_ok_at            timestamptz,
    last_new_entry_at     timestamptz,
    consecutive_failures  integer NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    last_error            text,
    created_at            timestamptz NOT NULL DEFAULT now()
);
COMMENT ON TABLE  feeds.feeds                IS 'One per feed in the OPML config. Never deleted: a feed removed from the config gets retired_at.';
COMMENT ON COLUMN feeds.feeds.xml_url        IS 'Feed URL exactly as configured (OPML xmlUrl). The identity of the feed.';
COMMENT ON COLUMN feeds.feeds.title          IS 'Title from the config (OPML text/title), not from the feed.';
COMMENT ON COLUMN feeds.feeds.folder         IS 'Nearest enclosing OPML outline (e.g. "News"). NULL at top level.';
COMMENT ON COLUMN feeds.feeds.poll_minutes   IS 'Minimum minutes between fetches. OPML attribute poll_minutes, else the config default.';
COMMENT ON COLUMN feeds.feeds.weight         IS 'Ranking hint from the config (OPML attribute weight). Not used by the fetcher.';
COMMENT ON COLUMN feeds.feeds.retired_at     IS 'Set when the feed disappears from the config; cleared if it comes back. Retired feeds are not fetched; their entries stay.';
COMMENT ON COLUMN feeds.feeds.declared_title IS 'Title the feed document itself declares, from the last successful parse.';
COMMENT ON COLUMN feeds.feeds.resolved_url   IS 'Where the last fetch ended up after redirects, when different from xml_url. Informational: xml_url is always what is requested.';
COMMENT ON COLUMN feeds.feeds.etag           IS 'ETag from the last 200 response, sent back as If-None-Match.';
COMMENT ON COLUMN feeds.feeds.last_modified  IS 'Last-Modified from the last 200 response, as written, sent back as If-Modified-Since.';
COMMENT ON COLUMN feeds.feeds.next_fetch_at  IS 'Not fetched before this time (poll interval, failure backoff, or Retry-After).';
COMMENT ON COLUMN feeds.feeds.last_ok_at     IS 'Last fetch that ended in 200 + parsed, or 304.';
COMMENT ON COLUMN feeds.feeds.last_new_entry_at IS 'Last time a fetch of this feed produced a new entry. A feed that is "ok" but silent for far longer than usual has probably moved.';

CREATE TABLE feeds.entries (
    entry_id          bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    feed_id           integer NOT NULL REFERENCES feeds.feeds,
    entry_key         text NOT NULL,
    key_source        text NOT NULL CHECK (key_source IN ('id', 'fingerprint')),
    link              text,
    title             text,
    author            text,
    published_at      timestamptz,
    updated_at        timestamptz,
    summary_html      text,
    content_html      text,
    body_text         text,
    body_source       text NOT NULL CHECK (body_source IN ('content', 'summary', 'none')),
    content_sha256    bytea NOT NULL CHECK (octet_length(content_sha256) = 32),
    revisions         integer NOT NULL DEFAULT 0 CHECK (revisions >= 0),
    backfill          boolean NOT NULL DEFAULT false,
    first_seen_at     timestamptz NOT NULL DEFAULT now(),
    last_changed_at   timestamptz,
    fetcher_version   text NOT NULL,
    UNIQUE (feed_id, entry_key)
);
COMMENT ON TABLE  feeds.entries                 IS 'One row per entry (RSS item / Atom entry) per feed. Feeds drop old entries, so this is the only lasting copy.';
COMMENT ON COLUMN feeds.entries.entry_key       IS 'The entry''s id (RSS guid / Atom id) as written, trimmed; or, when it has none, a fingerprint "fp:<hex sha256>" of the normalised link and title.';
COMMENT ON COLUMN feeds.entries.published_at    IS 'Publication time the feed declares. NULL when absent or unparseable; order by coalesce(published_at, first_seen_at).';
COMMENT ON COLUMN feeds.entries.updated_at      IS 'Update time the feed declares (Atom updated), if any.';
COMMENT ON COLUMN feeds.entries.summary_html    IS 'Summary/description exactly as the feed gave it (usually HTML). Untrusted: sanitise before display.';
COMMENT ON COLUMN feeds.entries.content_html    IS 'Full content exactly as the feed gave it (content:encoded / Atom content). Untrusted: sanitise before display.';
COMMENT ON COLUMN feeds.entries.body_text       IS 'Plain text for search and blurbs: content, else summary, rendered with html2text and normalised. NULL when neither has text.';
COMMENT ON COLUMN feeds.entries.content_sha256  IS 'SHA-256 over title, link, summary_html and content_html. A change means the publisher edited the entry.';
COMMENT ON COLUMN feeds.entries.revisions       IS 'How many times the publisher changed the entry after first sight. Fields hold the latest version; earlier versions are not kept.';
COMMENT ON COLUMN feeds.entries.backfill        IS 'True for entries found on a feed''s first successful fetch: the feed''s back catalogue, not news.';

CREATE INDEX entries_first_seen_idx ON feeds.entries (first_seen_at);
CREATE INDEX entries_published_idx  ON feeds.entries (published_at);
CREATE INDEX entries_feed_seen_idx  ON feeds.entries (feed_id, first_seen_at DESC);
-- Hash, not btree: links come from feeds and can be arbitrarily long, and a btree index row
-- over ~2.7 kB is an error that would fail the whole fetch. Joins by link need equality only.
CREATE INDEX entries_link_idx       ON feeds.entries USING hash (link) WHERE link IS NOT NULL;

CREATE TABLE feeds.fetch_runs (
    run_id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    started_at       timestamptz NOT NULL DEFAULT now(),
    finished_at      timestamptz,
    feeds_due        integer NOT NULL DEFAULT 0,
    ok               integer NOT NULL DEFAULT 0,
    not_modified     integer NOT NULL DEFAULT 0,
    failed           integer NOT NULL DEFAULT 0,
    new_entries      integer NOT NULL DEFAULT 0,
    changed_entries  integer NOT NULL DEFAULT 0,
    fetcher_version  text NOT NULL
);
COMMENT ON TABLE feeds.fetch_runs IS 'One row per fetch pass. finished_at NULL = the run did not complete.';

CREATE TABLE feeds.fetches (
    fetch_id         bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    run_id           bigint NOT NULL REFERENCES feeds.fetch_runs ON DELETE CASCADE,
    feed_id          integer NOT NULL REFERENCES feeds.feeds,
    started_at       timestamptz NOT NULL DEFAULT now(),
    duration_ms      integer NOT NULL CHECK (duration_ms >= 0),
    outcome          text NOT NULL CHECK (outcome IN
                       ('ok', 'not_modified', 'http_error', 'network_error', 'parse_error', 'too_large')),
    http_status      smallint,
    bytes            integer,
    entries_in_doc   integer,
    new_entries      integer NOT NULL DEFAULT 0,
    changed_entries  integer NOT NULL DEFAULT 0,
    error            text
);
COMMENT ON TABLE  feeds.fetches       IS 'One row per attempt to fetch one feed. Pruned: the fetcher deletes rows older than its retention (default 90 days).';
COMMENT ON COLUMN feeds.fetches.error IS 'Short error description. Never the response body.';
CREATE INDEX fetches_feed_idx ON feeds.fetches (feed_id, started_at DESC);
CREATE INDEX fetches_run_idx  ON feeds.fetches (run_id);
