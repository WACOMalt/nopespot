//! Post-capture Ogg Vorbis tagging and library organization.
//!
//! When a track finishes playing, the forked `librespot-playback` has written the raw, decrypted
//! Ogg Vorbis stream to `capture_<track_id>.ogg` (published atomically from a `.partial` file, so
//! its appearance is a reliable "capture complete" signal - see `librespot-playback`'s `tee.rs`).
//!
//! This module takes the metadata ncspot already knows about that track (from the Spotify Web API,
//! via the [`Track`] model), waits for the capture file to be finalized, writes standard Vorbis
//! Comment tags + embeds the album art, and finally moves the file into a tidy library layout:
//!
//! ```text
//! <root>/<AlbumArtist>/<Album>/<Disc#>.<Track##> - <Title>.ogg
//! ```
//!
//! All of this happens on a dedicated, detached background thread spawned per finished track, so it
//! never blocks the audio playback thread or the librespot capture-writer thread.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::{Accessor, AudioFile, ItemKey};
use lofty::tag::{Tag, TagType};
use log::{debug, error, info, warn};

use rspotify::model::Id;

use crate::model::track::Track;
use crate::spotify_api::WebApi;

/// How long to wait for the librespot capture writer to finalize (`.partial` -> final `.ogg`)
/// before giving up. Tagging is scheduled at track-load time; the fork finalizes the capture as
/// soon as the whole stream has been downloaded/decrypted (usually within a few seconds, far
/// faster than real-time playback), but we allow a generous window to cover long tracks on slow
/// connections. This is a cheap background poll, so a large timeout costs nothing when idle.
const CAPTURE_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
/// Poll interval while waiting for the capture file to appear.
const CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// A self-contained snapshot of everything needed to tag and organize one captured track. This is
/// cloned out of the [`Track`] on the caller's thread so the background thread owns all its data
/// and needs no further access to shared ncspot state.
#[derive(Clone, Debug)]
pub struct TagRequest {
    /// Spotify base62 track id, used to locate the `capture_<id>.ogg` file.
    pub track_id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub album_artists: Vec<String>,
    pub track_number: u32,
    pub disc_number: i32,
    /// Release year/date string if known (e.g. "2019" or "2019-03-01").
    pub date: Option<String>,
    /// Genre strings, if known.
    pub genres: Vec<String>,
    /// ISRC, if known.
    pub isrc: Option<String>,
    /// URL of the album cover art to embed, if any.
    pub cover_url: Option<String>,
    /// Directory the capture file is written to (where librespot ran), and under which the
    /// organized library tree is created.
    pub output_root: PathBuf,
}

impl TagRequest {
    /// Build a [`TagRequest`] from a [`Track`]. `output_root` is the directory in which capture
    /// files are produced and under which the organized `<AlbumArtist>/<Album>/...` tree is built.
    pub fn from_track(track: &Track, output_root: PathBuf) -> Option<Self> {
        // Local files and anything without a Spotify id can't have a capture file, skip them.
        let track_id = track.id.clone()?;
        Some(Self {
            track_id,
            title: track.title.clone(),
            artists: track.artists.clone(),
            album: track.album.clone(),
            album_artists: track.album_artists.clone(),
            track_number: track.track_number,
            disc_number: track.disc_number,
            // `Track` doesn't currently carry release date / genre / isrc; these are filled in by
            // an optional Web API lookup on the tagging thread (see `enrich`).
            date: None,
            genres: Vec::new(),
            isrc: None,
            cover_url: track.cover_url.clone(),
            output_root,
        })
    }

    /// Path to the finalized capture file for this track.
    fn capture_path(&self) -> PathBuf {
        self.output_root
            .join(format!("capture_{}.ogg", self.track_id))
    }
}

