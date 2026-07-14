# Product Context

This file provides a high-level overview of the project and the expected product that will be created. Initially it is based upon projectBrief.md (if provided) and all other available project-related information in the working directory. This file is intended to be updated as the project evolves, and should be used to inform all other modes of the project's goals and context.
2026-07-10 12:50:00 - Log of updates made will be appended as footnotes to the end of this file.

*

## Project Goal

*   Build a custom fork of ncspot (a terminal Spotify client written in Rust) that intercepts the raw, decrypted Ogg Vorbis stream of played tracks and saves it losslessly to disk, exactly as delivered, while still allowing normal playback through speakers.
*   The core technique: fork `librespot` (the playback backend crate) and inject a transparent "Tee" mechanism right before the `SymphoniaDecoder` consumes the decrypted `AudioFile` buffer.

## Key Features

*   `TeeAudioFile` wrapper struct in `librespot-playback` that wraps `AudioFile`, implements `std::io::Read` and `std::io::Seek` by delegating to the underlying `AudioFile`.
*   Transparent capture: bytes read by the decoder are asynchronously appended to a local file (e.g. `capture_[track_id].ogg`) via a buffered writer.
*   Graceful `Seek` handling (zero-padding or warn on mid-song seeks; uninterrupted playback is the primary use-case).
*   Cargo `[patch.crates-io]` mechanism to force ncspot to compile against the local modified librespot fork.

## Overall Architecture

*   ncspot (this workspace) depends on `librespot-playback` v0.8.0 for playback.
*   Existing capture attempt: [`src/capture_sink.rs`](src/capture_sink.rs) taps decoded PCM samples at the Sink level and writes WAV — this is lossy re-encoding vs the goal of raw Ogg Vorbis capture.
*   New approach intercepts EARLIER in the pipeline (post-decrypt, pre-decode) inside a forked librespot: `AudioFile::open()` -> `TeeAudioFile` -> `SymphoniaDecoder::new()`.
*   Injection point: `librespot-playback/src/player.rs`.
*   Constraint: Do NOT modify ncspot's own code for the tee; all interception lives in the librespot-playback crate fork. Do NOT parse/decode Ogg packets; blindly tee raw decrypted bytes.

---
Footnotes:
2026-07-10 12:50:00 - Initial Memory Bank creation from ncspot workspace context and user task brief.
