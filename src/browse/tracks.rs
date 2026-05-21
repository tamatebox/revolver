//! Shared logic for building a DIDL Item from a track row.
//! Used by both Browse (under album) and Search.

use rusqlite::params;

use super::{BrowseContext, ChildrenResult, DidlOutput};
use crate::error::Result;
use crate::upnp::didl::{Item, Resource};

/// DB values for a single track row, as fetched by the `load_*` helpers.
pub(crate) struct TrackRow {
    pub album_id: i64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub genre: Option<String>,
    pub track_num: Option<i64>,
    pub disc_num: Option<i64>,
    pub duration_ms: Option<i64>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub channels: Option<i64>,
    pub bitrate: Option<i64>,
    pub mime_type: String,
    pub file_size: i64,
    pub album: String,
}

/// BrowseMetadata (`trk:{id}`). Returns a single Item.
pub fn track_metadata(ctx: &BrowseContext, track_id: i64) -> Result<DidlOutput> {
    let item = load_track_item(ctx, track_id)?;
    Ok(DidlOutput {
        containers: vec![],
        items: vec![item],
    })
}

/// BrowseDirectChildren (`alb:{id}`). Returns the track list.
pub fn album_tracks_children(
    ctx: &BrowseContext,
    album_id: i64,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM tracks WHERE album_id = ?1",
        params![album_id],
        |r| r.get(0),
    )?;
    let items = load_album_tracks(ctx, album_id, start, count)?;
    Ok(ChildrenResult {
        didl: DidlOutput {
            containers: vec![],
            items,
        },
        total_matches: total as usize,
    })
}

fn load_track_item(ctx: &BrowseContext, track_id: i64) -> Result<Item> {
    let row: TrackRow = ctx.conn.query_row(
        "SELECT t.album_id, t.title, t.artist, t.genre, t.track_num, t.disc_num,
                t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                t.bitrate, t.mime_type, t.file_size, a.album
         FROM tracks t JOIN albums a ON t.album_id = a.id
         WHERE t.id = ?1",
        params![track_id],
        |r| {
            Ok(TrackRow {
                album_id: r.get(0)?,
                title: r.get(1)?,
                artist: r.get(2)?,
                genre: r.get(3)?,
                track_num: r.get(4)?,
                disc_num: r.get(5)?,
                duration_ms: r.get(6)?,
                sample_rate: r.get(7)?,
                bit_depth: r.get(8)?,
                channels: r.get(9)?,
                bitrate: r.get(10)?,
                mime_type: r.get(11)?,
                file_size: r.get(12)?,
                album: r.get(13)?,
            })
        },
    )?;
    Ok(build_track_item(ctx, track_id, &row))
}

fn load_album_tracks(
    ctx: &BrowseContext,
    album_id: i64,
    start: usize,
    count: usize,
) -> Result<Vec<Item>> {
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT t.id, t.album_id, t.title, t.artist, t.genre, t.track_num, t.disc_num,
                t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                t.bitrate, t.mime_type, t.file_size, a.album
         FROM tracks t JOIN albums a ON t.album_id = a.id
         WHERE t.album_id = ?1
         ORDER BY t.disc_num, t.track_num
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows: Vec<(i64, TrackRow)> = stmt
        .query_map(params![album_id, count as i64, start as i64], |r| {
            Ok((
                r.get(0)?,
                TrackRow {
                    album_id: r.get(1)?,
                    title: r.get(2)?,
                    artist: r.get(3)?,
                    genre: r.get(4)?,
                    track_num: r.get(5)?,
                    disc_num: r.get(6)?,
                    duration_ms: r.get(7)?,
                    sample_rate: r.get(8)?,
                    bit_depth: r.get(9)?,
                    channels: r.get(10)?,
                    bitrate: r.get(11)?,
                    mime_type: r.get(12)?,
                    file_size: r.get(13)?,
                    album: r.get(14)?,
                },
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows
        .into_iter()
        .map(|(id, row)| build_track_item(ctx, id, &row))
        .collect())
}

/// Track row → DIDL Item. Used by both Browse and Search.
pub(crate) fn build_track_item(ctx: &BrowseContext, track_id: i64, row: &TrackRow) -> Item {
    let protocol_info = format!("http-get:*:{}:*", row.mime_type);
    Item {
        id: format!("trk:{track_id}"),
        parent_id: format!("alb:{}", row.album_id),
        title: row.title.clone().unwrap_or_else(|| "Unknown".to_string()),
        upnp_class: "object.item.audioItem.musicTrack",
        artist: row.artist.clone(),
        album: Some(row.album.clone()),
        genre: row.genre.clone(),
        original_track_number: row.track_num.map(|n| n as u32),
        original_disc_number: row.disc_num.filter(|&n| n > 0).map(|n| n as u32),
        album_art_uri: Some(format!("{}/{}", ctx.art_base_url, row.album_id)),
        res: Resource {
            url: format!("{}/{}", ctx.stream_base_url, track_id),
            protocol_info,
            size: row.file_size as u64,
            duration_ms: row.duration_ms.map(|n| n as u64),
            bitrate: row.bitrate.map(|n| n as u32),
            sample_frequency: row.sample_rate.map(|n| n as u32),
            bits_per_sample: row.bit_depth.map(|n| n as u8),
            nr_audio_channels: row.channels.map(|n| n as u8),
        },
    }
}