/// Spawn a detached background thread that waits for the capture file, tags it, embeds album art,
/// and moves it into the organized library layout. Never blocks the caller.
///
/// `api` is an optional clone of the Spotify [`WebApi`]; if provided, the thread will enrich the
/// request with fields not carried by [`Track`] (release date, genres, ISRC) via a one-shot lookup.
/// All of this happens on the spawned thread, never on the audio/capture path.
pub fn spawn_tag_and_organize(mut request: TagRequest, api: Option<WebApi>) {
    thread::Builder::new()
        .name("ncspot-tagging".into())
        .spawn(move || {
            if let Some(api) = api.as_ref() {
                enrich(&mut request, api);
            }
            if let Err(e) = tag_and_organize(&request) {
                warn!(
                    "tagging: failed to tag/organize capture for track {}: {e}",
                    request.track_id
                );
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| error!("tagging: failed to spawn tagging thread: {e}"));
}

/// Fill in release date, genres, and ISRC via the Spotify Web API. Best-effort: any lookup failure
/// simply leaves the corresponding field(s) empty, and tagging proceeds with what's available.
fn enrich(request: &mut TagRequest, api: &WebApi) {
    if let Ok(full_track) = api.track(&request.track_id) {
        // ISRC lives in external_ids under the "isrc" key.
        if request.isrc.is_none() {
            request.isrc = full_track.external_ids.get("isrc").cloned();
        }
        // The simplified album on a FullTrack carries the release date.
        if request.date.is_none() {
            request.date = full_track.album.release_date.clone();
        }
        // Fetch the full album for genres (and release date as a fallback).
        if let Some(album_id) = full_track.album.id.as_ref() {
            if let Ok(full_album) = api.album(album_id.id()) {
                if request.genres.is_empty() {
                    request.genres = full_album.genres.clone();
                }
                if request.date.is_none() {
                    request.date = Some(full_album.release_date.clone());
                }
            }
        }
    }
}

/// Wait for the capture file to be finalized, then tag and move it.
fn tag_and_organize(request: &TagRequest) -> Result<(), String> {
    let capture_path = request.capture_path();

    wait_for_capture(&capture_path)?;

    // Download album art (best effort) before opening the file for tagging.
    let cover = request
        .cover_url
        .as_deref()
        .and_then(|url| download_cover(url));

    write_tags(&capture_path, request, cover)?;

    let dest = organized_destination(request);
    move_file(&capture_path, &dest)?;

    info!("tagging: wrote tagged capture to {dest:?}");
    Ok(())
}

/// Block (on this background thread only) until the finalized capture file exists, or the timeout
/// elapses.
fn wait_for_capture(capture_path: &Path) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if capture_path.exists() {
            return Ok(());
        }
        if start.elapsed() >= CAPTURE_WAIT_TIMEOUT {
            return Err(format!(
                "timed out after {CAPTURE_WAIT_TIMEOUT:?} waiting for capture file {capture_path:?}"
            ));
        }
        thread::sleep(CAPTURE_POLL_INTERVAL);
    }
}

/// Download album art from `url`, returning a lofty [`Picture`] on success. Best-effort: any
/// failure just yields `None` and tagging proceeds without art.
fn download_cover(url: &str) -> Option<Picture> {
    let resp = match reqwest::blocking::get(url) {
        Ok(r) => r,
        Err(e) => {
            debug!("tagging: could not fetch cover {url}: {e}");
            return None;
        }
    };

    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let bytes = match resp.bytes() {
        Ok(b) => b.to_vec(),
        Err(e) => {
            debug!("tagging: could not read cover bytes from {url}: {e}");
            return None;
        }
    };

    if bytes.len() < 8 {
        return None;
    }

    // Prefer sniffing the mime type from the bytes; fall back to the HTTP Content-Type header.
    let mut picture = match Picture::from_reader(&mut Cursor::new(bytes.clone())) {
        Ok(p) => p,
        Err(_) => {
            let mime_type = match mime.as_deref() {
                Some("image/png") => MimeType::Png,
                _ => MimeType::Jpeg,
            };
            Picture::unchecked(bytes).mime_type(mime_type).build()
        }
    };
    picture.set_pic_type(PictureType::CoverFront);
    Some(picture)
}

/// Open the captured Ogg file, apply Vorbis Comment tags mapped from Spotify metadata, embed the
/// cover art if present, and save in place.
fn write_tags(
    capture_path: &Path,
    request: &TagRequest,
    cover: Option<Picture>,
) -> Result<(), String> {
    let mut tagged_file = lofty::read_from_path(capture_path)
        .map_err(|e| format!("could not read capture file for tagging: {e}"))?;

    // Ogg Vorbis always uses Vorbis Comments; start from a clean tag we fully control.
    let mut tag = Tag::new(TagType::VorbisComments);

    // Single-valued common fields via the Accessor helpers.
    tag.set_title(request.title.clone());
    if let Some(album) = &request.album {
        tag.set_album(album.clone());
    }
    if request.track_number > 0 {
        tag.set_track(request.track_number);
    }
    if request.disc_number > 0 {
        tag.set_disk(request.disc_number as u32);
    }
    if let Some(date) = &request.date {
        // Store the raw date string under DATE (RecordingDate maps to DATE for Vorbis).
        tag.insert_text(ItemKey::RecordingDate, date.clone());
    }

    // Multi-valued fields: push one Vorbis comment per value (ARTIST, ALBUMARTIST, GENRE).
    for artist in &request.artists {
        tag.push(lofty::tag::TagItem::new(
            ItemKey::TrackArtist,
            lofty::tag::ItemValue::Text(artist.clone()),
        ));
    }
    for album_artist in &request.album_artists {
        tag.push(lofty::tag::TagItem::new(
            ItemKey::AlbumArtist,
            lofty::tag::ItemValue::Text(album_artist.clone()),
        ));
    }
    for genre in &request.genres {
        tag.push(lofty::tag::TagItem::new(
            ItemKey::Genre,
            lofty::tag::ItemValue::Text(genre.clone()),
        ));
    }
    if let Some(isrc) = &request.isrc {
        tag.insert_text(ItemKey::Isrc, isrc.clone());
    }

    if let Some(cover) = cover {
        tag.push_picture(cover);
    }

    // Replace any tag that may already exist and write to disk.
    tagged_file.insert_tag(tag);
    tagged_file
        .save_to_path(capture_path, WriteOptions::default())
        .map_err(|e| format!("could not write tags to capture file: {e}"))?;

    Ok(())
}

