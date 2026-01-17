diesel::table! {
    deathlinks (id) {
        id -> Int4,
        room_id -> Varchar,
        slot -> Int4,
        source -> Varchar,
        cause -> Nullable<Varchar>,
        created_at -> Timestamp,
    }
}

diesel::table! {
    countdowns (id) {
        id -> Int4,
        room_id -> Varchar,
        slot -> Int4,
        created_at -> Timestamp,
    }
}

diesel::table! {
    deathlink_exclusions (id) {
        id -> Int4,
        room_id -> Varchar,
        slot -> Int4,
    }
}

diesel::table! {
    deathlink_settings (room_id) {
        room_id -> Varchar,
        probability -> Float8,
    }
}
