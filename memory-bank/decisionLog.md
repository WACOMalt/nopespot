# Decision Log

## [2026-07-11 01:27:05] - Fix: multi-track capture/tagging races + cache cleanup

**Reported bug:** First song tagged fine; subsequent songs had `capture_<id>.ogg.partial` renamed
too early (before the song finished) and then weren't tagged. On-disk evidence: one stuck
`.ogg.partial`, one finalized-but-untagged `.ogg`, and empty (0-byte) files in librespot's audio
cache.

**Root causes:**
1. **Tee finalized on `Drop`** - but the decoder (and thus `TeeAudioFile`) is held alive for the
   entire duration of playback, and even longer for *preloaded* tracks still buffering. So the
   drop-based `.partial`->`.ogg` rename fired far too late, or never (for a preloaded/held track).
2. **ncspot triggered tagging on `FinishedTrack`** using `queue.get_current()`, which raced the
   capture writer and depended on fragile queue timing.

**Fixes:**
1. **Eager, gap-safe finalization (fork `tee.rs`):** The tee now receives the stream's total length
   (`subfile_len`, plumbed from `player.rs`) and finalizes (`.partial`->`.ogg`) as soon as the whole
   stream has been captured *contiguously*. Contiguity is tracked with a "frontier" + buffered
   ahead-ranges (`advance_frontier`), immune to Symphonia's header-indexing pass (which reads header
   fragments near EOF while seeking past - never writing - the payloads; a naive high-water-mark
   would finalize a silent file prematurely). Fires within seconds of track load, during buffering,
   independent of decoder/tee lifetime. `Drop` remains a best-effort fallback. 3 new unit tests.
2. **Trigger tagging at track-load, not playback-end (ncspot):** Moved `spawn_tag_and_organize` from
   `application.rs`'s `FinishedTrack` handler to `queue.rs`'s `play()` (fires once per track load,
   with `Track` + `WebApi` in hand). Tagging thread poll bumped to 300s. No longer races the writer;
   reliably fires for every played track including skips.
3. **Purge incomplete librespot audio cache (`spotify.rs`):** On session startup,
   `clean_incomplete_audio_cache` removes zero-length files from `<cache>/librespot/files` (stubs from
   interrupted downloads that would otherwise be served as `AudioFile::Cached` and fail to decode).

**Verification:** librespot `cargo test tee` 3/3 pass; ncspot debug + release builds clean.
Awaiting user multi-track playback re-test.

## [2026-07-11 00:17:22] - Ogg Vorbis tagging + library organization (feature branch)

**Feature:** After each track's raw Ogg is captured, tag it with Spotify metadata + album art and
move it into `<root>/<AlbumArtist>/<Album>/<Disc#>.<Track##> - <Title>.ogg`.

**Branches:** ncspot work on `feature/ogg-vorbis-tagging`; librespot fork work continues on
`ncspot-tee-fork`.

**Key decisions:**
*   **Completion signal (librespot fork):** `TeeAudioFile` now writes to `<path>.partial` and, once
    the writer thread finishes (all reads done / `Finish`/`Drop`) and `sync_all` succeeds, atomically
    `rename`s it to the final `<path>`. So the appearance of the final suffix-less `capture_<id>.ogg`
    is a race-free "capture complete" signal for ncspot. On write failure the `.partial` is left in
    place and NOT published, so consumers never see a corrupt "complete" file.
*   **Tagging library:** `lofty` 0.24 (already added). Ogg -> `TagType::VorbisComments`. Standard
    fields mapped: TITLE, ARTIST (multi, one Vorbis comment each), ALBUM, ALBUMARTIST (multi),
    TRACKNUMBER, DISCNUMBER, DATE (RecordingDate), GENRE (multi), ISRC. Album art fetched from the
    track's `cover_url` (Spotify returns images largest-first, so `images.first()` = largest) and
    embedded as a `CoverFront` `Picture` / `METADATA_BLOCK_PICTURE`.
*   **Threading:** All tagging runs on a dedicated detached thread spawned per finished track
    (`spawn_tag_and_organize`), per explicit user requirement that tagging never interfere with live
    playback or the Ogg capture. The thread polls (bounded 30s) for the finalized capture file, then
    tags + moves it.
*   **Metadata enrichment:** `Track` doesn't carry release date / genres / ISRC, so the tagging
    thread optionally enriches via a cloned `WebApi` (one-shot `track()` + `album()` lookups). `WebApi`
    is `Clone + Send` (Arc/RwLock internals), so this is cheap and fully off the audio path.
*   **Hook point:** `application.rs` `PlayerEvent::FinishedTrack` handler, reading `queue.get_current()`
    (still the just-finished track) BEFORE `queue.next()` advances.
*   **Filesystem safety:** path components sanitized (illegal Windows chars -> `_`, trailing dots/spaces
    trimmed, length-capped); destination de-duplicated with ` (n)` suffix so re-captures don't clobber.
    Move uses `rename` with copy+remove fallback for cross-device.
*   **Output root:** current working directory (same place librespot writes `capture_*.ogg`).

**Verification:** debug + release builds succeed; 4 `tagging::tests` unit tests pass (destination
layout, sanitization, extension preservation, album-artist fallback).

## [2026-07-10 17:24:35] - Fixed real root cause of silent-but-valid capture files (2nd bug)

