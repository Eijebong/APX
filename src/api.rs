use rocket::{
    Request, State,
    request::{FromRequest, Outcome},
    serde::json::Json,
};
use serde::{Deserialize, Serialize};

use crate::config::AppState;
use crate::lobby::refresh_login_info;

struct ApiKey;

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ApiKey {
    type Error = ();

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let state = req.guard::<&State<AppState>>().await.unwrap();

        match req.headers().get_one("X-Api-Key") {
            Some(key) if key == state.config.apx_api_key => Outcome::Success(ApiKey),
            _ => Outcome::Error((rocket::http::Status::Unauthorized, ())),
        }
    }
}

#[rocket::post("/refresh_passwords")]
async fn refresh_passwords(
    _key: ApiKey,
    state: &State<AppState>,
) -> Result<(), rocket::http::Status> {
    log::info!("Refreshing passwords from lobby API");

    match refresh_login_info(&state.config).await {
        Ok(new_passwords) => {
            let mut passwords = state.passwords.write().await;
            *passwords = new_passwords;
            log::info!("Successfully refreshed passwords");
            Ok(())
        }
        Err(e) => {
            log::error!("Failed to refresh passwords: {:?}", e);
            Err(rocket::http::Status::InternalServerError)
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct ExclusionListResponse {
    excluded_slots: Vec<u32>,
}

#[rocket::get("/deathlink_exclusions")]
async fn get_deathlink_exclusions(
    _key: ApiKey,
    state: &State<AppState>,
) -> Json<ExclusionListResponse> {
    let exclusions = state.deathlink_exclusions.read().await;
    let mut excluded_slots: Vec<u32> = exclusions.iter().copied().collect();
    excluded_slots.sort_unstable();
    Json(ExclusionListResponse { excluded_slots })
}

#[rocket::post("/deathlink_exclusions/<slot>")]
async fn add_deathlink_exclusion(
    _key: ApiKey,
    state: &State<AppState>,
    slot: u32,
) -> rocket::http::Status {
    let mut exclusions = state.deathlink_exclusions.write().await;
    let newly_added = exclusions.insert(slot);

    if newly_added {
        log::info!("Added slot {} to deathlink exclusion list", slot);
        rocket::http::Status::Created
    } else {
        log::debug!("Slot {} was already in deathlink exclusion list", slot);
        rocket::http::Status::Ok
    }
}

#[rocket::delete("/deathlink_exclusions/<slot>")]
async fn remove_deathlink_exclusion(
    _key: ApiKey,
    state: &State<AppState>,
    slot: u32,
) -> rocket::http::Status {
    let mut exclusions = state.deathlink_exclusions.write().await;
    let was_present = exclusions.remove(&slot);

    if was_present {
        log::info!("Removed slot {} from deathlink exclusion list", slot);
        rocket::http::Status::Ok
    } else {
        log::debug!("Slot {} was not in deathlink exclusion list", slot);
        rocket::http::Status::NotFound
    }
}

#[rocket::get("/deathlinks/<room_id>")]
async fn get_room_deathlinks(
    _key: ApiKey,
    state: &State<AppState>,
    room_id: &str,
) -> Result<Json<Vec<crate::db::models::DeathLink>>, rocket::http::Status> {
    match crate::db::models::get_room_deathlinks(&state.db_pool, room_id).await {
        Ok(deathlinks) => Ok(Json(deathlinks)),
        Err(e) => {
            log::error!("Failed to get deathlinks for room {}: {:?}", room_id, e);
            Err(rocket::http::Status::InternalServerError)
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct ProbabilityResponse {
    probability: f64,
}

#[rocket::get("/deathlink_probability")]
async fn get_deathlink_probability(
    _key: ApiKey,
    state: &State<AppState>,
) -> Json<ProbabilityResponse> {
    let probability = state.deathlink_probability.get();
    Json(ProbabilityResponse { probability })
}

#[derive(Deserialize)]
pub struct SetProbabilityRequest {
    probability: f64,
}

#[rocket::put("/deathlink_probability", data = "<request>")]
async fn set_deathlink_probability(
    _key: ApiKey,
    state: &State<AppState>,
    request: Json<SetProbabilityRequest>,
) -> Json<ProbabilityResponse> {
    let normalized = request.probability / 100.0;

    let actual = state.deathlink_probability.set(normalized);
    log::info!("DeathLink probability set to {:.2}%", actual * 100.0);
    Json(ProbabilityResponse {
        probability: actual,
    })
}

pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![
        refresh_passwords,
        get_deathlink_exclusions,
        add_deathlink_exclusion,
        remove_deathlink_exclusion,
        get_room_deathlinks,
        get_deathlink_probability,
        set_deathlink_probability,
    ]
}

#[derive(Clone)]
pub struct MetricsRoute(pub rocket_prometheus::PrometheusMetrics);

#[rocket::async_trait]
impl rocket::route::Handler for MetricsRoute {
    async fn handle<'r>(
        &self,
        req: &'r rocket::Request<'_>,
        data: rocket::Data<'r>,
    ) -> rocket::route::Outcome<'r> {
        let rocket::outcome::Outcome::Success(_api_key) = req.guard::<ApiKey>().await else {
            return rocket::route::Outcome::Error(rocket::http::Status::Unauthorized);
        };

        self.0.handle(req, data).await
    }
}

impl From<MetricsRoute> for Vec<rocket::Route> {
    fn from(val: MetricsRoute) -> Self {
        vec![rocket::Route::new(rocket::http::Method::Get, "/", val)]
    }
}
