use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;

use crate::db::{GpxPoint, Ping};
use crate::SharedState;

pub fn api_router() -> Router<SharedState> {
    Router::new()
        .route("/sessions", post(create_session))
        .route("/sessions/:id", get(get_session))
        .route("/sessions/:id/ping", post(post_ping))
        .route("/sessions/:id/pings", get(get_pings))
        .route("/sessions/:id/gpx", get(get_gpx).post(upload_gpx))
        .route("/sessions/:id/ws", get(ws_upgrade))
        .route("/sessions/:id/owntracks", post(owntracks_ping))
}

#[derive(Deserialize)]
struct CreateSession {
    name: Option<String>,
    hours: Option<u32>,
}

async fn create_session(
    State(state): State<SharedState>,
    Json(body): Json<CreateSession>,
) -> Result<impl IntoResponse, StatusCode> {
    let id = generate_id();
    let hours = body.hours.unwrap_or(48).min(72);
    let session = state
        .db
        .create_session(&id, body.name.as_deref(), hours)
        .map_err(|e| {
            tracing::error!("create_session: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok((StatusCode::CREATED, Json(session)))
}

async fn get_session(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = state.db.get_session(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let pings = state.db.get_pings(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let gpx = state.db.get_gpx(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({
        "session": session,
        "pings": pings,
        "gpx": gpx,
    })))
}

async fn post_ping(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(ping): Json<Ping>,
) -> Result<StatusCode, StatusCode> {
    state.db.get_session(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    state.db.insert_ping(&id, &ping).map_err(|e| {
        tracing::error!("insert_ping: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let tx = state.channels.get_or_create(&id);
    let _ = tx.send(ping);

    Ok(StatusCode::NO_CONTENT)
}

async fn get_pings(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<Ping>>, StatusCode> {
    state.db.get_session(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let pings = state.db.get_pings(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(pings))
}

async fn upload_gpx(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: String,
) -> Result<StatusCode, StatusCode> {
    state.db.get_session(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let points = parse_gpx(&body).map_err(|e| {
        tracing::warn!("gpx parse error: {e}");
        StatusCode::BAD_REQUEST
    })?;

    let json = serde_json::to_string(&points).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.db.set_gpx(&id, &json).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::NO_CONTENT)
}

async fn get_gpx(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> Result<Json<Option<Vec<GpxPoint>>>, StatusCode> {
    state.db.get_session(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let gpx = state.db.get_gpx(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(gpx))
}

#[derive(Deserialize)]
struct OwnTracksPayload {
    #[serde(rename = "_type")]
    msg_type: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    tst: Option<i64>,
    alt: Option<f64>,
    vel: Option<f64>,
    cog: Option<f64>,
}

async fn owntracks_ping(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    body: String,
) -> Result<Json<Vec<()>>, StatusCode> {
    if body.is_empty() {
        return Ok(Json(vec![]));
    }

    let payload: OwnTracksPayload =
        serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;

    if payload.msg_type.as_deref() != Some("location") {
        return Ok(Json(vec![]));
    }

    let (lat, lon, tst) = match (payload.lat, payload.lon, payload.tst) {
        (Some(lat), Some(lon), Some(tst)) => (lat, lon, tst),
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    state
        .db
        .get_session(&id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let ts = chrono::DateTime::from_timestamp(tst, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string());

    let ping = Ping {
        ts,
        lat,
        lon,
        ele: payload.alt,
        speed: payload.vel,
        heading: payload.cog,
    };

    state.db.insert_ping(&id, &ping).map_err(|e| {
        tracing::error!("owntracks insert_ping: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let tx = state.channels.get_or_create(&id);
    let _ = tx.send(ping);

    Ok(Json(vec![]))
}

async fn ws_upgrade(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, StatusCode> {
    state.db.get_session(&id).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let rx = state.channels.get_or_create(&id).subscribe();
    let session_id = id.clone();
    let st = state.clone();

    Ok(ws.on_upgrade(move |socket| handle_ws(socket, rx, session_id, st)))
}

async fn handle_ws(
    socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<Ping>,
    session_id: String,
    state: SharedState,
) {
    let (mut sink, mut stream) = socket.split();

    let send_task = tokio::spawn(async move {
        while let Ok(ping) = rx.recv().await {
            if let Ok(json) = serde_json::to_string(&ping)
                && sink.send(Message::Text(json)).await.is_err()
            {
                break;
            }
        }
    });

    // Keep reading to detect client disconnect
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(_)) = stream.next().await {}
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    state.channels.remove_if_empty(&session_id);
}

fn generate_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let chars: Vec<char> = "abcdefghjkmnpqrstuvwxyz23456789".chars().collect();
    (0..8).map(|_| chars[rng.gen_range(0..chars.len())]).collect()
}

fn parse_gpx(xml: &str) -> anyhow::Result<Vec<GpxPoint>> {
    let gpx = gpx::read(xml.as_bytes())?;
    let mut points = Vec::new();
    let mut total_dist = 0.0_f64;
    let mut prev: Option<(f64, f64)> = None;

    for track in &gpx.tracks {
        for segment in &track.segments {
            for pt in &segment.points {
                let lat = pt.point().y();
                let lon = pt.point().x();
                let ele = pt.elevation;

                if let Some((plat, plon)) = prev {
                    total_dist += haversine_km(plat, plon, lat, lon);
                }
                prev = Some((lat, lon));

                points.push(GpxPoint {
                    lat,
                    lon,
                    ele,
                    dist_km: (total_dist * 100.0).round() / 100.0,
                });
            }
        }
    }
    Ok(points)
}

fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = 6371.0;
    let dlat = (lat2 - lat1).to_radians();
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat / 2.0).sin().powi(2)
        + lat1.to_radians().cos() * lat2.to_radians().cos() * (dlon / 2.0).sin().powi(2);
    r * 2.0 * a.sqrt().asin()
}
