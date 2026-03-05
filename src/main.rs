include!("pb/events.rs");

use lazy_static::lazy_static;
use std::error::Error;
use std::fs;
use std::sync::Arc;

use axum::{
    Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};
use base64::{self, Engine, engine::general_purpose::URL_SAFE};
use prost::Message;
use rusqlite::Connection;
use tokio::sync::Mutex;

const ADMIN_PWD: &str = "kxpage-password";
const IMAGE_STORE: &str = "./assets/images";

lazy_static! {
    static ref ADMIN_HASH: String = {
        use sha2::{Digest, Sha512};
        let mut hasher = Sha512::new();
        hasher.update(ADMIN_PWD.as_bytes());
        format!("{:x}", hasher.finalize())
    };
}

#[derive(Clone)]
struct AppState {
    database: Arc<Mutex<Connection>>,
}

fn scan_storage_info() -> StorageInfo {
    use std::path::Path;
    let images_dir = Path::new(IMAGE_STORE);

    if !images_dir.exists() {
        return StorageInfo {
            size: 0,
            count: 0,
            files: vec![],
        };
    }

    let entries = match fs::read_dir(images_dir) {
        Ok(entries) => entries,
        Err(_) => {
            return StorageInfo {
                size: 0,
                count: 0,
                files: vec![],
            };
        }
    };

    let mut files = Vec::new();
    let mut total_size: u64 = 0;

    for entry in entries.flatten() {
        let path = entry.path();

        if path.is_file() {
            if let Some(filename) = path.file_name() {
                if let Some(name) = filename.to_str() {
                    files.push(name.to_string());

                    // 获取文件大小
                    if let Ok(metadata) = fs::metadata(&path) {
                        total_size += metadata.len();
                    }
                }
            }
        }
    }

    let count = files.len() as u32;

    StorageInfo {
        size: total_size,
        count,
        files,
    }
}

fn connect_to_database() -> Result<Connection, Box<dyn Error>> {
    let conn = Connection::open("a.db")?;

    conn.execute(
        "CREATE TABLE IF NOT EXISTS events (
        uuid VARCHAR(36) PRIMARY KEY,
        time TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
        title VARCHAR(255) NOT NULL,
        href TEXT DEFAULT NULL,
        image_hash VARCHAR(70) DEFAULT NULL,
        desc TEXT DEFAULT NULL
        )",
        (),
    )?;

    Ok(conn)
}

// 返回的 Result 会在 OK 的情况下返回前面的 IntoResponse，Err 返回后面的请求，前端拿到的都是请求

async fn get_event(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<impl IntoResponse, (StatusCode, Vec<u8>)> {
    let date = if let Some(q) = params.get("q") {
        match Engine::decode(&URL_SAFE, &q) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => q.clone(),
            },
            Err(_) => q.clone(),
        }
    } else {
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
    };

    let conn = state.database.lock().await;

    let mut stmt = conn
        .prepare(
            "SELECT uuid, time, title, href, desc, image_hash
             FROM events
             WHERE time >= datetime(?1, '-6 months')
               AND time < ?1
             ORDER BY time DESC",
        )
        .map_err(|e| {
            eprintln!("{}", e);
            let resp = StateResponse {
                message: "failed".to_string(),
            };
            (StatusCode::INTERNAL_SERVER_ERROR, resp.encode_to_vec())
        })?;

    let events_iter = stmt
        .query_map([&date], |row| {
            Ok(EventSpec {
                event_uuid: row.get(0)?,
                event_title: row.get(2)?,
                event_time: row.get(1)?,
                event_href: row.get::<usize, Option<String>>(3)?.unwrap_or_default(),
                event_description: row.get::<usize, Option<String>>(4)?.unwrap_or_default(),
                image_hash: row.get::<usize, Option<String>>(5)?.unwrap_or_default(),
            })
        })
        .map_err(|e| {
            eprintln!("{}", e);
            let resp = StateResponse {
                message: "failed".to_string(),
            };
            (StatusCode::INTERNAL_SERVER_ERROR, resp.encode_to_vec())
        })?;

    let events: Vec<EventSpec> = events_iter
        .collect::<Result<Vec<EventSpec>, rusqlite::Error>>()
        .map_err(|e| {
            eprintln!("{}", e);
            let resp = StateResponse {
                message: "failed".to_string(),
            };
            (StatusCode::INTERNAL_SERVER_ERROR, resp.encode_to_vec())
        })?;

    let events = EventList {
        events: events
            .into_iter()
            .map(|mut event| {
                // convert stored timestamp "YYYY-MM-DD HH:MM:SS" into "YYYY/MM/DD"
                if let Ok(dt) =
                    chrono::NaiveDateTime::parse_from_str(&event.event_time, "%Y-%m-%d %H:%M:%S")
                {
                    event.event_time = dt.format("%Y/%m/%d").to_string();
                }
                event
            })
            .collect(),
    };

    // 这里就是那个序列化
    let data = events.encode_to_vec();

    Ok((
        HeaderMap::from_iter([(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        )]),
        data,
    ))
}

