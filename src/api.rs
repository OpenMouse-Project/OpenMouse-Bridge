use std::time::Duration;

use axum::{
    Json, Router,
    body::Body,
    extract::{FromRef, Path, State},
    http::{HeaderValue, Method, Response, StatusCode, header},
    routing::{get, put},
};
use serde::{Deserialize, Serialize};
use tower_http::{cors::CorsLayer, trace::TraceLayer};

use crate::{
    config::{ApplicationProfile, GameConfig},
    devices::{DeviceInfo, DeviceManager},
    platform,
    service::{BatteryReading, BridgeService},
};

/// Everything the HTTP handlers share. `FromRef` lets existing handlers keep
/// extracting `State<BridgeService>` unchanged while new ones reach the
/// device manager.
#[derive(Clone)]
pub struct AppState {
    service: BridgeService,
    devices: Option<DeviceManager>,
}

impl FromRef<AppState> for BridgeService {
    fn from_ref(state: &AppState) -> Self {
        state.service.clone()
    }
}

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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PollingPayload {
    hz: u16,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PollingResult {
    ok: bool,
    polling_rate_hz: u16,
}

pub fn router(
    service: BridgeService,
    devices: Option<DeviceManager>,
    origins: &[String],
) -> Router {
    let allowed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();
    // allow_private_network puts `Access-Control-Allow-Private-Network: true`
    // on the CORS preflight itself. A public HTTPS origin (the deployed site)
    // reaching this loopback server is a private-network request, and the
    // browser only accepts it when that header is on the *preflight* response.
    // A separate response-header layer cannot do this: CorsLayer answers the
    // preflight OPTIONS directly and never calls inner layers.
    let cors = CorsLayer::new()
        .allow_origin(allowed)
        .allow_methods([Method::GET, Method::PUT])
        .allow_headers([axum::http::header::CONTENT_TYPE])
        .allow_private_network(true)
        .max_age(Duration::from_secs(3600));
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/handshake", put(handshake))
        .route("/v1/games", get(games).put(replace_games))
        .route("/v1/applications", get(applications))
        .route("/v1/applications/{icon_id}/icon", get(application_icon))
        .route("/v1/profiles", get(profiles).put(replace_profiles))
        .route("/v1/default-profile", put(set_default_profile))
        .route("/v1/battery", put(record_battery))
        .route("/v1/autostart", put(set_autostart))
        .route("/v1/devices", get(list_devices))
        .route("/v1/devices/{id}/polling", put(set_device_polling))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(AppState { service, devices })
}

async fn list_devices(State(state): State<AppState>) -> Json<Vec<DeviceInfo>> {
    match state.devices {
        Some(manager) => Json(manager.list().await),
        None => Json(Vec::new()),
    }
}

async fn set_device_polling(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<PollingPayload>,
) -> Result<Json<PollingResult>, (StatusCode, String)> {
    let manager = state
        .devices
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "native device support is unavailable".to_owned()))?;
    let confirmed = manager
        .set_polling(id, payload.hz)
        .await
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    Ok(Json(PollingResult {
        ok: true,
        polling_rate_hz: confirmed,
    }))
}

async fn status(State(service): State<BridgeService>) -> Json<crate::service::BridgeSnapshot> {
    Json(service.snapshot().await)
}

async fn handshake(State(service): State<BridgeService>) -> Json<ApiResult> {
    service.record_client_heartbeat().await;
    Json(ApiResult { ok: true })
}

async fn applications(
    State(service): State<BridgeService>,
) -> Json<Vec<crate::applications::ApplicationInfo>> {
    Json(service.applications().await)
}

async fn application_icon(
    State(service): State<BridgeService>,
    Path(icon_id): Path<String>,
) -> Result<Response<Body>, StatusCode> {
    let icon = service
        .application_icon(&icon_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    Response::builder()
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "private, max-age=86400")
        .body(Body::from(icon))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn profiles(State(service): State<BridgeService>) -> Json<Vec<ApplicationProfile>> {
    Json(service.profiles().await)
}

async fn games(State(service): State<BridgeService>) -> Json<Vec<GameConfig>> {
    Json(service.games().await)
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

async fn set_default_profile(
    State(service): State<BridgeService>,
    Json(profile): Json<ApplicationProfile>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    service
        .set_default_profile(profile)
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
