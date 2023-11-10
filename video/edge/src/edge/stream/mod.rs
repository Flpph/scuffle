use std::sync::Arc;
use std::time::Duration;

use chrono::TimeZone;
use common::prelude::FutureTimeout;
use hyper::{Body, Request, Response, StatusCode};
use pb::scuffle::video::{
    internal::{LiveManifest, LiveRenditionManifest},
    v1::types::{AudioConfig, VideoConfig},
};
use prost::Message;
use routerify::{prelude::RequestExt, Router};
use tokio::io::AsyncReadExt;
use tokio::time::Instant;
use ulid::Ulid;
use uuid::Uuid;
use video_common::{
    database::{PgNonNullVec, Protobuf, Rendition, Room, RoomStatus},
    keys,
};
use video_player_types::SessionRefresh;

use self::tokens::{ScreenshotClaims, SessionClaims, SessionClaimsType};

use super::error::{Result, RouteError};
use crate::edge::stream::hls_config::HlsConfig;
use crate::edge::stream::tokens::MediaClaims;
use crate::edge::{error::ResultExt, macros::make_response};
use crate::{edge::ext::RequestExt as _, global::GlobalState};

mod block_style;
mod hls_config;
mod playlist;
mod tokens;

fn organization_id(req: &Request<Body>) -> Result<Ulid> {
    Ulid::from_string(req.param("organization_id").unwrap())
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid organization_id").into())
}

fn room_id(req: &Request<Body>) -> Result<Ulid> {
    Ulid::from_string(req.param("room_id").unwrap())
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid room_id").into())
}

fn recording_id(req: &Request<Body>) -> Result<Ulid> {
    Ulid::from_string(req.param("recording_id").unwrap())
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid recording_id").into())
}

fn rendition(req: &Request<Body>) -> Result<Rendition> {
    req.param("rendition")
        .unwrap()
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid rendition").into())
}

fn token(req: &Request<Body>) -> Option<String> {
    req.uri().query().and_then(|v| {
        url::form_urlencoded::parse(v.as_bytes()).find_map(|(k, v)| {
            if k == "token" {
                Some(v.to_string())
            } else {
                None
            }
        })
    })
}

async fn room_playlist(req: Request<Body>) -> Result<Response<Body>> {
    let config = HlsConfig::new(&req)?;

    let global = req.get_global()?;

    let organization_id = organization_id(&req)?;
    let room_id = room_id(&req)?;

    let token = if let Some(token) = token(&req) {
        Some(
            tokens::TokenClaims::verify(
                &global,
                organization_id,
                tokens::TargetId::Room(room_id),
                &token,
            )
            .await?,
        )
    } else {
        None
    };

    let room: Option<Room> = sqlx::query_as(
        "SELECT * FROM rooms WHERE organization_id = $1 AND id = $2 AND status != $3",
    )
    .bind(Uuid::from(organization_id))
    .bind(Uuid::from(room_id))
    .bind(RoomStatus::Offline)
    .fetch_optional(global.db.as_ref())
    .await
    .map_err_route((
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed to query database",
    ))?;

    let room = room.ok_or((StatusCode::NOT_FOUND, "room not found"))?;

    let connection_id = Ulid::from(
        room.active_ingest_connection_id
            .ok_or((StatusCode::NOT_FOUND, "room not found"))?,
    );

    let audio_output = room
        .audio_output
        .ok_or((StatusCode::NOT_FOUND, "room not found"))?;

    let video_output = room
        .video_output
        .ok_or((StatusCode::NOT_FOUND, "room not found"))?;

    if room.private && token.is_none() {
        return Err((
            StatusCode::UNAUTHORIZED,
            "room is private, token is required",
        )
            .into());
    }

    let id = Ulid::new();

    sqlx::query(
        r#"
    INSERT INTO playback_sessions (
        id,
        organization_id,
        room_id,
        user_id,
        playback_key_pair_id,
        issued_at,
        ip_address,
        user_agent,
        referer,
        origin,
        player_version
    ) VALUES (
        $1,
        $2,
        $3,
        $4,
        $5,
        $6,
        $7,
        $8,
        $9,
        $10,
        $11
    )
    "#,
    )
    .bind(Uuid::from(id))
    .bind(Uuid::from(organization_id))
    .bind(Uuid::from(room_id))
    .bind(token.as_ref().and_then(|t| t.claims().user_id.as_ref()))
    .bind(token.as_ref().and_then(|t| t.header().key_id.as_ref()))
    .bind(token.as_ref().and_then(|t| {
        chrono::Utc
            .timestamp_opt(t.claims().iat.unwrap(), 0)
            .single()
    }))
    .bind(req.remote_addr().ip().to_string())
    .bind(
        req.headers()
            .get("user-agent")
            .map(|v| v.to_str().unwrap_or_default()),
    )
    .bind(
        req.headers()
            .get("referer")
            .map(|v| v.to_str().unwrap_or_default()),
    )
    .bind(
        req.headers()
            .get("origin")
            .map(|v| v.to_str().unwrap_or_default()),
    )
    .bind(
        req.headers()
            .get("x-player-version")
            .map(|v| v.to_str().unwrap_or_default()),
    )
    .execute(global.db.as_ref())
    .await
    .map_err_route((
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed to create session",
    ))?;

    let manifest = playlist::room_playlist(
        &global,
        id,
        organization_id,
        connection_id,
        room_id,
        token.is_some(),
        audio_output.iter(),
        video_output.iter(),
    )?;

    let body = if config.scuffle_json {
        Body::from(serde_json::to_string(&manifest).map_err_route((
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encode playlist",
        ))?)
    } else {
        Body::from(manifest.to_m3u8(organization_id))
    };

    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        "Content-Type",
        if config.scuffle_json {
            "application/json"
        } else {
            "application/vnd.apple.mpegurl"
        }
        .parse()
        .unwrap(),
    );
    resp.headers_mut()
        .insert("Cache-Control", "no-cache".parse().unwrap());

    Ok(resp)
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct RecordingExt {
    public: bool,
    renditions: PgNonNullVec<Rendition>,
    configs: PgNonNullVec<Vec<u8>>,
}

