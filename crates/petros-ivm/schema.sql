-- The schema this crate's tests generate row types from.
CREATE TABLE IF NOT EXISTS song (
    id BLOB NOT NULL, title TEXT NOT NULL, done BOOL NOT NULL DEFAULT 0,
    pos BIGINT NOT NULL, PRIMARY KEY (id));
CREATE TABLE IF NOT EXISTS other (
    id BLOB NOT NULL, pos BIGINT NOT NULL, PRIMARY KEY (id));
