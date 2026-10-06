# Deploying feed-index

The package installs the binary and two systemd units. It enables nothing and installs no
configuration into `/etc`: you do that, once, by hand.

## 1. Install

    dpkg -i feed-index_*.deb

This creates the system user and group `feed-index` (if absent) and registers
`feed-index-fetch.service` and `feed-index-fetch.timer`, disabled.

## 2. Configure

    install -d -m 0750 -o root -g feed-index /etc/feed-index
    cp /usr/share/doc/feed-index/config.example.toml /etc/feed-index/config.toml
    cp /usr/share/doc/feed-index/feeds.example.opml  /etc/feed-index/feeds.opml
    cp /usr/share/doc/feed-index/default.example     /etc/default/feed-index

Edit `/etc/feed-index/config.toml` (at least `contact`, which goes into the User-Agent so that
publishers can reach you), replace `/etc/feed-index/feeds.opml` with your feed list (OPML 2.0,
as exported by most feed readers; `poll_minutes` and `weight` are optional attributes), and set
the `PG*` variables in `/etc/default/feed-index`.

Create the pgpass file named by `PGPASSFILE`, owned by the service user and mode 0600 (it is
ignored otherwise):

    install -m 0600 -o feed-index -g feed-index /dev/null /etc/feed-index/pgpass
    # one line: host:port:database:user:password   (escape ':' and '\' with '\')

The database role needs to create the schema `feeds` on first `migrate` (or own it, if you
create it beforehand). Everything the program writes, including its migration record
(`feeds._sqlx_migrations`), lives in that schema, so it can share a database with other
sqlx-migrated programs such as maildir-index.

## 3. Check the feed list, without a database

    feed-index probe --opml /etc/feed-index/feeds.opml
    feed-index probe --opml /etc/feed-index/feeds.opml --show | less

`probe` fetches and parses every feed exactly as `fetch` would and prints, per feed, the
outcome, HTTP status, final URL (after redirects), declared title, entry count, how many
entries have ids versus fingerprints, and the newest publication date. `--show` adds the first
three entries' titles and text. Fix or remove feeds that fail, or that redirect somewhere you
would rather name directly. It exits non-zero if any feed did not end in `ok`.

## 4. First run, by hand

Run as the service user with the same environment the unit uses:

    run() { sudo -u feed-index sh -c 'set -a; . /etc/default/feed-index; exec "$@"' sh "$@"; }
    run feed-index migrate
    run feed-index sync
    run feed-index fetch
    run feed-index status

`sync` prints `added=… updated=… retired=…`. The first `fetch` fetches every feed (new feeds
are due at once) and marks what it finds as back catalogue (`backfill = true`). It prints one
summary line, plus one line per feed that failed. `status` lists every active feed with its last
success, last new entry, failure count and last outcome, and marks failing and quiet feeds.

## 5. Enable the timer

Only once the manual run and `status` look right:

    systemctl enable --now feed-index-fetch.timer
    journalctl -u feed-index-fetch.service

The timer fires every five minutes; each run fetches only the feeds that are due, so a feed
with `poll_minutes="10"` and one with the hourly default share it. Runs never overlap: a run
that finds another one still going logs one line and exits 0.

## Upgrades

`prerm` stops the timer and the service, and `postinst` starts the timer again **if it is
enabled**. To keep fetching paused across an upgrade, disable it first:

    systemctl disable --now feed-index-fetch.timer
    dpkg -i feed-index_*.deb
    # ... later
    systemctl enable --now feed-index-fetch.timer

## Site adjustments

Change the user or the schedule with a drop-in rather than editing the unit:

    systemctl edit feed-index-fetch.service

    [Service]
    User=feedreader
    Group=feedreader

    systemctl edit feed-index-fetch.timer

    [Timer]
    OnCalendar=
    OnCalendar=*:0/10

(An empty `OnCalendar=` clears the packaged schedule before setting a new one.) A different
user needs read access to `/etc/feed-index/config.toml`, the OPML file and the pgpass file
(and the pgpass file must still be mode 0600 and owned by that user).

## Exit status

`fetch` exits non-zero only for run-level failures: configuration or OPML errors, the retire
guard (the feed list is empty or holds fewer than half of the active feeds; nothing is retired
or changed), database errors. A feed that fails is normal for a fetcher: it is recorded on the
feed (`consecutive_failures`, `last_error`, `feeds.fetches`), backed off, reported on stdout and
by `status`, and does not fail the unit.