async fn recording_playlist(req: Request<Body>) -> Result<Response<Body>> {
    let config = HlsConfig::new(&req)?;

    let global = req.get_global()?;

    let organization_id = organization_id(&req)?;
    let recording_id = recording_id(&req)?;

    let token = if let Some(token) = token(&req) {
        Some(
            tokens::TokenClaims::verify(
                &global,
                organization_id,
                tokens::TargetId::Recording(recording_id),
                &token,
            )
            .await?,
        )
    } else {
        None
    };

    let recording: Option<RecordingExt> = sqlx::query_as(
        r#"
        WITH filtered_recordings AS (
            SELECT
                id,
                public
            FROM recordings 
            WHERE 
                id = $1
                AND organization_id = $2
                AND deleted = FALSE
        )

        SELECT 
            r.public as public,
            ARRAY_AGG(rr.rendition) as renditions,
            ARRAY_AGG(rr.config) as configs
        FROM 
            filtered_recordings AS r
        INNER JOIN recording_renditions rr
            ON r.id = rr.recording_id 
        GROUP BY 
            r.public
    "#,
    )
    .bind(Uuid::from(recording_id))
    .bind(Uuid::from(organization_id))
    .fetch_optional(global.db.as_ref())
    .await
    .map_err_route((
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed to query database",
    ))?;

    let recording = recording.ok_or((StatusCode::NOT_FOUND, "recording not found"))?;

    if !recording.public && token.is_none() {
        return Err((
            StatusCode::UNAUTHORIZED,
            "recording is private, token is required",
        )
            .into());
    }

    let audio_output = recording
        .renditions
        .iter()
        .zip(recording.configs.iter())
        .filter_map(|(r, c)| {
            if r.is_audio() {
                Some(AudioConfig::decode(c.as_slice()))
            } else {
                None
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err_route((
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to decode audio configs",
        ))?;

    let video_output = recording
        .renditions
        .iter()
        .zip(recording.configs.iter())
        .filter_map(|(r, c)| {
            if r.is_video() {
                Some(VideoConfig::decode(c.as_slice()))
            } else {
                None
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err_route((
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to decode video configs",
        ))?;

    let id = Ulid::new();

    sqlx::query(
        r#"
    INSERT INTO playback_sessions (
        id,
        organization_id,
        recording_id,
        user_id,
        playback_key_pair_id,
        issued_at,
        ip_address,
        user_agent,
        referer,
        origin,
        player_version
    ) VALUES (
        $1,
        $2,
        $3,
        $4,
        $5,
        $6,
        $7,
        $8,
        $9,
        $10,
        $11
    )
    "#,
    )
    .bind(Uuid::from(id))
    .bind(Uuid::from(organization_id))
    .bind(Uuid::from(recording_id))
    .bind(token.as_ref().and_then(|t| t.claims().user_id.as_ref()))
    .bind(token.as_ref().and_then(|t| t.header().key_id.as_ref()))
    .bind(token.as_ref().and_then(|t| {
        chrono::Utc
            .timestamp_opt(t.claims().iat.unwrap(), 0)
            .single()
    }))
    .bind(req.remote_addr().ip().to_string())
    .bind(
        req.headers()
            .get("user-agent")
            .map(|v| v.to_str().unwrap_or_default()),
    )
    .bind(
        req.headers()
            .get("referer")
            .map(|v| v.to_str().unwrap_or_default()),
    )
    .bind(
        req.headers()
            .get("origin")
            .map(|v| v.to_str().unwrap_or_default()),
    )
    .bind(
        req.headers()
            .get("x-player-version")
            .map(|v| v.to_str().unwrap_or_default()),
    )
    .execute(global.db.as_ref())
    .await
    .map_err_route((
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed to create session",
    ))?;

    let manifest = playlist::recording_playlist(
        &global,
        id,
        organization_id,
        recording_id,
        token.is_some(),
        audio_output.into_iter().map(Protobuf),
        video_output.into_iter().map(Protobuf),
    )?;

    let body = if config.scuffle_json {
        Body::from(serde_json::to_string(&manifest).map_err_route((
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encode playlist",
        ))?)
    } else {
        Body::from(manifest.to_m3u8(organization_id))
    };

    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        "Content-Type",
        if config.scuffle_json {
            "application/json"
        } else {
            "application/vnd.apple.mpegurl"
        }
        .parse()
        .unwrap(),
    );
    resp.headers_mut()
        .insert("Cache-Control", "no-cache".parse().unwrap());

    Ok(resp)
}

async fn session_playlist(req: Request<Body>) -> Result<Response<Body>> {
    let config = HlsConfig::new(&req)?;
    let global = req.get_global()?;

    let organization_id = organization_id(&req)?;

    let session = req.param("session").unwrap();

    let rendition = rendition(&req)?;

    let session = SessionClaims::verify(&global, organization_id, session)?;

    let resp = sqlx::query(
        r#"
    UPDATE playback_sessions SET
        expires_at = NOW() + INTERVAL '10 minutes'
    WHERE
        id = $1 AND
        organization_id = $2 AND
        expires_at > NOW()
    "#,
    )
    .bind(Uuid::from(session.id))
    .bind(Uuid::from(session.organization_id))
    .execute(global.db.as_ref())
    .await
    .map_err_route((
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed to update session",
    ))?;

    if resp.rows_affected() == 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid session, expired or not found",
        )
            .into());
    }

    let manifest = if let SessionClaimsType::Room {
        room_id,
        connection_id,
    } = session.ty
    {
        let mut subscription = global
            .subscriber
            .subscribe_kv(keys::rendition_manifest(
                organization_id,
                room_id,
                connection_id,
                rendition,
            ))
            .await
            .map_err_route((StatusCode::INTERNAL_SERVER_ERROR, "failed to get manifest"))?;

        let mut manifest: LiveRenditionManifest;

        let now = Instant::now();

        loop {
            let result = subscription
                .next()
                .timeout(Duration::from_secs(2))
                .await
                .map_err_route((StatusCode::BAD_REQUEST, "manifest watch time timedout"))?
                .ok_or((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "manifest watch returned invalid value",
                ))?;

            manifest = LiveRenditionManifest::decode(result.value).map_err(|_| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to decode manifest",
                )
            })?;

            let info = manifest
                .info
                .as_ref()
                .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "manifest missing info"))?;

            if manifest.completed || !config.is_blocked(info) {
                break;
            }

            if now.elapsed() > Duration::from_secs(5) {
                return Err((StatusCode::BAD_REQUEST, "segment watch time timedout").into());
            }
        }

        Some(manifest)
    } else {
        None
    };

    let playlist =
        playlist::rendition_playlist(&global, &session, &config, rendition, manifest.as_ref())
            .await?;
    let body = if config.scuffle_json {
        Body::from(serde_json::to_string(&playlist).map_err_route((
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to encode playlist",
        ))?)
    } else {
        Body::from(playlist.to_m3u8(
            organization_id,
            match session.ty {
                SessionClaimsType::Room { room_id, .. } => Some(room_id),
                _ => None,
            },
        ))
    };

    let mut resp = Response::new(body);
    resp.headers_mut().insert(
        "Content-Type",
        if config.scuffle_json {
            "application/json"
        } else {
            "application/vnd.apple.mpegurl"
        }
        .parse()
        .unwrap(),
    );
    resp.headers_mut()
        .insert("Cache-Control", "no-cache".parse().unwrap());

    Ok(resp)
}

