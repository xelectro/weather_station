use askama::Template;
use axum::response::{Html, IntoResponse, Redirect};
use axum::response::sse::KeepAlive;
use axum::{
    Router,
    extract::{ws::{WebSocket, WebSocketUpgrade, Message as WsMessage}, DefaultBodyLimit, Multipart, State, Path},
    http::{StatusCode, header},
    Json,
    response::sse::{Event, Sse},
    routing::{get, post},
};
use async_stream::stream;
use axum_extra::TypedHeader;
use futures_util::{SinkExt, StreamExt, stream::Stream};
use serde::{Deserialize};
use serde_json;
use std::collections::HashMap;
use std::future::Future;
use std::fs;
use std::path::{self, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use std::{convert::Infallible, time::Duration};
use tokio::sync::Mutex;
use tokio::time::interval;
use tower_http::services::ServeDir;
use chrono::{self, Local, TimeZone};
pub mod web;
pub mod stocks;
pub mod oil;
use web::{AppState, AviationEntry, AviationSocketRequest, AviationSocketResponse, BuoyObservation, ChatBootstrap, ChatClientMessage, ChatEvent, ChatUploadResponse, SseData, WsOutput};
use stocks::Stock;
use anyhow::anyhow;
use uuid;
use lettre::{Message, SmtpTransport, Transport};
use anyhow::Result;

const CHAT_UPLOAD_LIMIT_BYTES: usize = 20 * 1024 * 1024;
const CHAT_UPLOAD_REQUEST_LIMIT_BYTES: usize = 25 * 1024 * 1024;
const CHAT_MEDIA_DIR: &str = "static/chat_media";
const CHAT_MEDIA_ORIGINALS_DIR: &str = "static/chat_media/originals";
const CHAT_MEDIA_THUMBS_DIR: &str = "static/chat_media/thumbs";
const CHAT_MEDIA_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;
const CHAT_MEDIA_CLEANUP_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Boots the Axum application, initializes shared state, and starts the
/// background data acquisition loop before serving HTTP and WebSocket routes.
#[tokio::main]
async fn main() -> Result<()> {
    ensure_chat_media_dirs()?;
    purge_chat_media_on_startup()?;
    let app_state = AppState::new();
    tokio::spawn(aquire_data(app_state.clone()));
    tokio::spawn(chat_media_cleanup_task());
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await?;
    // build our application with a route
    let app = Router::new()
        .route("/sse", get(sse_handler))
        .route("/", get(default_get).post(default_post))
        .nest_service("/static", ServeDir::new("static"))
        .nest_service("/chat_media", ServeDir::new(CHAT_MEDIA_DIR))
        .route("/metar", get(metar_get).post(metar_post))
        .route("/stock", post(stock_post))
        .route("/taf", get(taf_root_get))
        .route("/taf/{airport}", get(taf_get))
        .route("/chat", get(chat_get).post(chat_post))
        .route("/chat_state", get(chat_state_get))
        .route("/chat_upload", post(chat_upload_post))
        .route("/users", get(users_get))
        .route("/output", get(output_get))
        .route("/input", get(input_get).post(input_post))
        .route("/add_user", get(add_user_get))
        .route("/logout", post(logout))
        .route("/ws", get(ws_handler))
        .route("/ws_local", get(ws_local_handler))
        .route("/ws_aviation", get(ws_aviation_handler))
        .route("/test", get(test_get))
        .layer(DefaultBodyLimit::max(CHAT_UPLOAD_REQUEST_LIMIT_BYTES))
        .with_state(app_state);
    let _ = axum::serve(listener, app).await;
    Ok(())
}

/// Streams the latest dashboard snapshot to browser clients over Server-Sent
/// Events so the main weather page can stay live without polling.
async fn sse_handler(
    TypedHeader(user_agent): TypedHeader<headers::UserAgent>,
    State(app_state): State<Arc<Mutex<AppState>>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    println!("{:?}", user_agent);
    let state_lck = app_state.lock().await;
    let mut rx = state_lck.sender.clone().subscribe();
    Sse::new(stream! {
        while let Ok(msg) = rx.recv().await {
            yield Ok(Event::default().data::<String>(msg));
        }


    }).keep_alive(KeepAlive::default())
}
/// Upgrades a client connection into the shared broadcast WebSocket channel.
async fn ws_handler(State(state):State<Arc<Mutex<AppState>>>, ws:WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws(state, socket, false))
    
}
/// Upgrades a client connection into a loopback WebSocket that only echoes the
/// sender's own messages. This is useful for local testing and isolated UIs.
async fn ws_local_handler(State(state):State<Arc<Mutex<AppState>>>, ws:WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws(state, socket, true))
}

