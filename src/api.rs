use rocket::{
    Request, State,
    request::{FromRequest, Outcome},
};

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

pub fn routes() -> Vec<rocket::Route> {
    rocket::routes![refresh_passwords]
}
