# Progress

## [2026-07-10 17:41:54] - Release build produced

`cargo build --release` (with `CMAKE_GENERATOR="Visual Studio 17 2022"` and
`AWS_LC_SYS_CFLAGS="/std:c11"` set) succeeded, producing `target\release\ncspot.exe` (17.3 MB),
including the fixed `TeeAudioFile` (random-access seek+write capture) and with the old
ncspot-side WAV `CaptureSink` fully removed.

This file tracks the project's progress using a task list format.
2026-07-10 12:50:00 - Log of updates made.

*

## Completed Tasks

*   2026-07-10 12:50:00 - Memory Bank initialized.
*   Prior work: `src/capture_sink.rs` exists (PCM->WAV tap at Sink level; lossy, superseded by the raw-Ogg tee approach).

## Current Tasks

*   Plan and implement `TeeAudioFile` wrapper in the librespot-playback fork.
*   Integrate wrapper into `librespot-playback/src/player.rs` audio loading pipeline.
*   Document `[patch.crates-io]` instructions for ncspot's `Cargo.toml`.

## Next Steps

*   Confirm librespot fork location and version.
*   Verify `AudioFile::open` and `SymphoniaDecoder::new` signatures in the 0.8.0 source.
*   Write the Rust code for `TeeAudioFile` (Read + Seek delegation + buffered async capture).
*   Provide integration diff for `player.rs`.
*   Provide Cargo patch instructions.