/// Upgrades a client connection into a dedicated aviation WebSocket used by
/// the METAR and TAF popup windows for request/response updates.
async fn ws_aviation_handler(State(state): State<Arc<Mutex<AppState>>>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_aviation_ws(state, socket))
}
/// Bridges one WebSocket connection into the broadcast channel and converts
/// incoming text payloads into chat or generic event messages.
async fn handle_ws(state: Arc<Mutex<AppState>>, socket: WebSocket, loop_back: bool) {
    let (sender, mut receiver) = socket.split();
    let tx = state.lock().await.sender_ws.clone();
    let mut rx = tx.subscribe();
    let id = uuid::Uuid::new_v4();
    let connection_id = id.to_string();
    let mut sender = (id, sender);
    tokio::spawn(async move  {
        println!("Listening...");
        while let Ok(msg) = rx.recv().await {
            println!("Inside thread: broadcast payload received ({} bytes)", msg.1.len());
            if !loop_back {
                if msg.0 != sender.0.to_string() {
                    match sender.1.send(WsMessage::from(msg.1.clone())).await {
                        Ok(_) => println!("Message sent successfully"),
                        Err(e) => {
                            println!("Error: {}", e);
                            break;
                        },
                    }
                }
            }  else {
                if msg.0 == sender.0.to_string() {
                    match sender.1.send(WsMessage::from(msg.1.clone())).await {
                        Ok(_) => println!("Message sent successfully"),
                        Err(e) => {
                            println!("Error: {}", e);
                            break;
                        },
                    }
                }

            }       
        }  
        println!("Thread was closed !!!");

    });
    println!("After thread");
    let mut joined_user: Option<String> = None;
    while let Some(Ok(msg)) = receiver.next().await {
        match msg {
            WsMessage::Text(content) => {
                if let Ok(input) = serde_json::from_str::<ChatClientMessage>(&content) {
                    let now = Local::now().format("%A, %B %-d, %Y at %-H:%M:%S").to_string();
                    match input.kind.as_str() {
                        "join" => {
                            if let Some(user_id) = input.user_id.filter(|val| !val.trim().is_empty()) {
                                joined_user = Some(user_id.clone());
                                let payload = {
                                    let mut state_lck = state.lock().await;
                                    state_lck.users.retain(|_, value| value != &connection_id);
                                    state_lck.users.insert(user_id.clone(), connection_id.clone());
                                    let users = sorted_users(&state_lck.users);
                                    let event = ChatEvent {
                                        kind: "join".to_string(),
                                        user_id: Some(user_id.clone()),
                                        message: Some(format!("{} joined the room.", user_id)),
                                        image_name: None,
                                        image_type: None,
                                        image_data: None,
                                        image_thumb: None,
                                        now,
                                        users,
                                    };
                                    push_chat_history(&mut state_lck, &event);
                                    serde_json::to_string(&event).unwrap_or_else(|_| "INVALID DATA".to_string())
                                };
                                if let Err(e) = tx.send((connection_id.clone(), payload)) {
                                    println!("ERROR WHILE SENDING JOIN: {}", e);
                                }
                            }
                        }
                        "message" => {
                            if let Some(user_id) = input.user_id {
                                let message = input.message.unwrap_or_default();
                                let trimmed = message.trim().to_string();
                                let has_image = input.image_data.as_ref().map(|data| !data.trim().is_empty()).unwrap_or(false);
                                if !trimmed.is_empty() || has_image {
                                    let payload = {
                                        let mut state_lck = state.lock().await;
                                        let users = sorted_users(&state_lck.users);
                                        let event = ChatEvent {
                                            kind: "message".to_string(),
                                            user_id: Some(user_id),
                                            message: if trimmed.is_empty() { None } else { Some(trimmed) },
                                            image_name: input.image_name,
                                            image_type: input.image_type,
                                            image_data: input.image_data,
                                            image_thumb: input.image_thumb,
                                            now,
                                            users,
                                        };
                                        push_chat_history(&mut state_lck, &event);
                                        serde_json::to_string(&event).unwrap_or_else(|_| "INVALID DATA".to_string())
                                    };
                                    if let Err(e) = tx.send((connection_id.clone(), payload)) {
                                        println!("ERROR WHILE SENDING MESSAGE: {}", e);
                                        break;
                                    }
                                }
                            }
                        }
                        "leave" => {
                            if let Some(user_id) = joined_user.clone().or(input.user_id) {
                                handle_user_departure(&state, &tx, &connection_id, &user_id, &now);
                                joined_user = None;
                            }
                        }
                        _ => {}
                    }
                } else {
                    let input = serde_json::from_str::<serde_json::Value>(&content).unwrap_or(serde_json::Value::Null);
                    if let Some(val) = input.get("data"){
                        println!("Recv: {:?}", val);
                    }
                    let output = serde_json::to_string(&WsOutput{
                        from_id: id.to_string(),
                        now: Local::now().format("%A, %B %-d, %Y at %-H:%M:%S").to_string(),
                        data: content.to_string(),
                    }).unwrap_or("INVALID DATA".to_string());
                    match tx.send((id.to_string(), output)) {
                        Ok(_) => {},
                        Err(e) => {
                            println!("ERROR WHILE SENDING: {}", e);
                            break;
                        }
                    }
                }
                
            },
            WsMessage::Ping(content) => println!("Ping: {:?}", content),
            WsMessage::Close(_content) => println!("Connection Closed"),
            _ => match msg.to_text() {
                Ok(text) => println!("Other type of message: {}", text),
                Err(_) => println!("Other type of message has no text payload"),
            },
        }
    }
    if let Some(user_id) = joined_user {
        let now = Local::now().format("%A, %B %-d, %Y at %-H:%M:%S").to_string();
        handle_user_departure(&state, &tx, &connection_id, &user_id, &now);
    }

}

/// Serves per-connection METAR and TAF requests so the aviation popups can
/// refresh directly over WebSocket instead of listening to the dashboard SSE.
async fn handle_aviation_ws(state: Arc<Mutex<AppState>>, mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.next().await {
        let WsMessage::Text(content) = msg else {
            if matches!(msg, WsMessage::Close(_)) {
                break;
            }
            continue;
        };

        let request = match serde_json::from_str::<AviationSocketRequest>(&content) {
            Ok(request) => request,
            Err(e) => {
                let response = AviationSocketResponse {
                    kind: "error".to_string(),
                    airport: None,
                    entries: Vec::new(),
                    error: Some(format!("Invalid aviation request: {}", e)),
                };
                let payload = serde_json::to_string(&response).unwrap_or_else(|_| "{\"kind\":\"error\",\"entries\":[],\"error\":\"Serialization error\"}".to_string());
                if socket.send(WsMessage::Text(payload.into())).await.is_err() {
                    break;
                }
                continue;
            }
        };

        let response = match request.kind.as_str() {
            "snapshot" => {
                let state_lck = state.lock().await;
                AviationSocketResponse {
                    kind: "snapshot".to_string(),
                    airport: None,
                    entries: aviation_entries(&state_lck),
                    error: None,
                }
            }
            "metar" => {
                let airport = normalize_airport_id(request.airport);
                match airport {
                    Some(airport) => {
                        let fetch_result = get_raw_data(
                            airport.clone(),
                            false,
                            state.clone(),
                            |output, id, value| async move {
                                output.lock().await.metar.insert(id.to_string(), value);
                            },
                        ).await;
                        match fetch_result {
                            Ok(_) => {
                                let state_lck = state.lock().await;
                                AviationSocketResponse {
                                    kind: "metar".to_string(),
                                    airport: Some(airport),
                                    entries: aviation_entries(&state_lck),
                                    error: None,
                                }
                            }
                            Err(e) => AviationSocketResponse {
                                kind: "error".to_string(),
                                airport: Some(airport),
                                entries: Vec::new(),
                                error: Some(format!("Unable to fetch METAR: {}", e)),
                            },
                        }
                    }
                    None => AviationSocketResponse {
                        kind: "error".to_string(),
                        airport: None,
                        entries: Vec::new(),
                        error: Some("Enter a valid ICAO airport ID.".to_string()),
                    },
                }
            }
            "taf" => {
                let airport = normalize_airport_id(request.airport);
                match airport {
                    Some(airport) => {
                        let fetch_result = get_raw_data(
                            airport.clone(),
                            true,
                            state.clone(),
                            |output, id, value| async move {
                                output.lock().await.taf.insert(id.to_string(), value);
                            },
                        ).await;
                        match fetch_result {
                            Ok(_) => {
                                let state_lck = state.lock().await;
                                let entry = state_lck.taf.get(&airport).cloned().map(|taf| AviationEntry {
                                    airport: airport.clone(),
                                    metar: state_lck.metar.get(&airport).cloned(),
                                    taf: Some(taf),
                                });
                                AviationSocketResponse {
                                    kind: "taf".to_string(),
                                    airport: Some(airport),
                                    entries: entry.into_iter().collect(),
                                    error: None,
                                }
                            }
                            Err(e) => AviationSocketResponse {
                                kind: "error".to_string(),
                                airport: Some(airport),
                                entries: Vec::new(),
                                error: Some(format!("Unable to fetch TAF: {}", e)),
                            },
                        }
                    }
                    None => AviationSocketResponse {
                        kind: "error".to_string(),
                        airport: None,
                        entries: Vec::new(),
                        error: Some("Invalid airport ID for TAF.".to_string()),
                    },
                }
            }
            "clear" => {
                let mut state_lck = state.lock().await;
                state_lck.metar.clear();
                state_lck.taf.clear();
                AviationSocketResponse {
                    kind: "clear".to_string(),
                    airport: None,
                    entries: Vec::new(),
                    error: None,
                }
            }
            _ => AviationSocketResponse {
                kind: "error".to_string(),
                airport: request.airport,
                entries: Vec::new(),
                error: Some("Unknown aviation request.".to_string()),
            },
        };

        let payload = serde_json::to_string(&response).unwrap_or_else(|_| "{\"kind\":\"error\",\"entries\":[],\"error\":\"Serialization error\"}".to_string());
        if socket.send(WsMessage::Text(payload.into())).await.is_err() {
            break;
        }
    }
}

