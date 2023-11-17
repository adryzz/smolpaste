use std::{sync::Arc, time::Duration, path};

use axum::{
    extract::{Multipart, State, Query, Path},
    http::StatusCode,
    routing::{get, post, delete},
    Router, body::Bytes,
};
use chrono::prelude::*;

use serde::Deserialize;
use sqlx::{sqlite::SqlitePoolOptions, SqlitePool};
use tower_http::services::ServeDir;
use uuid::Uuid;
use futures::{Stream, TryStreamExt};
use std::io;
use tokio::{fs::File, io::BufWriter};
use tokio_util::io::StreamReader;


const PASTES_DIRECTORY: &str = "pastes";
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    tracing::info!("Starting server...");

    match run().await {
        Ok(_) => tracing::info!("Program exited successfully."),
        Err(e) => tracing::error!("Error: {}", e),
    }
}

async fn run() -> anyhow::Result<()> {
    let base_url: &'static str = std::env::var("BASE_URL")
    .map(|s| Box::leak(s.into_boxed_str()) as &str)
    .unwrap_or("http://127.0.0.1:3001");

    tokio::fs::create_dir_all(PASTES_DIRECTORY).await?;

    let db_connection_str =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "smolpaste.sqlite".to_string());

    tracing::info!("Opening database at \"{}\"...", &db_connection_str);
    let db = SqlitePoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(3))
        .connect(&db_connection_str)
        .await?;


    init_db(&db).await?;
    let state = Arc::new(AppState { db, base_url });

    let addr = std::env::var("SMOLPASTE_ADDR").unwrap_or_else(|_| "127.0.0.1:3001".to_string());
    let app = Router::new()
        .route("/new", post(new_paste))
        .route("/delete", delete(delete_paste))
        .nest_service("/paste",ServeDir::new(PASTES_DIRECTORY))
        .with_state(state);

    let listener = std::net::TcpListener::bind(addr)?;
    tracing::info!("Listening on {}...", listener.local_addr()?);

    axum::Server::from_tcp(listener)?
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

#[derive(Debug, Clone)]
pub struct AppState {
    db: SqlitePool,
    base_url: &'static str
}

pub async fn init_db(db: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS pastes (
        id TEXT PRIMARY KEY NOT NULL,
        size INTEGER,
        filename TEXT,
        timestamp INTEGER
    )")
    .execute(db).await?;

    sqlx::query("CREATE TABLE IF NOT EXISTS tokens (
        value TEXT,
        created_at INTEGER
)")
    .execute(db).await?;

    /*sqlx::query("INSERT INTO tokens (value, created_at) VALUES ($1, $2)")
    .bind("test")
    .bind(1699645888)
    .execute(db).await?;*/
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PasteInfo {
    id: Uuid,
    size: u32,
    filename: String,
    timestamp: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FileNameWrapper {
    filename: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TokenInfo {
    value: Uuid,
    created_at: i64
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenParam {
    token: String
}

#[derive(Debug, Clone, Deserialize)]
pub struct IdTokenParam {
    token: String,
    id: String
}


#[axum::debug_handler]
async fn new_paste(
    State(state): State<Arc<AppState>>,
    Query(token): Query<TokenParam>,
    mut multipart: Multipart,
) -> Result<String, StatusCode> {
    let res = sqlx::query_scalar::<_, i32>("SELECT COUNT(*) as count FROM tokens WHERE value = $1")
    .bind(token.token)
    .fetch_one(&state.db).await;

    match res {
        Ok(1) => {},
        Ok(0) => return Err(StatusCode::UNAUTHORIZED),
        Err(sqlx::Error::RowNotFound) => return Err(StatusCode::UNAUTHORIZED),
        _ => return Err(StatusCode::INTERNAL_SERVER_ERROR)
    };
    
    let id = uuid::Uuid::new_v4();
    let field = match multipart.next_field().await {
        Ok(Some(f)) => f,
        _ => return Err(StatusCode::BAD_REQUEST)
    };

    let upload_name = match field.file_name() {
        None => return Err(StatusCode::BAD_REQUEST),
        Some(n) => path::Path::new(n)
    };

    let filename = match upload_name.extension() {
        Some(e) => match e.to_str() {
            Some(e) => format!("{}.{}", id, e),
            None => return Err(StatusCode::BAD_REQUEST)
        },
        None => format!("{}", id)
    };

    let written = stream_to_file(&filename, field).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tracing::info!("Created a {} byte file.", written);

    let utc: DateTime<Utc> = Utc::now();

    let info = PasteInfo {
        id,
        size: written,
        filename,
        timestamp: utc.timestamp(),
    };

    sqlx::query("INSERT INTO pastes (
        id,
        size,
        filename,
        timestamp
    )VALUES (
        $1, $2, $3, $4
    )")
    .bind(info.id.to_string())
    .bind(info.size)
    .bind(&info.filename)
    .bind(info.timestamp)
    .execute(&state.db).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    tracing::info!("{}/paste/{}", state.base_url, info.filename);
    return Ok(format!("{}/paste/{}", state.base_url, info.filename))
}

#[axum::debug_handler]
async fn delete_paste(
    State(state): State<Arc<AppState>>,
    Query(query): Query<IdTokenParam>,
) -> Result<StatusCode, StatusCode> {
    let res = sqlx::query_scalar::<_, i32>("SELECT COUNT(*) as count FROM tokens WHERE value = $1")
    .bind(query.token)
    .fetch_one(&state.db).await;

    match res {
        Ok(1) => {},
        Ok(0) => return Err(StatusCode::UNAUTHORIZED),
        Err(sqlx::Error::RowNotFound) => return Err(StatusCode::UNAUTHORIZED),
        _ => return Err(StatusCode::INTERNAL_SERVER_ERROR)
    };

    let paste = match sqlx::query_as::<_, FileNameWrapper>("DELETE FROM pastes WHERE id = $1 RETURNING filename")
    .bind(&query.id)
    .fetch_one(&state.db)
    .await {
        Ok(f) => f,
        Err(sqlx::Error::RowNotFound) => return Err(StatusCode::NOT_FOUND),
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR)
    };

    tracing::info!("Deleting paste {}", &paste.filename);

    tokio::fs::remove_file(format!("{}/{}", PASTES_DIRECTORY, paste.filename))
    .await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(StatusCode::OK)
}

async fn stream_to_file<S, E>(path: &str, stream: S) -> anyhow::Result<u32>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: Into<anyhow::Error>,
{

    async {
        // Convert the stream into an `AsyncRead`.
        let body_with_io_error = stream.map_err(|_| io::Error::new(io::ErrorKind::Other, ""));
        let body_reader = StreamReader::new(body_with_io_error);
        futures::pin_mut!(body_reader);

        // Create the file. `File` implements `AsyncWrite`.
        let path = std::path::Path::new(PASTES_DIRECTORY).join(path);
        let mut file = BufWriter::new(File::create(path).await?);

        // Copy the body into the file.
        let total = tokio::io::copy(&mut body_reader, &mut file).await?;
        Ok(total as u32)
    }
    .await
}