async fn session_refresh(req: Request<Body>) -> Result<Response<Body>> {
    let global = req.get_global()?;

    let organization_id = organization_id(&req)?;

    let session = req.param("session").unwrap();

    let session = SessionClaims::verify(&global, organization_id, session)?;

    let resp = sqlx::query(
        r#"
    UPDATE playback_sessions SET
        expires_at = NOW() + INTERVAL '10 minutes'
    WHERE
        id = $1 AND
        organization_id = $2 AND
        expires_at > NOW()
    "#,
    )
    .bind(Uuid::from(session.id))
    .bind(Uuid::from(session.organization_id))
    .execute(global.db.as_ref())
    .await
    .map_err_route((
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed to update session",
    ))?;

    if resp.rows_affected() == 0 {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid session, expired or not found",
        )
            .into());
    }

    let mut resp = Response::new(Body::from(
        serde_json::to_vec(&SessionRefresh { success: true }).unwrap(),
    ));
    resp.headers_mut()
        .insert("Content-Type", "application/json".parse().unwrap());
    resp.headers_mut()
        .insert("Cache-Control", "no-cache".parse().unwrap());

    Ok(resp)
}

async fn room_media(req: Request<Body>) -> Result<Response<Body>> {
    let global = req.get_global()?;

    let organization_id = organization_id(&req)?;

    let room_id = room_id(&req)?;

    let media = req.param("media").unwrap();

    let claims = MediaClaims::verify(&global, organization_id, room_id, media)?;

    let mut subscriber = global
        .subscriber
        .subscribe_kv(keys::rendition_manifest(
            organization_id,
            room_id,
            claims.connection_id,
            claims.rendition,
        ))
        .await
        .map_err_route((StatusCode::INTERNAL_SERVER_ERROR, "failed to get manifest"))?;

    let keys: Vec<String>;
    let now = Instant::now();
    loop {
        if now.elapsed() > Duration::from_secs(5) {
            return Err((StatusCode::BAD_REQUEST, "media watch time timedout").into());
        }

        let playlist = subscriber.next().await.ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "manifest watch returned invalid value",
        ))?;
        let playlist = LiveRenditionManifest::decode(playlist.value).map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to decode manifest",
            )
        })?;
        let info = playlist
            .info
            .as_ref()
            .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "manifest missing info"))?;

        keys = match claims.ty {
            tokens::MediaClaimsType::Init => vec![keys::init(
                organization_id,
                room_id,
                claims.connection_id,
                claims.rendition,
            )],
            tokens::MediaClaimsType::Part(idx) => {
                if info.next_part_idx <= idx {
                    if playlist.completed {
                        return Err(RouteError::from(make_response!(
                            StatusCode::NOT_FOUND,
                            serde_json::json!({
                                "message": "part not found",
                                "finished": true,
                                "success": false,
                            })
                        )));
                    }

                    continue;
                }

                if playlist
                    .segments
                    .first()
                    .and_then(|s| s.parts.first())
                    .map(|p| p.idx)
                    .unwrap_or(0)
                    > idx
                {
                    return Err((StatusCode::NOT_FOUND, "part not found").into());
                }

                vec![keys::part(
                    organization_id,
                    room_id,
                    claims.connection_id,
                    claims.rendition,
                    idx,
                )]
            }
            tokens::MediaClaimsType::Segment(idx) => {
                // The segment is still being written, so we return not found, this should not be possible.
                if info.next_segment_idx == idx + 1 {
                    tracing::warn!(idx, "segment not found");
                    return Err((StatusCode::NOT_FOUND, "segment not found").into());
                }

                let Some(segment) = playlist.segments.iter().find(|s| s.idx == idx) else {
                    return Err((StatusCode::NOT_FOUND, "segment not found").into());
                };

                segment
                    .parts
                    .iter()
                    .map(|p| {
                        keys::part(
                            organization_id,
                            room_id,
                            claims.connection_id,
                            claims.rendition,
                            p.idx,
                        )
                    })
                    .collect()
            }
        };

        break;
    }

    drop(subscriber);

    // Streaming response
    let mut data = Vec::new();

    for key in &keys {
        let mut item = global
            .media_store
            .get(key)
            .await
            .map_err_route((StatusCode::INTERNAL_SERVER_ERROR, "failed to get media"))?;

        item.read_to_end(&mut data)
            .timeout(Duration::from_secs(2))
            .await
            .map_err_route((
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read media, timedout",
            ))?
            .map_err_route((
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read media, unknown",
            ))?;
    }

    let mut resp = Response::new(Body::from(data));
    resp.headers_mut()
        .insert("Content-Type", "video/mp4".parse().unwrap());
    resp.headers_mut()
        .insert("Cache-Control", "max-age=31536000".parse().unwrap());

    Ok(resp)
}

