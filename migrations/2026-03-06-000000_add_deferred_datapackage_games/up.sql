CREATE TABLE deferred_datapackage_games (
    id SERIAL PRIMARY KEY,
    game_name VARCHAR NOT NULL UNIQUE
);
