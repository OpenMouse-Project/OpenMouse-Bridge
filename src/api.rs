use std::time::Duration;

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderValue, Method, StatusCode},
    routing::{get, put},
};
use serde::{Deserialize, Serialize};
use tower_http::{cors::CorsLayer, set_header::SetResponseHeaderLayer, trace::TraceLayer};

use crate::{
    config::{ApplicationProfile, GameConfig},
    platform,
    service::{BatteryReading, BridgeService},
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiResult {
    ok: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GamesPayload {
    games: Vec<GameConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AutostartPayload {
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfilesPayload {
    profiles: Vec<ApplicationProfile>,
}

pub fn router(service: BridgeService, origins: &[String]) -> Router {
    let allowed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(allowed)
        .allow_methods([Method::GET, Method::PUT])
        .allow_headers([axum::http::header::CONTENT_TYPE])
        .max_age(Duration::from_secs(3600));
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/games", put(replace_games))
        .route("/v1/applications", get(applications))
        .route("/v1/profiles", get(profiles).put(replace_profiles))
        .route("/v1/battery", put(record_battery))
        .route("/v1/autostart", put(set_autostart))
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::HeaderName::from_static("access-control-allow-private-network"),
            HeaderValue::from_static("true"),
        ))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(service)
}

async fn status(State(service): State<BridgeService>) -> Json<crate::service::BridgeSnapshot> {
    Json(service.snapshot().await)
}

async fn applications(
    State(service): State<BridgeService>,
) -> Json<Vec<crate::applications::ApplicationInfo>> {
    Json(service.applications().await)
}

async fn profiles(State(service): State<BridgeService>) -> Json<Vec<ApplicationProfile>> {
    Json(service.profiles().await)
}

async fn replace_profiles(
    State(service): State<BridgeService>,
    Json(payload): Json<ProfilesPayload>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    service
        .replace_profiles(payload.profiles)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiResult { ok: true }))
}

async fn replace_games(
    State(service): State<BridgeService>,
    Json(payload): Json<GamesPayload>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    service
        .replace_games(payload.games)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiResult { ok: true }))
}

async fn record_battery(
    State(service): State<BridgeService>,
    Json(reading): Json<BatteryReading>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    service
        .record_battery(reading)
        .await
        .map_err(internal_error)?;
    Ok(Json(ApiResult { ok: true }))
}

async fn set_autostart(
    Json(payload): Json<AutostartPayload>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    platform::set_autostart(payload.enabled).map_err(internal_error)?;
    Ok(Json(ApiResult { ok: true }))
}

fn internal_error(error: anyhow::Error) -> (StatusCode, String) {
    tracing::error!(%error, "Bridge request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}