async fn put_event(
    State(state): State<AppState>,
    data: Bytes,
) -> Result<impl IntoResponse, (StatusCode, Vec<u8>)> {
    let event_update = EventUpdate::decode(data.as_ref()).map_err(|_| {
        let resp = StateResponse {
            message: "failed".to_string(),
        };
        (StatusCode::INTERNAL_SERVER_ERROR, resp.encode_to_vec())
    })?;

    if event_update.token != *ADMIN_HASH {
        let resp = StateResponse {
            message: "failed".to_string(),
        };
        return Err((StatusCode::UNAUTHORIZED, resp.encode_to_vec()));
    }

    if event_update.event.is_none() {
        let resp = StateResponse {
            message: "failed".to_string(),
        };
        return Err((StatusCode::UNAUTHORIZED, resp.encode_to_vec()));
    };

    let event = event_update.event.unwrap();

    let mut changes: Vec<String> = vec![];

    changes.push(format!("title='{}'", event.event_title));
    changes.push(format!("time='{}'", event.event_time));

    if !event.event_href.is_empty() {
        changes.push(format!("href='{}'", event.event_href));
    }

    if !event.event_description.is_empty() {
        changes.push(format!("desc='{}'", event.event_description));
    }

    if !event.image_hash.is_empty() {
        changes.push(format!("image_hash='{}'", event.image_hash));
    }

    println!("{:?}", changes);

    let conn = state.database.lock().await;

    // build update statement dynamically because rusqlite doesn't support
    // binding an entire SET clause as a single placeholder
    let sql = format!(
        "UPDATE events SET {} WHERE uuid = ?1;",
        changes.join(", ")
    );

    let mut stmt = conn.prepare(&sql).map_err(|e| {
        eprintln!("{}", e);
        let resp = StateResponse {
            message: "failed".to_string(),
        };
        (StatusCode::INTERNAL_SERVER_ERROR, resp.encode_to_vec())
    })?;

    stmt.execute([&event.event_uuid])
        .map_err(|e| {
            eprintln!("{}", e);
            let resp = StateResponse {
                message: "failed".to_string(),
            };
            (StatusCode::BAD_REQUEST, resp.encode_to_vec())
        })?;

    let resp = StateResponse {
        message: "success".to_string(),
    };

    Ok((
        HeaderMap::from_iter([(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        )]),
        resp.encode_to_vec(),
    ))
}

async fn delete_event(
    State(state): State<AppState>,
    data: Bytes,
) -> Result<impl IntoResponse, (StatusCode, Vec<u8>)> {
    let event_delete = EventDelete::decode(data.as_ref()).map_err(|_| {
        let resp = StateResponse {
            message: "failed".to_string(),
        };
        (StatusCode::INTERNAL_SERVER_ERROR, resp.encode_to_vec())
    })?;

    if event_delete.token != *ADMIN_HASH {
        let resp = StateResponse {
            message: "failed".to_string(),
        };
        return Err((StatusCode::UNAUTHORIZED, resp.encode_to_vec()));
    }

    let conn = state.database.lock().await;

    let delete_collection = event_delete.uuids.join(", ");

    let mut stmt = conn
        .prepare("DELETE FROM events WHERE uuid IN (?1);")
        .map_err(|e| {
            eprintln!("{}", e);
            let resp = StateResponse {
                message: "failed".to_string(),
            };
            (StatusCode::INTERNAL_SERVER_ERROR, resp.encode_to_vec())
        })?;

    stmt.execute((&delete_collection,)).map_err(|e| {
        eprintln!("{}", e);
        let resp = StateResponse {
            message: "failed".to_string(),
        };
        (StatusCode::BAD_REQUEST, resp.encode_to_vec())
    })?;

    let resp = StateResponse {
        message: "success".to_string(),
    };

    Ok((
        HeaderMap::from_iter([(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        )]),
        resp.encode_to_vec(),
    ))
}

