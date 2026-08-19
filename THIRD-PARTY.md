# Third-party libraries

Direct dependencies of this app. The complete transitive list (with exact versions) is pinned in [`Cargo.lock`](Cargo.lock).

## Rust crates

| Library | Version | License | Used for |
|---|---|---|---|
| [hayro](https://github.com/LaurenzV/hayro) | 0.4.0 (pinned) | Apache-2.0 | Pure-Rust PDF parsing and rasterization (embedded standard-14 fonts and CMaps included) |
| [log](https://crates.io/crates/log) | 0.4 | MIT OR Apache-2.0 | Logging facade |
| [jpeg-decoder](https://github.com/image-rs/jpeg-decoder) | 0.3 (`platform_independent`, no `rayon`) | MIT OR Apache-2.0 | Pulled in by the vendored `hayro-syntax` patch below; its IDCT downscaling decodes large embedded JPEGs at reduced resolution instead of full size |
| [flate2](https://github.com/rust-lang/flate2-rs) | 1 (dev-dependency only) | MIT OR Apache-2.0 | Encodes synthetic FlateDecode (zlib) image streams in `tests/interpret_downsample.rs`; not part of the shipped app |
| [kurbo](https://github.com/linebender/kurbo) | 0.13 (dev-dependency only) | MIT OR Apache-2.0 | Geometry types (`Affine`/`BezPath`/`Rect`) needed to implement a minimal `hayro_interpret::Device` in `tests/interpret_downsample.rs`, pinned to match the vendored `hayro-interpret`'s own dependency; not part of the shipped app |

## Vendored code

| Component | Version | License | Why vendored |
|---|---|---|---|
| [hayro-syntax](https://github.com/LaurenzV/hayro) | 0.7.2 | Apache-2.0 OR MIT | `vendor/hayro-syntax`, pulled in via `[patch.crates-io]`. Patches the `DCTDecode` filter (`src/filter/dct.rs`) to honor the renderer's `ImageDecodeParams::target_dimension` hint with a scaled `jpeg-decoder` decode, so a large embedded JPEG that will only ever be drawn small doesn't pay full-resolution decode time/memory. Falls through to the original zune-jpeg path on any error or unmet precondition. Not yet upstreamed. |
| [hayro-interpret](https://github.com/LaurenzV/hayro) | 0.7.0 | Apache-2.0 OR MIT | `vendor/hayro-interpret`, pulled in via `[patch.crates-io]` (its own `hayro-syntax` dependency resolves to the vendored copy above through the same patch table). The hayro-syntax DCTDecode patch above only speeds up JPEG *decode* -- every other filter (Flate, CCITT, JBIG2, ...) still decoded at native resolution, and hayro's per-render cache retains every decoded image for the page, so an image-heavy page's peak memory was the SUM of native-size decodes. Patches `decode_raster`/`ImageXObject::decoded_mask` (`src/x_object.rs`) to downsample the *final* decoded `ImageData`/`LumaData` (post color-space conversion, so it works uniformly regardless of filter) toward `target_dimension` with an integer box filter, once it's more than ~2x that hint. No-op (bit-exact with upstream) when `target_dimension` is `None`. Not yet upstreamed. |

## Foundation SDK / KeyOS platform

Provided by the installed Foundation SDK (path dependencies, not crates.io):

| Component | Role |
|---|---|
| `server` (KeyOS) | App runtime, KeyOS service messaging, filesystem API |
| `xous-api-log` | Log output to the KeyOS log server |
| `slint-keyos-platform` (+ `-build`) | [Slint](https://slint.dev) UI runtime and build integration for KeyOS |
| `foundation-themes` | Design tokens and light/dark theming |

The Slint UI toolkit itself is licensed under GPL-3.0-only OR the Slint
Royalty-free / commercial licenses. **This app elects the GPL**, which is why
it is GPL-3.0-or-later. That is not a free choice: section 3 of the Slint
Royalty-free license excludes embedded systems, and a Passport Prime is one, so
on-device the GPL is the only option that costs nothing. KeyOS's own API crates
(`server`, `fs`, `crypto`, `security`, ...) are GPL-3.0-or-later as well. Taking
this app closed-source would require a paid Slint license *and* a resolution of
the KeyOS side.
