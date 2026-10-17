-- Pending IDs are independent of novels: a failed publication has no novel row.
CREATE TABLE IF NOT EXISTS novel_id_reservations (
    id INTEGER PRIMARY KEY CHECK (id >= 0),
    generation INTEGER NOT NULL UNIQUE CHECK (typeof(generation) = 'integer' AND generation >= 0),
    available INTEGER NOT NULL DEFAULT 0 CHECK (available IN (0, 1))
);
CREATE INDEX IF NOT EXISTS novel_id_reservations_available
    ON novel_id_reservations(available, id);
