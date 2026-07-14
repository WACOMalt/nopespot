# System Patterns *Optional*

This file documents recurring patterns and standards used in the project.
It is optional, but recommended to be updated as the project evolves.
2026-07-10 12:50:00 - Log of updates made.

*

## Coding Patterns

*   Rust edition 2024 (workspace), release profile uses LTO + codegen-units=1.
*   Transparent decorator/wrapper pattern for stream interception (e.g. `CaptureSink` wraps `Sink`; `TeeAudioFile` wraps `AudioFile`), delegating trait methods to an inner value while side-effecting a capture writer.
*   Use `BufWriter<File>` for capture I/O to avoid blocking the real-time audio path.
*   Timestamped capture filenames to avoid collisions on rapid track skips (see `capture_sink.rs` using `%Y%m%d_%H%M%S_%3f`).

## Architectural Patterns

*   ncspot delegates all Spotify playback/decoding to the `librespot-*` crate family (pinned 0.8.0).
*   Stream interception is layered at trait boundaries: Sink level (PCM) vs AudioFile level (raw decrypted Ogg). The AudioFile-level tee is preferred for lossless capture.
*   Dependency override via Cargo `[patch.crates-io]` to swap the registry librespot for a local fork.

## Testing Patterns

*   (TBD) Manual verification: play a full track uninterrupted, confirm resulting `capture_*.ogg` is a valid, playable Ogg Vorbis file (e.g. via ffprobe/vlc) and that playback through speakers is unaffected.
