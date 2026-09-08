-- The app's schema, and the single source of truth for it.
--
-- `App::migrate` runs this, and `petros-sql` prepares every statement in the
-- domain against it at build time — so a column renamed here is a compile error
-- at the call sites that use it, rather than a missing row on a device.
CREATE TABLE IF NOT EXISTS todo (
    id         BLOB PRIMARY KEY NOT NULL,
    text       TEXT NOT NULL,
    done       BOOL NOT NULL DEFAULT 0,
    pos        BIGINT NOT NULL,
    created_ms BIGINT NOT NULL,
    actor      TEXT NOT NULL
);
