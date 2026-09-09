-- The schema the macro checks against, for this crate's own tests.
-- An app keeps one of these beside its Cargo.toml and holds it to its
-- `tables!` declaration with a one-line test.
CREATE TABLE IF NOT EXISTS song (
    id BLOB NOT NULL, title TEXT NOT NULL, artist TEXT NOT NULL,
    pos BIGINT NOT NULL, PRIMARY KEY (id));
-- The REFERENCES is what generates `Song::favorite` and `Favorite::song`.
CREATE TABLE IF NOT EXISTS favorite (
    song_id BLOB NOT NULL REFERENCES song(id), pos BIGINT NOT NULL,
    favorited_ms BIGINT NOT NULL, actor TEXT NOT NULL, PRIMARY KEY (song_id));
-- A nullable column, because they are ordinary and used to generate a type that
-- could not hold a NULL. `notes` is absent for most songs.
CREATE TABLE IF NOT EXISTS sleeve (
    song_id BLOB NOT NULL REFERENCES song(id), notes TEXT, year BIGINT,
    PRIMARY KEY (song_id));
