use std::{collections::HashSet, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State, ws::WebSocketUpgrade},
    http::{HeaderMap, HeaderValue, Method, Response, StatusCode, header},
    routing::{delete, get, put},
};
use serde::{Deserialize, Serialize};
use tower_http::{cors::CorsLayer, set_header::SetResponseHeaderLayer, trace::TraceLayer};

use crate::{
    config::{ApplicationProfile, GameConfig},
    drivers, platform,
    service::{BatteryReading, BridgeService},
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiResult {
    ok: bool,
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
struct NativeSettingsPayload {
    brand: String,
    dpi: Option<u32>,
    polling_rate_hz: Option<u32>,
}

pub fn router(service: BridgeService, origins: &[String]) -> Router {
    let allowed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();
    let hid_origins = Arc::new(origins.iter().cloned().collect::<HashSet<_>>());
    let hid_route = Router::new().route(
        "/v1/hid",
        get(move |websocket, headers| hid_upgrade(websocket, headers, Arc::clone(&hid_origins))),
    );
    let cors = CorsLayer::new()
        .allow_origin(allowed)
        .allow_methods([Method::GET, Method::PUT, Method::DELETE])
        .allow_headers([axum::http::header::CONTENT_TYPE])
        .max_age(Duration::from_secs(3600));
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/handshake", put(handshake))
        .route("/v1/games", get(games))
        .route(
            "/v1/custom-games",
            get(custom_games).put(replace_custom_games),
        )
        .route(
            "/v1/custom-games/image",
            get(custom_game_image).put(set_custom_game_image),
        )
        .route("/v1/custom-games/image-from-url", get(image_from_link))
        .route("/v1/applications", get(applications))
        .route("/v1/applications/{icon_id}/icon", get(application_icon))
        .route("/v1/running-applications", get(running_applications))
        .route("/v1/profiles", get(profiles).put(replace_profiles))
        .route(
            "/v1/default-profile",
            get(default_profile).put(set_default_profile),
        )
        .route("/v1/native/settings", put(apply_native_settings))
        .route("/v1/battery", put(record_battery))
        .route("/v1/battery/{device_id}", delete(forget_battery))
        .route("/v1/autostart", put(set_autostart))
        .route("/v1/panel", put(show_panel))
        .merge(hid_route)
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::HeaderName::from_static("access-control-allow-private-network"),
            HeaderValue::from_static("true"),
        ))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(service)
}