async fn room_screenshot(req: Request<Body>) -> Result<Response<Body>> {
    let global = req.get_global()?;

    let organization_id = organization_id(&req)?;
    let room_id = room_id(&req)?;
    let token = if let Some(token) = token(&req) {
        Some(
            tokens::TokenClaims::verify(
                &global,
                organization_id,
                tokens::TargetId::Room(room_id),
                &token,
            )
            .await?,
        )
    } else {
        None
    };

    let room: Option<Room> = sqlx::query_as(
        "SELECT * FROM rooms WHERE organization_id = $1 AND id = $2 AND status != $3",
    )
    .bind(Uuid::from(organization_id))
    .bind(Uuid::from(room_id))
    .bind(RoomStatus::Offline)
    .fetch_optional(global.db.as_ref())
    .await
    .map_err_route((
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed to query database",
    ))?;

    let room = room.ok_or((StatusCode::NOT_FOUND, "room not found"))?;

    let connection_id = Ulid::from(
        room.active_ingest_connection_id
            .ok_or((StatusCode::NOT_FOUND, "room not found"))?,
    );

    if room.private && token.is_none() {
        return Err((
            StatusCode::UNAUTHORIZED,
            "room is private, token is required",
        )
            .into());
    }

    // We have permission to see the screenshot.
    let manifest = global
        .metadata_store
        .get(keys::manifest(organization_id, room_id, connection_id))
        .await
        .map_err_route((StatusCode::INTERNAL_SERVER_ERROR, "failed to get manifest"))?
        .ok_or((StatusCode::NOT_FOUND, "manifest not found"))?;

    let manifest = LiveManifest::decode(manifest).map_err_route((
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed to decode manifest",
    ))?;

    let screenshot = ScreenshotClaims {
        connection_id,
        idx: manifest.screenshot_idx,
        organization_id,
        room_id,
    }
    .sign(&global)?;

    let mut response = Response::new(Body::default());

    *response.status_mut() = StatusCode::TEMPORARY_REDIRECT;

    let url = format!("/{organization_id}/{room_id}/{screenshot}.jpg");

    response
        .headers_mut()
        .insert("Location", url.parse().unwrap());

    response
        .headers_mut()
        .insert("Cache-Control", "no-cache".parse().unwrap());

    Ok(response)
}