**Symptom:** After fixing the bounded-channel bug, the user's real capture
(`capture_0qU806xTLhuZ5kCrex2x4r.ogg`, 9.7MB) looked correct at a glance - `ffprobe` reported a
fully valid Ogg Vorbis stream, `probe_score=100`, correct 234.67s duration matching the track - but
`ffmpeg -af volumedetect` showed `mean_volume: -91dB, max_volume: -72dB`, i.e. essentially silent.
A raw byte scan revealed a "checkerboard": ~986 tiny non-zero regions (tens to ~200 bytes, matching
Ogg page *header* sizes) separated by large zero-filled gaps (matching page *payload* sizes).

**Root cause:** Symphonia's Ogg demuxer, when given a source that reports `is_seekable() == true`
(as ours does, via delegating `MediaSource`), performs an initial *indexing pass*: it reads each
page's small header only, then seeks forward past that page's audio payload straight to the next
header, without ever reading the payload bytes during this pass. It then seeks back to the start
and does the real sequential *decode pass*, which does read the full payloads this time.

The previous "high-water-mark" tee design (append-only, comparing each write's `pos` against the
highest offset written so far to detect forward gaps vs. backward re-reads) handled this exactly
wrong: during the indexing pass it zero-padded the skipped payload regions (correct at the time),
but then during the real decode pass, it saw `pos < written` (since the file already had the
indexing pass's zero-padding written up to a high offset) and treated the real audio bytes as a
"backward seek re-reading duplicate data" - silently discarding them and leaving the zero-padding
in place. Net result: a structurally perfect, correctly-timed Ogg Vorbis file containing almost
entirely silence.

**Fix:** Discarded the append-only/high-water-mark/pad-or-dedup model entirely. `TeeAudioFile` now
tags every chunk with the exact absolute byte offset (`pos`) it was read from in the source stream,
and the background writer thread performs true random-access `Seek::seek` + `Write::write_all`
directly on the output `File` at that offset - unconditionally overwriting whatever was there
before. This correctly models what's actually happening: the indexing pass's header-only reads and
placeholder gaps get correctly overwritten by the decode pass's full-payload reads at the same
offsets. Any byte range genuinely never read (e.g., user stops partway through a track) is simply
left as zero-filled space at the end of the file - the one remaining, expected imperfection, and
consistent with the original design brief's allowance for skips being acceptable.

**Verification:** `cargo check -p librespot-playback` and a full `cargo build` of ncspot both
succeed after the fix. Awaiting a fresh user playback test to confirm the new capture contains
audible, correctly-decoded audio throughout.

## [2026-07-10 13:51:37] - Fixed critical silent-capture bug in TeeAudioFile

**Issue:** First real-world test produced a `capture_<id>.ogg` that was instantly 8,518 KB and
never grew during playback - i.e. it contained no actual audio (silence/zero-padding only).

**Root cause:** The original `tee()` implementation used a *bounded* `std::sync::mpsc::sync_channel`
(depth 256) with `try_send()`, dropping the chunk when the channel was full ("never block the audio
thread" was over-applied). Critically, `self.pos` was still advanced by `data.len()` even when the
send failed and the chunk was dropped. Because `AudioFile`/`Subfile` reads run far faster than
real-time during buffering/prefetch, the channel filled almost immediately, so the vast majority of
real audio chunks were silently dropped - while `pos` kept advancing as if they'd been written. The
writer thread then saw large forward "gaps" (pos > written) on every message and zero-padded them,
producing a correctly file-sized but essentially silent capture.

**Fix:** Replaced the bounded `sync_channel`/`try_send` with an unbounded `std::sync::mpsc::channel`
and a plain (always-succeeds-unless-thread-dead) `send()`. This is still effectively "free" for the
caller (a cheap in-memory queue push, no disk I/O on the calling thread) and guarantees every byte
read is eventually written to the capture file - correctness prioritized over the largely theoretical
risk of unbounded memory growth (bounded in practice by a single track's remaining audio data).
`pos` is now only ever advanced in lock-step with data that was actually (successfully) queued for
writing.

**Verification:** `cargo check -p librespot-playback` and a full `cargo build` of ncspot both succeed
after the fix.

This file records architectural and implementation decisions using a list format.
2026-07-10 12:50:00 - Log of updates made.

*

## Decision

*   [2026-07-10 12:50:00] - Intercept the audio stream inside a forked `librespot-playback` at the `AudioFile` -> `SymphoniaDecoder` boundary (via a `TeeAudioFile` Read/Seek wrapper), rather than at ncspot's Sink level.

## Rationale

*   Capturing at the Sink (existing `src/capture_sink.rs`) only yields decoded PCM, forcing lossy WAV re-encoding. Tapping post-decrypt / pre-decode yields the original raw Ogg Vorbis bytes exactly as delivered = lossless capture.
*   AES decryption and chunk assembly are already done by librespot at the `AudioFile` stage, so the tee grabs clean decrypted bytes without re-implementing crypto.
*   Keeping interception inside librespot-playback satisfies the constraint of not modifying ncspot code and makes the capture transparent to the decoder.

## Implementation Details

*   `TeeAudioFile` implements `std::io::Read` + `std::io::Seek`, delegating to inner `AudioFile`; on successful `read`, appends bytes to `capture_[track_id].ogg` through a buffered writer.
*   Disk writes must not stall the audio thread -> use `BufWriter` (and/or an async/background writer channel) to avoid buffer underruns.
*   `Seek` is handled gracefully: mid-song seeks may cause skips in the capture file; document that uninterrupted playback is the primary supported case (optionally zero-pad).
*   Do NOT parse/decode Ogg packets; tee raw bytes blindly.
*   ncspot consumes the fork via `[patch.crates-io]` entries pointing librespot-core/oauth/playback/protocol at the local fork path; fork must remain API-compatible with the pinned 0.8.0.
