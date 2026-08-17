# Third-party libraries

Direct dependencies of this app. The complete transitive list (with exact versions) is pinned in [`Cargo.lock`](Cargo.lock).

## Rust crates

| Library | Version | License | Used for |
|---|---|---|---|
| [hayro](https://github.com/LaurenzV/hayro) | 0.4.0 (pinned) | Apache-2.0 | Pure-Rust PDF parsing and rasterization (embedded standard-14 fonts and CMaps included) |
| [log](https://crates.io/crates/log) | 0.4 | MIT OR Apache-2.0 | Logging facade |
| [jpeg-decoder](https://github.com/image-rs/jpeg-decoder) | 0.3 (`platform_independent`, no `rayon`) | MIT OR Apache-2.0 | Pulled in by the vendored `hayro-syntax` patch below; its IDCT downscaling decodes large embedded JPEGs at reduced resolution instead of full size |

## Vendored code

| Component | Version | License | Why vendored |
|---|---|---|---|
| [hayro-syntax](https://github.com/LaurenzV/hayro) | 0.7.2 | Apache-2.0 OR MIT | `vendor/hayro-syntax`, pulled in via `[patch.crates-io]`. Patches the `DCTDecode` filter (`src/filter/dct.rs`) to honor the renderer's `ImageDecodeParams::target_dimension` hint with a scaled `jpeg-decoder` decode, so a large embedded JPEG that will only ever be drawn small doesn't pay full-resolution decode time/memory. Falls through to the original zune-jpeg path on any error or unmet precondition. Not yet upstreamed. |

## Foundation SDK / KeyOS platform

Provided by the installed Foundation SDK (path dependencies, not crates.io):

| Component | Role |
|---|---|
| `server` (KeyOS) | App runtime, KeyOS service messaging, filesystem API |
| `xous-api-log` | Log output to the KeyOS log server |
| `slint-keyos-platform` (+ `-build`) | [Slint](https://slint.dev) UI runtime and build integration for KeyOS |
| `foundation-themes` | Design tokens and light/dark theming |

The Slint UI toolkit itself is licensed under GPL-3.0-only OR the Slint Royalty-free / commercial licenses; this app is GPL-3.0-or-later.