async fn room_screenshot_media(req: Request<Body>) -> Result<Response<Body>> {
    let global = req.get_global()?;

    let organization_id = Ulid::from_string(req.param("organization_id").unwrap())
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid organization_id"))?;

    let room_id = Ulid::from_string(req.param("room_id").unwrap())
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid room_id"))?;

    let screenshot = req.param("screenshot").unwrap();

    let claims = ScreenshotClaims::verify(&global, organization_id, room_id, screenshot)?;

    if claims.organization_id != organization_id {
        return Err((
            StatusCode::BAD_REQUEST,
            "invalid media, organization_id mismatch",
        )
            .into());
    }

    if claims.room_id != room_id {
        return Err((StatusCode::BAD_REQUEST, "invalid media, room_name mismatch").into());
    }

    let key = keys::screenshot(organization_id, room_id, claims.connection_id, claims.idx);

    tracing::debug!(key = %key, "getting screenshot");

    let mut item = global.media_store.get(&key).await.map_err_route((
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed to get screenshot",
    ))?;

    let mut buf = Vec::new();

    item.read_to_end(&mut buf).await.map_err_route((
        StatusCode::INTERNAL_SERVER_ERROR,
        "failed to read screenshot",
    ))?;

    let mut resp = Response::new(Body::from(buf));
    resp.headers_mut()
        .insert("Content-Type", "image/jpeg".parse().unwrap());
    resp.headers_mut()
        .insert("Cache-Control", "max-age=31536000".parse().unwrap());

    Ok(resp)
}

pub fn routes(_: &Arc<GlobalState>) -> Router<Body, RouteError> {
    Router::builder()
        .get("/:organization_id/:room_id.m3u8", room_playlist)
        .get("/:organization_id/r/:recording_id.m3u8", recording_playlist)
        .get(
            "/:organization_id/:session/:rendition.m3u8",
            session_playlist,
        )
        .get("/:organization_id/:session/refresh", session_refresh)
        .get("/:organization_id/:room_id.jpg", room_screenshot)
        .get("/:organization_id/:room_id/:media.mp4", room_media)
        .get(
            "/:organization_id/:room_id/:screenshot.jpg",
            room_screenshot_media,
        )
        .build()
        .expect("failed to build router")
}