/// Returns the active chat usernames in a deterministic sorted order for UI use.
fn sorted_users(users: &HashMap<String, String>) -> Vec<String> {
    let mut vals = users.keys().cloned().collect::<Vec<_>>();
    vals.sort();
    vals
}

/// Appends a chat event to the in-memory history buffer and caps retention so
/// the process does not grow unbounded.
fn push_chat_history(state: &mut AppState, event: &ChatEvent) {
    state.chat_history.push(event.clone());
    if state.chat_history.len() > 100 {
        let keep_from = state.chat_history.len() - 100;
        state.chat_history.drain(0..keep_from);
    }
}

/// Removes a disconnected chat user from shared state and broadcasts a leave
/// event so connected clients can update their presence lists.
fn handle_user_departure(
    state: &Arc<Mutex<AppState>>,
    tx: &tokio::sync::broadcast::Sender<(String, String)>,
    connection_id: &str,
    user_id: &str,
    now: &str,
) {
    let state = state.clone();
    let tx = tx.clone();
    let connection_id = connection_id.to_string();
    let user_id = user_id.to_string();
    let now = now.to_string();
    tokio::spawn(async move {
        let payload = {
            let mut state_lck = state.lock().await;
            let should_remove = state_lck
                .users
                .get(&user_id)
                .map(|value| value == &connection_id)
                .unwrap_or(false);
            if !should_remove {
                return;
            }
            state_lck.users.remove(&user_id);
            let users = sorted_users(&state_lck.users);
            let event = ChatEvent {
                kind: "leave".to_string(),
                user_id: Some(user_id.clone()),
                message: Some(format!("{} left the room.", user_id)),
                image_name: None,
                image_type: None,
                image_data: None,
                image_thumb: None,
                now,
                users,
            };
            push_chat_history(&mut state_lck, &event);
            serde_json::to_string(&event).unwrap_or_else(|_| "INVALID DATA".to_string())
        };
        let _ = tx.send((connection_id, payload));
    });
}

/// Normalizes airport identifiers into uppercase ICAO-style values and rejects
/// obviously invalid user input before hitting the aviation data service.
fn normalize_airport_id(airport: Option<String>) -> Option<String> {
    let airport = airport?.trim().to_uppercase();
    if airport.is_empty() || airport.len() > 8 || !airport.chars().all(|ch| ch.is_ascii_alphanumeric()) {
        return None;
    }
    Some(airport)
}

/// Builds a stable combined METAR/TAF list for the aviation popup UIs.
fn aviation_entries(state: &AppState) -> Vec<AviationEntry> {
    let mut airports = state
        .metar
        .keys()
        .chain(state.taf.keys())
        .cloned()
        .collect::<Vec<_>>();
    airports.sort();
    airports.dedup();
    airports
        .into_iter()
        .map(|airport| AviationEntry {
            metar: state.metar.get(&airport).cloned(),
            taf: state.taf.get(&airport).cloned(),
            airport,
        })
        .collect()
}



/// Renders the main weather dashboard with the currently selected ZIP code.
async fn default_get(State(app_state): State<Arc<Mutex<AppState>>>,) -> impl IntoResponse {
    println!("Form handler");
    let state_lck = app_state.lock().await;
    let template = web::IndexTemplate { session: vec!["32011".to_string()] , zip: state_lck.zip.clone()};
    match template.render() {
        Ok(html) => Html(html),
        Err(e) => {
            println!("Failed to render index template: {}", e);
            Html("Template render failed".to_string())
        }
    }
}
/// Accepts ZIP code submissions from the dashboard and triggers an immediate
/// weather refresh for the selected location.
async fn default_post(
    State(app_state): State<Arc<Mutex<AppState>>>,
    form: Multipart,
) -> StatusCode {
    println!("Form handler");
    let form_data = match process_form(form).await {
        Ok(data) => data,
        Err(e) => {
            println!("Failed to parse zip form: {}", e);
            return StatusCode::BAD_REQUEST;
        }
    };
    if let Some(zip) = form_data.get("zip") {
        let mut state_lck = app_state.lock().await;
        if zip.len() == 5 {
            println!("New ZIP was parsed");
            state_lck.zip = zip.clone();
        } else {
            state_lck.status = "Invalid ZIP Code ! ! !".to_string();
        }
    }
    get_wx(app_state.clone()).await;
    StatusCode::OK
}

