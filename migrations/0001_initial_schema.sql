-- crust-seed initial schema.
--
-- This is the *final* shape of cross-seed's knex migration chain (00 through
-- 17) collapsed into one migration. crust-seed does not attempt to open or
-- upgrade an existing cross-seed.db: the chain replayed table renames, column
-- drops and data backfills that only make sense inside knex, and a fresh
-- database is rebuilt from the torrent client / dataDirs on first run anyway.
--
-- Timestamps are milliseconds since the Unix epoch (INTEGER), matching the
-- original's use of Date.now() everywhere.

CREATE TABLE searchee (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    name           TEXT UNIQUE,
    first_searched INTEGER,
    last_searched  INTEGER
);

CREATE TABLE indexer (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    name              TEXT,
    -- Deliberately NOT unique: migration 15 dropped that constraint so the same
    -- Prowlarr URL can be registered twice with different API keys.
    url               TEXT NOT NULL,
    apikey            TEXT,
    trackers          TEXT,
    enabled           BOOLEAN NOT NULL DEFAULT 1,
    status            TEXT,
    retry_after       INTEGER,
    search_cap        BOOLEAN,
    tv_search_cap     BOOLEAN,
    movie_search_cap  BOOLEAN,
    music_search_cap  BOOLEAN,
    audio_search_cap  BOOLEAN,
    book_search_cap   BOOLEAN,
    tv_id_caps        TEXT,
    movie_id_caps     TEXT,
    cat_caps          TEXT,
    limits_caps       TEXT
);

CREATE TABLE decision (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    searchee_id       INTEGER REFERENCES searchee(id),
    guid              TEXT,
    info_hash         TEXT,
    decision          TEXT,
    first_seen        INTEGER,
    last_seen         INTEGER,
    fuzzy_size_factor REAL DEFAULT 0.02,
    UNIQUE (searchee_id, guid)
);

CREATE INDEX idx_decision_info_hash_guid     ON decision (info_hash, guid);
CREATE INDEX idx_decision_info_hash          ON decision (info_hash);
CREATE INDEX idx_decision_guid               ON decision (guid);
CREATE INDEX idx_decision_decision           ON decision (decision);
CREATE INDEX idx_decision_last_seen          ON decision (last_seen);
CREATE INDEX idx_decision_decision_last_seen ON decision (decision, last_seen);

CREATE TABLE torrent (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    info_hash TEXT,
    name      TEXT,
    file_path TEXT UNIQUE
);

CREATE TABLE job_log (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    name     TEXT UNIQUE,
    last_run INTEGER
);

CREATE TABLE timestamp (
    searchee_id    INTEGER REFERENCES searchee(id),
    indexer_id     INTEGER REFERENCES indexer(id),
    first_searched INTEGER,
    last_searched  INTEGER,
    PRIMARY KEY (searchee_id, indexer_id)
);

-- Single-row table (id is pinned to 0). `apikey` is the pre-migration-17
-- location of the API key; it now lives inside settings_json.apiKey and the
-- column is kept only so an operator reading the DB by hand is not surprised.
CREATE TABLE settings (
    id            INTEGER PRIMARY KEY CHECK (id = 0),
    apikey        TEXT,
    settings_json TEXT
);
INSERT INTO settings (id, apikey, settings_json) VALUES (0, NULL, NULL);

CREATE TABLE rss (
    indexer_id     INTEGER PRIMARY KEY REFERENCES indexer(id),
    last_seen_guid TEXT
);

CREATE TABLE client_searchee (
    client_host TEXT,
    info_hash   TEXT,
    name        TEXT,
    title       TEXT,
    files       TEXT,
    length      INTEGER,
    save_path   TEXT,
    category    TEXT,
    tags        TEXT,
    trackers    TEXT,
    PRIMARY KEY (client_host, info_hash)
);
CREATE INDEX idx_client_searchee_info_hash ON client_searchee (info_hash);

CREATE TABLE data (
    path  TEXT PRIMARY KEY,
    title TEXT
);

CREATE TABLE ensemble (
    client_host TEXT,
    path        TEXT,
    info_hash   TEXT,
    ensemble    TEXT,
    element     TEXT,
    PRIMARY KEY (client_host, path)
);
CREATE INDEX idx_ensemble_path      ON ensemble (path);
CREATE INDEX idx_ensemble_info_hash ON ensemble (info_hash);

CREATE TABLE user (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    username   TEXT NOT NULL UNIQUE,
    password   TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE session (
    id         TEXT PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES user(id),
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL
);
