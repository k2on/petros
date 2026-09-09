-- The schema this crate's tests generate row types from.
CREATE TABLE IF NOT EXISTS song (
    id BLOB NOT NULL, title TEXT NOT NULL, done BOOL NOT NULL DEFAULT 0,
    pos BIGINT NOT NULL, PRIMARY KEY (id));
-- The REFERENCES generates `Song::favorite` and `Favorite::song`, which is what
-- the join operator is built over.
CREATE TABLE IF NOT EXISTS favorite (
    song_id BLOB NOT NULL REFERENCES song(id), pos BIGINT NOT NULL,
    PRIMARY KEY (song_id));
CREATE TABLE IF NOT EXISTS other (
    id BLOB NOT NULL, pos BIGINT NOT NULL, PRIMARY KEY (id));
-- A child whose link to its parent is *not* its primary key, so it can be
-- edited from one parent onto another — which a favourite cannot, since moving
-- one is a delete and an insert. The join treats the two differently and only
-- this shape can tell them apart.
CREATE TABLE IF NOT EXISTS note (
    id BLOB NOT NULL, song_id BLOB NOT NULL REFERENCES song(id),
    text TEXT NOT NULL, PRIMARY KEY (id));