async fn hid_upgrade(
    websocket: WebSocketUpgrade,
    headers: HeaderMap,
    allowed_origins: Arc<HashSet<String>>,
) -> Result<Response<Body>, StatusCode> {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    else {
        tracing::warn!("Rejected native HID WebSocket without an Origin header");
        return Err(StatusCode::FORBIDDEN);
    };
    if !allowed_origins.contains(origin) {
        tracing::warn!(%origin, "Rejected native HID WebSocket from an unapproved origin");
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(websocket.on_upgrade(crate::hid::serve))
}

async fn status(State(service): State<BridgeService>) -> Json<crate::service::BridgeSnapshot> {
    Json(service.snapshot().await)
}

async fn show_panel(State(service): State<BridgeService>) -> Json<ApiResult> {
    service.request_show_panel();
    Json(ApiResult { ok: true })
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

async fn running_applications(
    State(service): State<BridgeService>,
) -> Json<Vec<crate::applications::ApplicationInfo>> {
    Json(service.running_applications().await)
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

async fn custom_games(State(service): State<BridgeService>) -> Json<Vec<GameConfig>> {
    Json(service.custom_games().await)
}

async fn replace_custom_games(
    State(service): State<BridgeService>,
    Json(games): Json<Vec<GameConfig>>,
) -> Result<Json<Vec<GameConfig>>, (StatusCode, String)> {
    service
        .set_custom_games(games)
        .await
        .map(Json)
        .map_err(internal_error)
}

#[derive(Debug, Deserialize)]
struct GameImageQuery {
    name: String,
}

async fn custom_game_image(
    State(service): State<BridgeService>,
    Query(query): Query<GameImageQuery>,
) -> Result<Response<Body>, StatusCode> {
    let (image, kind) = service
        .custom_game_image(&query.name)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    Response::builder()
        .header(header::CONTENT_TYPE, kind)
        // Revalidated every time: an image can be replaced under the same name.
        .header(header::CACHE_CONTROL, "private, no-cache")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from(image))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Debug, Deserialize)]
struct ImageLinkQuery {
    url: String,
}

async fn image_from_link(
    Query(query): Query<ImageLinkQuery>,
) -> Result<Response<Body>, (StatusCode, String)> {
    let (image, kind) = crate::remote_image::fetch(&query.url)
        .await
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    Response::builder()
        .header(header::CONTENT_TYPE, kind)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from(image))
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

async fn set_custom_game_image(
    State(service): State<BridgeService>,
    Query(query): Query<GameImageQuery>,
    body: Bytes,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    service
        .set_custom_game_image(&query.name, &body)
        .await
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    Ok(Json(ApiResult { ok: true }))
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

async fn default_profile(State(service): State<BridgeService>) -> Json<Option<ApplicationProfile>> {
    Json(service.default_profile().await)
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

async fn forget_battery(
    State(service): State<BridgeService>,
    Path(device_id): Path<String>,
) -> Json<ApiResult> {
    service.forget_battery(&device_id).await;
    Json(ApiResult { ok: true })
}

async fn set_autostart(
    Json(payload): Json<AutostartPayload>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    platform::set_autostart(payload.enabled).map_err(internal_error)?;
    Ok(Json(ApiResult { ok: true }))
}

async fn apply_native_settings(
    Json(payload): Json<NativeSettingsPayload>,
) -> Result<Json<ApiResult>, (StatusCode, String)> {
    if payload.dpi.is_none() && payload.polling_rate_hz.is_none() {
        return Err((
            StatusCode::BAD_REQUEST,
            "at least one of dpi or pollingRateHz is required".into(),
        ));
    }

    let NativeSettingsPayload {
        brand,
        dpi,
        polling_rate_hz,
    } = payload;
    let error_brand = brand.clone();
    let applied =
        tokio::task::spawn_blocking(move || drivers::apply_settings(&brand, dpi, polling_rate_hz))
            .await
            .map_err(|error| internal_error(anyhow::anyhow!(error)))?
            .map_err(internal_error)?;

    if !applied {
        return Err((
            StatusCode::NOT_FOUND,
            format!("no native driver is registered for {error_brand}"),
        ));
    }
    Ok(Json(ApiResult { ok: true }))
}

fn internal_error(error: anyhow::Error) -> (StatusCode, String) {
    tracing::error!(%error, "Bridge request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

#[cfg(test)]
mod tests {
    use axum::{
        body::to_bytes,
        http::{Method, Request},
    };
    use tower::ServiceExt;

    use super::*;
    use crate::{config::BridgeConfig, remote_image::PNG_SIGNATURE};

    fn app() -> (Router, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "bridge-api-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let service = BridgeService::new(BridgeConfig::default(), dir.join("config.json"));
        (router(service, &[]), dir)
    }

    async fn call(
        app: &Router,
        method: Method,
        uri: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>, Option<String>) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body))
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        let status = response.status();
        let kind = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body")
            .to_vec();
        (status, bytes, kind)
    }

    #[tokio::test]
    async fn a_custom_game_keeps_the_image_saved_for_it() {
        let (app, dir) = app();
        let games = br#"[{"name":"My Game","executables":["MyGame.exe"]}]"#.to_vec();
        let (status, saved, _) = call(
            &app,
            Method::PUT,
            "/v1/custom-games",
            "application/json",
            games,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            String::from_utf8_lossy(&saved).contains("mygame.exe"),
            "the list comes back normalized"
        );

        let mut png = PNG_SIGNATURE.to_vec();
        png.extend_from_slice(&[1, 2, 3]);
        let (status, ..) = call(
            &app,
            Method::PUT,
            "/v1/custom-games/image?name=My%20Game",
            "image/png",
            png.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, bytes, kind) = call(
            &app,
            Method::GET,
            "/v1/custom-games/image?name=my%20game",
            "",
            vec![],
        )
        .await;
        assert_eq!(
            (status, bytes, kind.as_deref()),
            (StatusCode::OK, png, Some("image/png"))
        );
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn an_image_is_refused_for_an_unknown_game_or_a_file_that_is_not_an_image() {
        let (app, dir) = app();
        let (status, ..) = call(
            &app,
            Method::PUT,
            "/v1/custom-games/image?name=Nope",
            "image/png",
            PNG_SIGNATURE.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "the game must exist");
        let games = br#"[{"name":"Game","executables":["game.exe"]}]"#.to_vec();
        call(
            &app,
            Method::PUT,
            "/v1/custom-games",
            "application/json",
            games,
        )
        .await;
        let (status, ..) = call(
            &app,
            Method::PUT,
            "/v1/custom-games/image?name=Game",
            "image/png",
            b"<html>".to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "only real images are kept");
        let (status, ..) = call(
            &app,
            Method::GET,
            "/v1/custom-games/image?name=Game",
            "",
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn a_link_into_the_local_network_is_refused() {
        let (app, dir) = app();
        let (status, message, _) = call(
            &app,
            Method::GET,
            "/v1/custom-games/image-from-url?url=http%3A%2F%2F192.168.1.1%2Flogo.png",
            "",
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&message).contains("cannot be reached"));
        std::fs::remove_dir_all(dir).expect("cleanup");
    }
}