/// Runs the background scheduler that refreshes weather, stocks, oil prices,
/// and public IP state before publishing a combined SSE payload.
async fn aquire_data(state: Arc<Mutex<AppState>>) -> Result<()> {
    let mut interval = interval(Duration::from_secs(1));
    let mut sse_output = SseData::new();
    let mut now = Instant::now() - Duration::from_secs(60);
    let mut now2 = Instant::now() - Duration::from_hours(1);
    let mut now3 = Instant::now() - Duration::from_mins(5);
    let mut now4 = Instant::now() - Duration::from_mins(15);
    let mut ip = String::new();
    let mut old_ip = String::new();
    loop {
        let stocks_arc = {
            let state = state.lock().await;
            state.stocks.clone()
        };
        let oil_arc = {
            let state = state.lock().await;
            state.oil.clone()
        };
        if now.elapsed() >= Duration::from_secs(60) {
            get_wx(state.clone()).await;
            now = Instant::now();
            match update_ip(&old_ip).await {
                Ok(new_ip) => {
                    ip = new_ip;
                    old_ip = ip.clone();
                }
                Err(error) => println!("Failed to update IP: {}", error),
            }
            println!("Attempted getting Wx");
        }
        if now2.elapsed() >= Duration::from_hours(1) {
            let mut oil_lck = oil_arc.lock().await;
            for x in oil_lck.iter_mut() {
                if let Err(error) = x.update().await {
                    println!("Failed to update oil price: {}", error);
                }
            }
            now2 = Instant::now();
            println!("Attempted to update oil price!");
        }
        if now3.elapsed() >= Duration::from_mins(5) {
            if let Err(error) = updater(stocks_arc.clone()).await {
                println!("Failed to update stocks: {}", error);
            }
            now3 = Instant::now();
            println!("Attempted to update stocks");
        }
        if now4.elapsed() >= Duration::from_mins(15) {
            if let Err(error) = update_buoy_data(state.clone()).await {
                println!("Failed to update buoy data: {}", error);
            }
            now4 = Instant::now();
            println!("Attempted to update NOAA buoy data");
        }
        let (sender, display, users, wx, metar, taf, status, buoy) = {
            let mut state_lck = state.lock().await;
            state_lck.ip = ip.clone();
            state_lck.wx.entry("now".to_string()).insert_entry(Local::now().format("%A, %B %-d, %Y at %-H:%M:%S").to_string());
            state_lck.now = Local::now().format("%B %-d, %Y - %-H:%M:%S").to_string();
            (
                state_lck.sender.clone(),
                state_lck.display.clone(),
                state_lck.users.clone(),
                state_lck.wx.clone(),
                state_lck.metar.clone(),
                state_lck.taf.clone(),
                state_lck.status.clone(),
                state_lck.buoy.clone(),
            )
        };
        sse_output.now = Local::now().format("%B %-d, %Y - %-H:%M:%S").to_string();
        sse_output.wx = wx;
        sse_output.metar.clear();
        for (key, value) in metar.iter() {
            sse_output.metar.insert(key.to_uppercase(), vec![value.clone()]);
        }
        for (key, value) in taf.iter() {
            if let Some(values) = sse_output.metar.get_mut(key) {
                values.push(value.clone());
            }
        }
        sse_output.users = users;
        sse_output.display = display;
        sse_output.stocks = stocks_arc.lock().await.clone();
        let oil_guard = oil_arc.lock().await;
        if oil_guard.len() > 0 {
            sse_output.oil1 = oil_guard[0].display();
        }
        if oil_guard.len() > 1 {
            sse_output.oil2 = oil_guard[1].display();
        }
        sse_output.buoy = buoy;
        sse_output.status = status;
        let _ = sender.send(serde_json::to_string(&sse_output).unwrap_or_else(|_| "JSON ERROR".to_string()));
        interval.tick().await;
    }
}

async fn update_buoy_data(app_state: Arc<Mutex<AppState>>) -> Result<()> {
    let station = "41112";
    let addr = format!("https://www.ndbc.noaa.gov/data/realtime2/{station}.txt");
    let client = reqwest::Client::new();
    let body = client.get(addr).send().await?.text().await?;
    let mut observation = parse_buoy_observation(station, &body)?;
    if let Ok(flag_body) = client
        .get("https://safebeachday.com/fernandina-city/")
        .send()
        .await?
        .text()
        .await
    {
        observation.beach_flag = parse_fernandina_beach_flag(&flag_body);
    }
    app_state.lock().await.buoy = observation;
    Ok(())
}

fn parse_buoy_observation(station: &str, body: &str) -> Result<BuoyObservation> {
    let mut lines = body.lines().filter(|line| !line.trim().is_empty());
    let header_line = lines.next().ok_or_else(|| anyhow!("missing buoy header line"))?;
    let _units_line = lines.next().ok_or_else(|| anyhow!("missing buoy units line"))?;
    let data_line = lines.next().ok_or_else(|| anyhow!("missing buoy data line"))?;

    let headers: Vec<&str> = header_line.split_whitespace().collect();
    let values: Vec<&str> = data_line.split_whitespace().collect();
    if headers.len() != values.len() {
        return Err(anyhow!("buoy feed column mismatch"));
    }

    let columns: HashMap<&str, &str> = headers.into_iter().zip(values).collect();
    let timestamp: [Option<&str>; 5] = [
        columns.get("#YY").copied(),
        columns.get("MM").copied(),
        columns.get("DD").copied(),
        columns.get("hh").copied(),
        columns.get("mm").copied(),
    ];

    Ok(BuoyObservation {
        station: station.to_string(),
        wave_height_ft: normalize_buoy_wave_height(columns.get("WVHT").copied()),
        interval_sec: normalize_buoy_value(columns.get("DPD").copied(), Some("s")),
        water_temp_f: normalize_buoy_temperature(columns.get("WTMP").copied()),
        air_temp_f: normalize_buoy_temperature(columns.get("ATMP").copied()),
        beach_flag: None,
        updated_at: if timestamp.iter().all(|part| part.is_some()) {
            Some(format!(
                "{}-{}-{} {}:{} UTC",
                timestamp[0].unwrap_or(&""),
                timestamp[1].unwrap_or(&""),
                timestamp[2].unwrap_or(&""),
                timestamp[3].unwrap_or(&""),
                timestamp[4].unwrap_or(&"")
            ))
        } else {
            None
        },
    })
}

fn normalize_buoy_value(value: Option<&str>, suffix: Option<&str>) -> Option<String> {
    let raw = value?.trim();
    if raw.is_empty() || raw == "MM" {
        return None;
    }
    Some(match suffix {
        Some(unit) => format!("{raw} {unit}"),
        None => raw.to_string(),
    })
}

fn normalize_buoy_temperature(value: Option<&str>) -> Option<String> {
    let raw = value?.trim();
    if raw.is_empty() || raw == "MM" {
        return None;
    }
    let celsius = raw.parse::<f64>().ok()?;
    let fahrenheit = (celsius * 1.8) + 32.0;
    Some(format!("{:.1} F", fahrenheit))
}

