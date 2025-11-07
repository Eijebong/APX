CREATE TABLE deathlinks (
    id SERIAL PRIMARY KEY,
    room_id VARCHAR NOT NULL,
    slot INTEGER NOT NULL,
    source VARCHAR NOT NULL,
    cause VARCHAR,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

CREATE TABLE countdowns (
    id SERIAL PRIMARY KEY,
    room_id VARCHAR NOT NULL,
    slot INTEGER NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_deathlinks_room_id ON deathlinks(room_id);
CREATE INDEX idx_countdowns_room_id ON countdowns(room_id);
