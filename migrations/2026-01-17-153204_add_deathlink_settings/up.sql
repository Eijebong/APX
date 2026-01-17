CREATE TABLE deathlink_exclusions (
    id SERIAL PRIMARY KEY,
    room_id VARCHAR NOT NULL,
    slot INTEGER NOT NULL,
    UNIQUE(room_id, slot)
);

CREATE INDEX idx_deathlink_exclusions_room_id ON deathlink_exclusions(room_id);

CREATE TABLE deathlink_settings (
    room_id VARCHAR PRIMARY KEY,
    probability DOUBLE PRECISION NOT NULL DEFAULT 1.0
);