fn normalize_buoy_wave_height(value: Option<&str>) -> Option<String> {
    let raw = value?.trim();
    if raw.is_empty() || raw == "MM" {
        return None;
    }
    let meters = raw.parse::<f64>().ok()?;
    let feet = meters * 3.28084;
    Some(format!("{:.1} ft", feet))
}

fn parse_fernandina_beach_flag(body: &str) -> Option<String> {
    let marker_index = body.find("Aquatic Risk")?;
    let section = &body[marker_index..];
    let section_lower = section.to_lowercase();

    if section_lower.contains("double red") || section_lower.contains("water closed to the public") {
        return Some("Double Red".to_string());
    }
    if section_lower.contains("marine pest") || section_lower.contains("purple") {
        return Some("Purple".to_string());
    }
    if section_lower.contains("high hazard") || section_lower.contains(">danger") || section_lower.contains("### danger") {
        return Some("Red".to_string());
    }
    if section_lower.contains("caution") || section_lower.contains("medium hazard") {
        return Some("Yellow".to_string());
    }
    if section_lower.contains("low hazard") || section_lower.contains("calm") || section_lower.contains("green") {
        return Some("Green".to_string());
    }

    extract_heading_after(section, "###")
}

fn extract_heading_after(section: &str, marker: &str) -> Option<String> {
    let idx = section.find(marker)?;
    let after = &section[idx + marker.len()..];
    let line = after.lines().next()?.trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

/// Minimal representation of the OpenWeather API payload used by the dashboard.
#[derive(Deserialize)]
struct OpenWeatherResponse {
    weather: Vec<WeatherDescription>,
    main: MainWeather,
    wind: Wind,
    clouds: Clouds,
    sys: Sys,
    coord: Coord,
    visibility: Option<f64>,
    name: String,
    dt: i64,
}

/// Weather condition summary entries returned by OpenWeather.
#[derive(Deserialize)]
struct WeatherDescription {
    main: String,
    description: String,
}

/// Core atmospheric values from the OpenWeather response.
#[derive(Deserialize)]
struct MainWeather {
    temp: f64,
    pressure: Option<u64>,
    humidity: Option<u64>,
}

/// Wind fields from the OpenWeather response.
#[derive(Deserialize)]
struct Wind {
    speed: Option<f64>,
    gust: Option<f64>,
    deg: Option<u64>,
}

/// Cloud coverage information from the OpenWeather response.
#[derive(Deserialize)]
struct Clouds {
    all: Option<u64>,
}

/// Sunrise and sunset timestamps from the OpenWeather response.
#[derive(Deserialize)]
struct Sys {
    sunrise: i64,
    sunset: i64,
}

/// Latitude and longitude values used for storing the current location.
#[derive(Deserialize)]
struct Coord {
    lat: f64,
    lon: f64,
}

/// Fetches current weather for the configured ZIP code, derives display-ready
/// values, and writes them into shared application state.
async fn get_wx(app_state: Arc<Mutex<AppState>>) {
    let zip = {
        let state = app_state.lock().await;
        state.zip.clone()
    };
    let key = "373ee2691da6e29a8a5a3315557f8fff".to_string();
    if key.is_empty() {
        println!("OPENWEATHER_API_KEY is not set, skipping weather update");
        return;
    }
    let addr = format!("http://api.openweathermap.org/data/2.5/weather?zip={zip},us&appid={key}");
    let client = reqwest::Client::new();
    let wx_data = match client.get(addr).send().await {
        Ok(resp) => match resp.json::<OpenWeatherResponse>().await {
            Ok(data) => data,
            Err(e) => {
                println!("Failed to parse OpenWeather response: {}", e);
                return;
            }
        },
        Err(e) => {
            println!("Failed to fetch weather data: {}", e);
            return;
        }
    };
    let now = Local.timestamp_opt(wx_data.dt, 0).single().unwrap_or_else(|| Local::now());
    let sun_rise = Local.timestamp_opt(wx_data.sys.sunrise, 0).single().unwrap_or_else(|| Local::now());
    let sun_set = Local.timestamp_opt(wx_data.sys.sunset, 0).single().unwrap_or_else(|| Local::now());
    let main = wx_data.weather.get(0).map(|w| w.main.clone()).unwrap_or_default();
    let desc = wx_data.weather.get(0).map(|w| w.description.clone()).unwrap_or_default();
    let temp_f = (wx_data.main.temp * 1.8) - 459.67;
    let temp_c = (temp_f - 32.0) / 1.8;
    let press = wx_data.main.pressure.map(|p| p.to_string()).unwrap_or_default();
    let humid = wx_data.main.humidity.unwrap_or_default();
    let dew_point_c = {
        let a = 17.625;
        let b = 243.04;
        let gamma = (humid as f64 / 100.0).ln() + (a * temp_c) / (b + temp_c);
        b * gamma / (a - gamma)
    };
    let dew_point_f = dew_point_c * 1.8 + 32.0;
    let wind_speed = (wx_data.wind.speed.unwrap_or(0.0) * 2.23694).round();
    let gust = (wx_data.wind.gust.unwrap_or(0.0) * 2.23694).round();
    let wind_deg = wx_data.wind.deg.unwrap_or_default().to_string();
    let clouds = wx_data.clouds.all.unwrap_or_default().to_string();
    let t_o_d = if now > sun_rise && now < sun_set {
        "day".to_string()
    } else {
        "night".to_string()
    };
    let formatted_now = now.format("%A, %B %-d, %Y at %-H:%M:%S").to_string();
    let formatted_sun_rise = sun_rise.format("%-I:%M:%S %p").to_string();
    let formatted_sun_set = sun_set.format("%-I:%M:%S %p").to_string();
    for value in [&formatted_now, &formatted_sun_rise, &formatted_sun_set] {
        println!("{}", value);
    }
    let vis = (wx_data.visibility.unwrap_or(0.0) * 0.000621371).to_string();
    let wind_compass = ((wx_data.wind.deg.unwrap_or(0) as f64 / 22.5).round() as usize) % 16;
    let compass = ["N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW"];
    let lat = wx_data.coord.lat.to_string();
    let lon = wx_data.coord.lon.to_string();
    let name = wx_data.name;

    let mut state_lck = app_state.lock().await;
    state_lck.wx.insert("wx".to_string(), main.clone());
    state_lck.wx.insert("desc".to_string(), desc.clone());
    state_lck.wx.insert("temp".to_string(), temp_c.to_string());
    state_lck.wx.insert("dew_piont_c".to_string(), dew_point_c.to_string());
    state_lck.wx.insert("dew_point_f".to_string(), dew_point_f.to_string());
    state_lck.wx.insert("press".to_string(), press.clone());
    state_lck.wx.insert("humid".to_string(), humid.to_string());
    state_lck.wx.insert("wind_dir".to_string(), wind_deg.clone());
    state_lck.wx.insert("wind_speed".to_string(), wind_speed.to_string());
    state_lck.wx.insert("gust".to_string(), gust.to_string());
    state_lck.wx.insert("compass".to_string(), compass[wind_compass].to_string());
    state_lck.wx.insert("ceiling".to_string(), clouds.clone());
    state_lck.wx.insert("now".to_string(), formatted_now.clone());
    state_lck.wx.insert("sunrise".to_string(), formatted_sun_rise.clone());
    state_lck.wx.insert("sunset".to_string(), formatted_sun_set.clone());
    state_lck.wx.insert("vis".to_string(), vis.clone());
    state_lck.wx.insert("lat".to_string(), lat.clone());
    state_lck.wx.insert("lon".to_string(), lon.clone());
    state_lck.wx.insert("city".to_string(), name.clone());
    state_lck.wx.insert("t_o_d".to_string(), t_o_d.clone());
    println!("Updated weather for {}", name);
}

/// Serves the popup page that displays METAR data.
async fn metar_get(State(_state): State<Arc<Mutex<AppState>>>) -> impl IntoResponse {
    Html(read_html_file(path::Path::new("templates/metar.html")).unwrap_or_else(|e| {
        println!("Failed to read metar template: {}", e);
        "404".to_string()
    }))
}

/// Accepts airport IDs from the METAR popup and refreshes the corresponding
/// METAR/TAF data stored in shared state.
async fn metar_post(State(state): State<Arc<Mutex<AppState>>>, form_data: Multipart) -> StatusCode {
    let form = match process_form(form_data).await {
        Ok(map) => map,
        Err(e) => {
            println!("Failed to parse metar form: {}", e);
            return StatusCode::BAD_REQUEST;
        }
    };
    let airport_id = form.get("ID").map(|id| id.to_uppercase());
    if let Some(val) = airport_id {
        if val == "DELETE" {
            println!("Delete pressed");
            let mut state_lck = state.lock().await;
            state_lck.metar.clear();
            state_lck.taf.clear();
            println!("{:?}", state_lck.metar);
        } else {
            if let Err(e) = get_raw_data(val.clone(), false, state.clone(), |output, id, value| async move {output.lock().await.metar.insert(id.to_string(), value);}).await {
                println!("Error getting METAR data: {}", e);
            }
            if !state.lock().await.taf.is_empty() {
                if let Err(e) = get_raw_data(val, true, state, |output, id, value| async move {output.lock().await.taf.insert(id.to_string(), value);}).await {
                    println!("Error getting TAF data: {}", e);
                }
            }
        }
    }
    StatusCode::OK
}

/// Serves the shared TAF popup shell without selecting an airport up front.
async fn taf_root_get(State(_app_state): State<Arc<Mutex<AppState>>>) -> impl IntoResponse {
    Html::from(read_html_file(path::Path::new("templates/taf.html")).unwrap_or_else(|e| {
        println!("Failed to read taf template: {}", e);
        "404".to_string()
    }))
}

/// Serves the TAF popup shell; the page fetches the forecast over WebSocket
/// after it loads so it no longer depends on the dashboard SSE stream.
async fn taf_get(Path(val): Path<String>, State(_app_state): State<Arc<Mutex<AppState>>>) -> impl IntoResponse {
    println!("Path: {}", val);
    Html::from(read_html_file(path::Path::new("templates/taf.html")).unwrap_or_else(|e| {
        println!("Failed to read taf template: {}", e);
        "404".to_string()
    }))
}

/// Renders the chat window shell.
async fn chat_get(State(_app_state): State<Arc<Mutex<AppState>>>) -> impl IntoResponse{
    let template = web::ChatTemplate {userid: "none".to_string(), val: "Welcome".to_string()};
    match template.render() {
        Ok(html) => (
            [
                (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate, max-age=0"),
                (header::PRAGMA, "no-cache"),
                (header::EXPIRES, "0"),
            ],
            Html(html),
        ).into_response(),
        Err(e) => {
            println!("Failed to render chat template: {}", e);
            Html("Template render failed".to_string()).into_response()
        }
    }
}

/// Returns the current chat bootstrap payload, including active users and
/// recent in-memory history, for newly opened chat clients.
async fn chat_state_get(State(app_state): State<Arc<Mutex<AppState>>>) -> impl IntoResponse {
    let state_lck = app_state.lock().await;
    (
        [
            (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate, max-age=0"),
            (header::PRAGMA, "no-cache"),
            (header::EXPIRES, "0"),
        ],
        Json(ChatBootstrap {
            users: sorted_users(&state_lck.users),
            history: state_lck.chat_history.clone(),
        }),
    )
}

/// Accepts an original chat image and a client-generated thumbnail, stores
/// both under static media paths, and returns URLs that the chat UI can embed.
async fn chat_upload_post(mut form: Multipart) -> impl IntoResponse {
    let mut image_name = String::new();
    let mut image_type = String::new();
    let mut original_bytes = Vec::new();
    let mut thumbnail_bytes = Vec::new();
    let mut thumbnail_type = String::new();

    while let Some(field) = match form.next_field().await {
        Ok(field) => field,
        Err(e) => {
            println!("Failed to parse chat upload field: {}", e);
            return Err(StatusCode::BAD_REQUEST);
        }
    } {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "image_name" => {
                image_name = field.text().await.unwrap_or_default();
            }
            "image_type" => {
                image_type = field.text().await.unwrap_or_default();
            }
            "thumbnail_type" => {
                thumbnail_type = field.text().await.unwrap_or_default();
            }
            "image" => {
                original_bytes = match field.bytes().await {
                    Ok(bytes) => bytes.to_vec(),
                    Err(_) => return Err(StatusCode::BAD_REQUEST),
                };
            }
            "thumbnail" => {
                thumbnail_bytes = match field.bytes().await {
                    Ok(bytes) => bytes.to_vec(),
                    Err(_) => return Err(StatusCode::BAD_REQUEST),
                };
            }
            _ => {}
        }
    }

    if original_bytes.is_empty() || thumbnail_bytes.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if original_bytes.len() > CHAT_UPLOAD_LIMIT_BYTES {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }

    let original_ext = media_extension(&image_type, &image_name).unwrap_or("bin");
    let thumb_ext = media_extension(&thumbnail_type, &image_name).unwrap_or("jpg");
    let file_id = uuid::Uuid::new_v4().to_string();
    let original_filename = format!("{}.{}", file_id, original_ext);
    let thumb_filename = format!("{}-thumb.{}", file_id, thumb_ext);
    let original_path = PathBuf::from(CHAT_MEDIA_ORIGINALS_DIR).join(&original_filename);
    let thumb_path = PathBuf::from(CHAT_MEDIA_THUMBS_DIR).join(&thumb_filename);

    if let Err(e) = tokio::fs::write(&original_path, original_bytes).await {
        println!("Failed to store chat image: {}", e);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    if let Err(e) = tokio::fs::write(&thumb_path, thumbnail_bytes).await {
        println!("Failed to store chat thumbnail: {}", e);
        let _ = tokio::fs::remove_file(&original_path).await;
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let original_url = format!("/chat_media/originals/{}", original_filename);
    let thumbnail_url = format!("/chat_media/thumbs/{}", thumb_filename);

    Ok(Json(ChatUploadResponse {
        image_name: if image_name.trim().is_empty() {
            format!("image.{}", original_ext)
        } else {
            image_name
        },
        image_type,
        image_url: original_url,
        thumbnail_url,
    }))
}

/// Legacy form-based chat entry point that registers a user and re-renders the
/// chat template. The current chat window uses WebSockets after load.
async fn chat_post(State(app_state): State<Arc<Mutex<AppState>>>, form: Multipart) -> impl IntoResponse {
    let form_data = match process_form(form).await {
        Ok(data) => data,
        Err(e) => {
            println!("Failed to parse chat form: {}", e);
            return Html::from("ERROR".to_string());
        }
    };
    for (key, val) in &form_data {
        println!("{} : {}", key, val);
    }
    if form_data.contains_key("add_user") && form_data.contains_key("user_id") {
        let Some(user_id) = form_data.get("user_id").cloned() else {
            return Html::from("ERROR".to_string());
        };
        let mut state = app_state.lock().await;
        state.display.insert("data".to_string(), "".to_string());
        state.users.insert(user_id.clone(), "None".to_string());
        println!("{}", user_id);
        let template = web::ChatTemplate{userid: user_id.clone(), val: format!("Welcome {}", user_id.clone())};
        match template.render() {
            Ok(html) => Html(html),
            Err(e) => {
                println!("Failed to render chat template: {}", e);
                Html("Template render failed".to_string())
            }
        }
    } else {
        Html::from("ERROR".to_string())
    }
}

/// Serves the legacy users iframe template.
async fn users_get(State(_app_state): State<Arc<Mutex<AppState>>>) -> impl IntoResponse {
    let template = web::UsersTemplate {};
    match template.render() {
        Ok(html) => Html(html),
        Err(e) => {
            println!("Failed to render users template: {}", e);
            Html("Template render failed".to_string())
        }
    }
}

/// Serves the legacy message output iframe template.
async fn output_get(State(_app_statr): State<Arc<Mutex<AppState>>>) -> impl IntoResponse {
    let template = web::OutputTemplate{};
    match template.render() {
        Ok(html) => Html(html),
        Err(e) => {
            println!("Failed to render output template: {}", e);
            Html("Template render failed".to_string())
        }
    }
}

/// Serves the legacy message input iframe page.
async fn input_get(State(_app_state): State<Arc<Mutex<AppState>>>) -> impl IntoResponse {
    Html(read_html_file(path::Path::new("templates/input.html")).unwrap_or_else(|e| {
        println!("Failed to read input template: {}", e);
        "404".to_string()
    }))
}

/// Processes the legacy iframe chat form and stores the latest message payload
/// in shared state for older clients.
async fn input_post(State(app_state): State<Arc<Mutex<AppState>>>, form_data: Multipart) -> impl IntoResponse {
    let form_data = match process_form(form_data).await {
        Ok(data) => data,
        Err(e) => {
            println!("Failed to parse input form: {}", e);
            return Redirect::to("/input");
        }
    };
    if form_data.contains_key("user_id") && form_data.contains_key("value") {
        let msg_id = uuid::Uuid::new_v4().to_string();
        let Some(user_id) = form_data.get("user_id").cloned() else {
            return Redirect::to("/input");
        };
        let Some(value) = form_data.get("value").cloned() else {
            return Redirect::to("/input");
        };
        let output = HashMap::from([
        ("user".to_string(), user_id),
        ("data".to_string(), value),
        ("message_id".to_string(), msg_id)
        ]);
        println!("{:?}", &output);
        app_state.lock().await.display = output;

    }
    
    Redirect::to("/input")
}
/// Removes a user from the legacy user registry when the old popup chat closes.
async fn logout(State(app_state): State<Arc<Mutex<AppState>>>, form_data: Multipart) -> StatusCode {
    let form = match process_form(form_data).await {
        Ok(data) => data,
        Err(e) => {
            println!("Failed to parse logout form: {}", e);
            return StatusCode::BAD_REQUEST;
        }
    };
    if let Some(logout_id) = form.get("logout") {
        let mut state = app_state.lock().await;
        state.users.remove(logout_id);
    }
    println!("User Logged out");
    StatusCode::OK
}

/// Returns a short confirmation page used by the legacy chat logout flow.
async fn add_user_get(State(app_state): State<Arc<Mutex<AppState>>>) -> impl IntoResponse {
    let _state_lck = app_state.lock().await.clone();

    Html("<h1>You are logged out!</h1><script>setTimeout(() => window.close(), 3000)</script>")
}

/// Adds a stock symbol to the dashboard watch list and refreshes its first
/// quote before storing it in the bounded in-memory stock list.
async fn stock_post(State(app_state): State<Arc<Mutex<AppState>>>, form_data: Multipart) -> StatusCode {
    let form_data = match process_form(form_data).await {
        Ok(data) => data,
        Err(e) => {
            println!("Failed to parse stock form: {}", e);
            return StatusCode::BAD_REQUEST;
        }
    };
    if let Some(symbol) = form_data.get("symbol") {
        println!("{}", symbol.to_uppercase());
        let mut new_stock = Stock::new(symbol.clone().to_uppercase());
        if let Err(e) = new_stock.update().await {
            println!("Error occurred during update: {}", e);
        }
        let stocks_arc = {
            let state = app_state.lock().await;
            state.stocks.clone()
        };
        let mut stock_lck = stocks_arc.lock().await;
        if stock_lck.len() >= 5 {
            stock_lck.remove(0);
        }
        stock_lck.push(new_stock);
    }
    StatusCode::OK
}
/// Refreshes each tracked stock symbol in place.
async fn updater(stocks: Arc<Mutex<Vec<Stock>>>) -> Result<()> {
    let mut stocks_lck = stocks.lock().await;
    if !stocks_lck.is_empty() {
        for stock in stocks_lck.iter_mut() {
            if let Err(error) = stock.update().await {
                println!("Failed to update stock {}: {}", stock.symbol, error);
            }
        }
    }
    Ok(())
}

/// Serves a test page used for SSE/WebSocket experimentation.
async fn test_get(State(_app_app_state): State<Arc<Mutex<AppState>>>) -> impl IntoResponse {
    let path = path::Path::new("templates/test_sse.html");
    Html::from(read_html_file(path).unwrap_or_else(|e| {
        println!("Failed to read test template: {}", e);
        "404".to_string()
    }))
}
                
                     
// Helper functions located below.
/// Fetches raw METAR or TAF text from the aviationweather.gov JSON API and
/// forwards the parsed string into the provided async callback.
async fn get_raw_data<T, F, Fut>(id: String, taf:bool, data_out:T, callback: F) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where 
    F: Fn(T, String, String) -> Fut,
    Fut: Future<Output = ()>,

{
    let client = reqwest::Client::new();
    let addr = if !taf {
        format!("https://aviationweather.gov/api/data/metar?ids={}&format=json", id)
    } else {
        format!("https://aviationweather.gov/api/data/taf?ids={}&format=json", id)
    };
    let response = client.get(&addr).timeout(Duration::from_secs(10)).send().await?;
    let text = response.text().await?;
    let raw_output: serde_json::Value = serde_json::from_str(&text)?;
    if let Some(arr) = raw_output.as_array() {
        if let Some(obj) = arr.get(0) {
            if let Some(output_val) = obj.get(if !taf {"rawOb"} else {"rawTAF"}) {
                if let Some(output_str) = output_val.as_str() {
                    let output = output_str.trim_matches('"').to_string();
                    println!("{}", output);
                    callback(data_out, id.clone(), output.clone()).await;
                    println!("Getting {} for {}", if taf {"TAF"} else {"METAR"}, id);
                }
            }
        }
    }
    Ok(())
}

/// Collects multipart form fields into a simple string map for the route
/// handlers in this file.
async fn process_form(mut form: Multipart) -> Result<HashMap<String, String>> {
    let mut output = HashMap::new();
    while let Some(val) = form.next_field().await? {
        let key = val.name().unwrap_or("").to_string();
        println!("Name: {}", key);
        let value = val.text().await?;
        println!("{value}");
        output.insert(key, value);
    }
    Ok(output)
}

/// Reads a template or static HTML file from disk into memory.
fn read_html_file(path: &path::Path) -> Result<String> {
    let output = fs::read_to_string(path)?;
    Ok(output)
}

fn ensure_chat_media_dirs() -> Result<()> {
    fs::create_dir_all(CHAT_MEDIA_ORIGINALS_DIR)?;
    fs::create_dir_all(CHAT_MEDIA_THUMBS_DIR)?;
    Ok(())
}

fn purge_chat_media_on_startup() -> Result<()> {
    let removed = remove_chat_media_files_in_dir(CHAT_MEDIA_ORIGINALS_DIR)?
        + remove_chat_media_files_in_dir(CHAT_MEDIA_THUMBS_DIR)?;
    if removed > 0 {
        println!("Removed {} chat media files during startup cleanup.", removed);
    }
    Ok(())
}

async fn chat_media_cleanup_task() {
    let mut cleanup_interval = interval(Duration::from_secs(CHAT_MEDIA_CLEANUP_INTERVAL_SECS));
    cleanup_interval.tick().await;
    loop {
        cleanup_interval.tick().await;
        match cleanup_chat_media_older_than(Duration::from_secs(CHAT_MEDIA_MAX_AGE_SECS)) {
            Ok(removed) if removed > 0 => {
                println!("Removed {} expired chat media files.", removed);
            }
            Ok(_) => {}
            Err(e) => {
                println!("Failed to clean expired chat media: {}", e);
            }
        }
    }
}

fn cleanup_chat_media_older_than(max_age: Duration) -> Result<usize> {
    Ok(
        remove_expired_chat_media_files_in_dir(CHAT_MEDIA_ORIGINALS_DIR, max_age)?
            + remove_expired_chat_media_files_in_dir(CHAT_MEDIA_THUMBS_DIR, max_age)?
    )
}

fn remove_chat_media_files_in_dir(dir: &str) -> Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn remove_expired_chat_media_files_in_dir(dir: &str, max_age: Duration) -> Result<usize> {
    let mut removed = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(_) => continue,
        };
        let age = match modified.elapsed() {
            Ok(age) => age,
            Err(_) => continue,
        };
        if age >= max_age {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn media_extension(content_type: &str, filename: &str) -> Option<&'static str> {
    let normalized = content_type.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "image/bmp" => Some("bmp"),
        "image/tiff" => Some("tiff"),
        "image/svg+xml" => Some("svg"),
        "image/avif" => Some("avif"),
        _ => {
            let lowered = filename.to_ascii_lowercase();
            if lowered.ends_with(".jpg") || lowered.ends_with(".jpeg") {
                Some("jpg")
            } else if lowered.ends_with(".png") {
                Some("png")
            } else if lowered.ends_with(".webp") {
                Some("webp")
            } else if lowered.ends_with(".gif") {
                Some("gif")
            } else if lowered.ends_with(".bmp") {
                Some("bmp")
            } else if lowered.ends_with(".tif") || lowered.ends_with(".tiff") {
                Some("tiff")
            } else if lowered.ends_with(".svg") {
                Some("svg")
            } else if lowered.ends_with(".avif") {
                Some("avif")
            } else {
                None
            }
        }
    }
}

/// Looks up the current public IP address and sends an email notification if it
/// changed since the previous check.
async fn update_ip(old_ip: &str) -> Result<String> {
    let mut  output = String::new();
    let addr = "https://api.ipify.org";
    let client = reqwest::Client::new();
    if let Ok(data) = client.get(addr).timeout(Duration::from_secs(10)).send().await {
        output = data.text().await?.to_string();
        println!("IP: {}", output);
        if output != old_ip {
            let email = Message::builder()
                .from("xelectro@me.com".parse()?)
                .to("xelectro@protonmail.com".parse()?)
                .subject("New IP Address")
                .body(format!("{}\n", output.clone()))?;
            let mailer = SmtpTransport::builder_dangerous("127.0.0.1")
                .port(25)
                .build();
            mailer.send(&email)?;
            println!("New IP Sent!!!");
        } else {
            println!("IP Remains the same");
        }
    }
    Ok(output)
}