/// Compute the organized destination path:
/// `<root>/<AlbumArtist>/<Album>/<Disc#>.<Track##> - <Title>.ogg`.
fn organized_destination(request: &TagRequest) -> PathBuf {
    let album_artist = request
        .album_artists
        .first()
        .or_else(|| request.artists.first())
        .map(|s| s.as_str())
        .unwrap_or("Unknown Artist");
    let album = request.album.as_deref().unwrap_or("Unknown Album");

    let disc = if request.disc_number > 0 {
        request.disc_number
    } else {
        1
    };

    let filename = format!(
        "{}.{:02} - {}.ogg",
        disc,
        request.track_number,
        sanitize(&request.title)
    );

    request
        .output_root
        .join(sanitize(album_artist))
        .join(sanitize(album))
        .join(sanitize_filename(&filename))
}

/// Move `src` to `dest`, creating parent directories. Falls back to copy+remove if a plain rename
/// fails (e.g. across filesystems).
fn move_file(src: &Path, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("could not create destination directory {parent:?}: {e}"))?;
    }

    // Don't clobber an existing organized file (e.g. the track was captured before).
    let dest = unique_destination(dest);

    match fs::rename(src, &dest) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Cross-device or other rename failure: copy then remove.
            fs::copy(src, &dest)
                .map_err(|e| format!("could not copy capture to {dest:?}: {e}"))?;
            fs::remove_file(src)
                .map_err(|e| format!("could not remove source capture {src:?}: {e}"))?;
            Ok(())
        }
    }
}

/// If `dest` already exists, append ` (2)`, ` (3)`, ... before the extension until a free name is
/// found, so re-captures never overwrite an existing organized file.
fn unique_destination(dest: &Path) -> PathBuf {
    if !dest.exists() {
        return dest.to_path_buf();
    }
    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let stem = dest
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("capture");
    let ext = dest.extension().and_then(|s| s.to_str()).unwrap_or("ogg");
    for n in 2..10_000 {
        let candidate = parent.join(format!("{stem} ({n}).{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dest.to_path_buf()
}

/// Sanitize a single path component (directory or the title portion of a filename), replacing
/// characters that are illegal or problematic on common filesystems (Windows in particular).
fn sanitize(component: &str) -> String {
    let cleaned: String = component
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    // Trim trailing dots/spaces (illegal as a trailing char for Windows names) and cap length.
    let trimmed = cleaned.trim().trim_end_matches('.').trim();
    let capped: String = trimmed.chars().take(180).collect();
    if capped.is_empty() {
        "_".to_string()
    } else {
        capped
    }
}

/// Sanitize a full filename that already contains an extension, keeping the `.ogg` intact.
fn sanitize_filename(filename: &str) -> String {
    match filename.strip_suffix(".ogg") {
        Some(stem) => format!("{}.ogg", sanitize(stem)),
        None => format!("{}.ogg", sanitize(filename)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> TagRequest {
        TagRequest {
            track_id: "abc123".into(),
            title: "My Song".into(),
            artists: vec!["Artist A".into(), "Artist B".into()],
            album: Some("Great Album".into()),
            album_artists: vec!["Album Artist".into()],
            track_number: 3,
            disc_number: 1,
            date: Some("2019".into()),
            genres: vec!["Rock".into()],
            isrc: Some("USABC1234567".into()),
            cover_url: None,
            output_root: PathBuf::from("/music"),
        }
    }

    #[test]
    fn destination_layout() {
        let dest = organized_destination(&req());
        let s = dest.to_string_lossy().replace('\\', "/");
        assert!(
            s.ends_with("/music/Album Artist/Great Album/1.03 - My Song.ogg"),
            "got {s}"
        );
    }

    #[test]
    fn sanitize_strips_illegal_chars() {
        assert_eq!(sanitize("AC/DC: Live?"), "AC_DC_ Live_");
    }

    #[test]
    fn sanitize_filename_keeps_extension() {
        assert_eq!(sanitize_filename("1.03 - A/B.ogg"), "1.03 - A_B.ogg");
    }

    #[test]
    fn destination_falls_back_to_track_artist_when_no_album_artist() {
        let mut r = req();
        r.album_artists.clear();
        let dest = organized_destination(&r);
        let s = dest.to_string_lossy().replace('\\', "/");
        assert!(s.contains("/music/Artist A/"), "got {s}");
    }
}