async fn post_event(
    State(state): State<AppState>,
    data: Bytes,
) -> Result<impl IntoResponse, (StatusCode, Vec<u8>)> {
    let event_post = EventPost::decode(data.as_ref()).map_err(|_| {
        let resp = StateResponse {
            message: "failed".to_string(),
        };
        (StatusCode::INTERNAL_SERVER_ERROR, resp.encode_to_vec())
    })?;

    if event_post.token != *ADMIN_HASH {
        let resp = StateResponse {
            message: "failed".to_string(),
        };
        return Err((StatusCode::UNAUTHORIZED, resp.encode_to_vec()));
    }

    let conn = state.database.lock().await;

    for event in event_post.events {
        let date =
            chrono::NaiveDate::parse_from_str(&event.event_time, "%Y/%m/%d").map_err(|_| {
                let resp = StateResponse {
                    message: "failed".to_string(),
                };
                (StatusCode::INTERNAL_SERVER_ERROR, resp.encode_to_vec())
            })?;
        let datetime = date.and_hms_opt(0, 0, 0).unwrap();

        let href = if event.event_href.is_empty() {
            None
        } else {
            Some(event.event_href.as_str())
        };
        let image_hash = if event.image_hash.is_empty() {
            None
        } else {
            Some(event.image_hash.as_str())
        };

        let mut stmt = conn
            .prepare(
                "INSERT INTO events (uuid, time, title, href, image_hash, desc)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(|e| {
                let resp = StateResponse {
                    message: "failed".to_string(),
                };
                eprintln!("{}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, resp.encode_to_vec())
            })?;

        stmt.execute((
            &event.event_uuid,
            &datetime.format("%Y-%m-%d %H:%M:%S").to_string(),
            &event.event_title,
            href.as_ref(),
            image_hash.as_ref(),
            &event.event_description,
        ))
        .map_err(|e| {
            let resp = StateResponse {
                message: "failed".to_string(),
            };
            eprintln!("{}", e);
            (StatusCode::BAD_REQUEST, resp.encode_to_vec())
        })?;
    }

    let resp = StateResponse {
        message: "success".to_string(),
    };

    Ok((
        HeaderMap::from_iter([(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        )]),
        resp.encode_to_vec(),
    ))
}

// 为什么，这个接口在 img 下？
async fn get_storage_info(data: Bytes) -> Result<impl IntoResponse, (StatusCode, Vec<u8>)> {
    let admin_token = AdminToken::decode(data.as_ref()).map_err(|_| {
        let resp = StateResponse {
            message: "failed".to_string(),
        };
        (StatusCode::BAD_REQUEST, resp.encode_to_vec())
    })?;

    if admin_token.token != *ADMIN_HASH {
        let resp = StateResponse {
            message: "failed".to_string(),
        };
        return Err((StatusCode::UNAUTHORIZED, resp.encode_to_vec()));
    }
    let storage_info = scan_storage_info();

    Ok((
        HeaderMap::from_iter([(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        )]),
        storage_info.encode_to_vec(),
    ))
}

async fn get_image(
    Path(image_hash_and_ext): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Vec<u8>)> {
    use std::path::PathBuf;
    let mut image_path = PathBuf::from(IMAGE_STORE);
    image_path.push(&image_hash_and_ext);
    let data = fs::read(image_path).map_err(|_| {
        let resp = StateResponse {
            message: "Image not found.".to_string(),
        };
        (StatusCode::INTERNAL_SERVER_ERROR, resp.encode_to_vec())
    })?;
    let ext = image_hash_and_ext.split(".").last().unwrap();
    Ok((
        HeaderMap::from_iter([(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&format!("image/{}", ext)).unwrap(),
        )]),
        data,
    ))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let conn = Arc::new(Mutex::new(connect_to_database()?));

    let state = AppState { database: conn };

    let app = Router::new()
        .route("/api/v1/images/info", post(get_storage_info))
        .route("/api/v1/events", get(get_event))
        .route(
            "/api/v1/events",
            post(post_event).put(put_event).delete(delete_event),
        )
        .route("/api/v1/img", get(get_image))
        .with_state(state);

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 8000));

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();

    Ok(())
}
