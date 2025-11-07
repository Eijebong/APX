use chrono::NaiveDateTime;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = super::schema::deathlinks)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct DeathLink {
    pub id: i32,
    pub room_id: String,
    pub slot: i32,
    pub source: String,
    pub cause: Option<String>,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = super::schema::deathlinks)]
pub struct NewDeathLink {
    pub room_id: String,
    pub slot: i32,
    pub source: String,
    pub cause: Option<String>,
}

impl NewDeathLink {
    pub fn new(room_id: String, slot: u32, source: String, cause: Option<String>) -> Self {
        Self {
            room_id,
            slot: slot as i32,
            source,
            cause,
        }
    }
}

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = super::schema::countdowns)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct Countdown {
    pub id: i32,
    pub room_id: String,
    pub slot: i32,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = super::schema::countdowns)]
pub struct NewCountdown {
    pub room_id: String,
    pub slot: i32,
}

impl NewCountdown {
    pub fn new(room_id: String, slot: u32) -> Self {
        Self {
            room_id,
            slot: slot as i32,
        }
    }
}

pub async fn insert_deathlink(
    pool: &crate::db::DieselPool,
    new_deathlink: NewDeathLink,
) -> anyhow::Result<DeathLink> {
    use super::schema::deathlinks;

    let mut conn = pool.get().await?;

    let deathlink = diesel::insert_into(deathlinks::table)
        .values(&new_deathlink)
        .get_result(&mut conn)
        .await?;

    Ok(deathlink)
}

pub async fn insert_countdown(
    pool: &crate::db::DieselPool,
    new_countdown: NewCountdown,
) -> anyhow::Result<Countdown> {
    use super::schema::countdowns;

    let mut conn = pool.get().await?;

    let countdown = diesel::insert_into(countdowns::table)
        .values(&new_countdown)
        .get_result(&mut conn)
        .await?;

    Ok(countdown)
}

pub async fn get_room_deathlinks(
    pool: &crate::db::DieselPool,
    room_id: &str,
) -> anyhow::Result<Vec<DeathLink>> {
    use super::schema::deathlinks::dsl;

    let mut conn = pool.get().await?;

    let deathlinks = dsl::deathlinks
        .filter(dsl::room_id.eq(room_id))
        .order(dsl::created_at.desc())
        .load::<DeathLink>(&mut conn)
        .await?;

    Ok(deathlinks)
}

pub async fn get_room_countdowns(
    pool: &crate::db::DieselPool,
    room_id: &str,
) -> anyhow::Result<Vec<Countdown>> {
    use super::schema::countdowns::dsl;

    let mut conn = pool.get().await?;

    let countdowns = dsl::countdowns
        .filter(dsl::room_id.eq(room_id))
        .order(dsl::created_at.desc())
        .load::<Countdown>(&mut conn)
        .await?;

    Ok(countdowns)
}
