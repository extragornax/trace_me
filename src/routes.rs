use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
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
        .route("/accounts", post(create_account))
        .route("/accounts/:slug", get(get_account).put(update_account))
        .route("/accounts/:slug/owntracks", post(account_owntracks))
        .route("/accounts/:slug/ws", get(account_ws_upgrade))
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

#[derive(Deserialize)]
struct CreateAccount {
    slug: String,
    password: String,
}

async fn create_account(
    State(state): State<SharedState>,
    Json(body): Json<CreateAccount>,
) -> Result<impl IntoResponse, StatusCode> {
    let slug = body.slug.trim().to_lowercase();
    if slug.len() < 2 || slug.len() > 32 || !slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(StatusCode::BAD_REQUEST);
    }
    if body.password.len() < 4 {
        return Err(StatusCode::BAD_REQUEST);
    }

    let hash = bcrypt::hash(&body.password, 10).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let id = generate_id();
    let account = state.db.create_account(&id, &slug, &hash).map_err(|e| {
        tracing::warn!("create_account: {e}");
        StatusCode::CONFLICT
    })?;

    Ok((StatusCode::CREATED, Json(account)))
}

fn extract_basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let val = headers.get("authorization")?.to_str().ok()?;
    let encoded = val.strip_prefix("Basic ")?;
    let decoded = String::from_utf8(
        axum::body::Bytes::from(base64_decode(encoded)?).to_vec(),
    ).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = Vec::new();
    let mut decoder = base64_reader(input.as_bytes());
    decoder.read_to_end(&mut buf).ok()?;
    Some(buf)
}

fn base64_reader(input: &[u8]) -> impl std::io::Read + '_ {
    struct B64Reader<'a> { src: &'a [u8], pos: usize, buf: [u8; 3], buf_len: usize, buf_pos: usize }
    impl<'a> std::io::Read for B64Reader<'a> {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let mut written = 0;
            while written < out.len() {
                if self.buf_pos < self.buf_len {
                    out[written] = self.buf[self.buf_pos];
                    self.buf_pos += 1;
                    written += 1;
                    continue;
                }
                let mut chunk = [0u8; 4];
                let mut i = 0;
                while i < 4 {
                    if self.pos >= self.src.len() {
                        return Ok(written);
                    }
                    let b = self.src[self.pos];
                    self.pos += 1;
                    if b == b'\n' || b == b'\r' || b == b' ' { continue; }
                    chunk[i] = b;
                    i += 1;
                }
                let vals: Vec<u8> = chunk.iter().map(|&c| match c {
                    b'A'..=b'Z' => c - b'A',
                    b'a'..=b'z' => c - b'a' + 26,
                    b'0'..=b'9' => c - b'0' + 52,
                    b'+' => 62, b'/' => 63, _ => 0,
                }).collect();
                self.buf[0] = (vals[0] << 2) | (vals[1] >> 4);
                self.buf[1] = (vals[1] << 4) | (vals[2] >> 2);
                self.buf[2] = (vals[2] << 6) | vals[3];
                self.buf_len = if chunk[3] == b'=' { if chunk[2] == b'=' { 1 } else { 2 } } else { 3 };
                self.buf_pos = 0;
            }
            Ok(written)
        }
    }
    B64Reader { src: input, pos: 0, buf: [0; 3], buf_len: 0, buf_pos: 0 }
}

fn verify_account(state: &crate::AppState, slug: &str, headers: &HeaderMap) -> Result<(crate::db::Account, String), StatusCode> {
    let (account, hash) = state.db.get_account_by_slug(slug)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let (_, password) = extract_basic_auth(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    if !bcrypt::verify(&password, &hash).unwrap_or(false) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok((account, hash))
}

fn ensure_account_session(state: &crate::AppState, account: &crate::db::Account) -> Result<String, StatusCode> {
    if let Some(ref sid) = account.session_id
        && state.db.get_session(sid).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?.is_some()
    {
        state.db.renew_session(sid, 48).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(sid.clone());
    }
    let sid = generate_id();
    state.db.create_session(&sid, Some(&account.slug), 48)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.db.set_account_session(&account.id, &sid)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(sid)
}

async fn get_account(
    State(state): State<SharedState>,
    Path(slug): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let (account, _) = state.db.get_account_by_slug(&slug)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let (pings, gpx) = if let Some(ref sid) = account.session_id {
        let p = state.db.get_pings(sid).unwrap_or_default();
        let g = state.db.get_gpx(sid).unwrap_or(None);
        (p, g)
    } else {
        (vec![], None)
    };

    Ok(Json(serde_json::json!({
        "account": { "slug": account.slug, "session_id": account.session_id },
        "pings": pings,
        "gpx": gpx,
    })))
}

#[derive(Deserialize)]
struct UpdateAccount {
    slug: Option<String>,
    password: Option<String>,
}

async fn update_account(
    State(state): State<SharedState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    Json(body): Json<UpdateAccount>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let (account, _) = verify_account(&state, &slug, &headers)?;

    if let Some(new_slug) = &body.slug {
        let new_slug = new_slug.trim().to_lowercase();
        if new_slug.len() < 2 || new_slug.len() > 32 || !new_slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(StatusCode::BAD_REQUEST);
        }
        state.db.update_account_slug(&account.id, &new_slug).map_err(|_| StatusCode::CONFLICT)?;
    }

    if let Some(new_password) = &body.password {
        if new_password.len() < 4 {
            return Err(StatusCode::BAD_REQUEST);
        }
        let hash = bcrypt::hash(new_password, 10).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let conn_result = state.db.update_account_password(&account.id, &hash);
        conn_result.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    let updated_slug = body.slug.as_deref().unwrap_or(&slug);
    Ok(Json(serde_json::json!({ "slug": updated_slug })))
}

async fn account_owntracks(
    State(state): State<SharedState>,
    Path(slug): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Json<Vec<()>>, StatusCode> {
    if body.is_empty() {
        return Ok(Json(vec![]));
    }

    let (account, hash) = state.db.get_account_by_slug(&slug)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    if let Some((_, password)) = extract_basic_auth(&headers) {
        if !bcrypt::verify(&password, &hash).unwrap_or(false) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    } else {
        return Err(StatusCode::UNAUTHORIZED);
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

    let sid = ensure_account_session(&state, &account)?;

    let ts = chrono::DateTime::from_timestamp(tst, 0)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string());

    let ping = Ping { ts, lat, lon, ele: payload.alt, speed: payload.vel, heading: payload.cog };

    state.db.insert_ping(&sid, &ping).map_err(|e| {
        tracing::error!("account owntracks insert_ping: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let tx = state.channels.get_or_create(&sid);
    let _ = tx.send(ping);

    Ok(Json(vec![]))
}

async fn account_ws_upgrade(
    State(state): State<SharedState>,
    Path(slug): Path<String>,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, StatusCode> {
    let (account, _) = state.db.get_account_by_slug(&slug)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let sid = account.session_id.ok_or(StatusCode::NOT_FOUND)?;
    state.db.get_session(&sid).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let rx = state.channels.get_or_create(&sid).subscribe();
    let st = state.clone();

    Ok(ws.on_upgrade(move |socket| handle_ws(socket, rx, sid, st)))
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
