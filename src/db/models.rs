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

#[derive(Debug, Clone, Queryable, Selectable, Serialize, Deserialize)]
#[diesel(table_name = super::schema::deathlink_exclusions)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct DeathlinkExclusion {
    pub id: i32,
    pub room_id: String,
    pub slot: i32,
}

#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = super::schema::deathlink_exclusions)]
pub struct NewDeathlinkExclusion {
    pub room_id: String,
    pub slot: i32,
}

pub async fn get_room_deathlink_exclusions(
    pool: &crate::db::DieselPool,
    room_id: &str,
) -> anyhow::Result<Vec<u32>> {
    use super::schema::deathlink_exclusions::dsl;

    let mut conn = pool.get().await?;

    let exclusions: Vec<DeathlinkExclusion> = dsl::deathlink_exclusions
        .filter(dsl::room_id.eq(room_id))
        .load(&mut conn)
        .await?;

    Ok(exclusions.into_iter().map(|e| e.slot as u32).collect())
}

pub async fn add_deathlink_exclusion(
    pool: &crate::db::DieselPool,
    room_id: &str,
    slot: u32,
) -> anyhow::Result<bool> {
    use super::schema::deathlink_exclusions::dsl;

    let mut conn = pool.get().await?;

    let new_exclusion = NewDeathlinkExclusion {
        room_id: room_id.to_string(),
        slot: slot as i32,
    };

    let result = diesel::insert_into(dsl::deathlink_exclusions)
        .values(&new_exclusion)
        .on_conflict_do_nothing()
        .execute(&mut conn)
        .await?;

    Ok(result > 0)
}

pub async fn remove_deathlink_exclusion(
    pool: &crate::db::DieselPool,
    room_id: &str,
    slot: u32,
) -> anyhow::Result<bool> {
    use super::schema::deathlink_exclusions::dsl;

    let mut conn = pool.get().await?;

    let result = diesel::delete(
        dsl::deathlink_exclusions
            .filter(dsl::room_id.eq(room_id))
            .filter(dsl::slot.eq(slot as i32)),
    )
    .execute(&mut conn)
    .await?;

    Ok(result > 0)
}

#[derive(Debug, Clone, Queryable, Selectable, Insertable, Serialize, Deserialize)]
#[diesel(table_name = super::schema::deathlink_settings)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct DeathlinkSettings {
    pub room_id: String,
    pub probability: f64,
}

pub async fn get_deathlink_settings(
    pool: &crate::db::DieselPool,
    room_id: &str,
) -> anyhow::Result<Option<DeathlinkSettings>> {
    use super::schema::deathlink_settings::dsl;

    let mut conn = pool.get().await?;

    let settings = dsl::deathlink_settings
        .filter(dsl::room_id.eq(room_id))
        .first::<DeathlinkSettings>(&mut conn)
        .await
        .optional()?;

    Ok(settings)
}

pub async fn set_deathlink_probability(
    pool: &crate::db::DieselPool,
    room_id: &str,
    probability: f64,
) -> anyhow::Result<f64> {
    use super::schema::deathlink_settings::dsl;

    let mut conn = pool.get().await?;

    let clamped = probability.clamp(0.0, 1.0);

    let settings = DeathlinkSettings {
        room_id: room_id.to_string(),
        probability: clamped,
    };

    diesel::insert_into(dsl::deathlink_settings)
        .values(&settings)
        .on_conflict(dsl::room_id)
        .do_update()
        .set(dsl::probability.eq(clamped))
        .execute(&mut conn)
        .await?;

    Ok(clamped)
}
