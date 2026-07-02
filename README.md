# PDF Viewer (Passport Prime)

A read-only PDF viewer for Foundation's Passport Prime, built on KeyOS with a
Slint UI. Browse Internal / Airlock / USB storage, tap a PDF, and page through
it rendered on-device — no cloud, no write access.

- **Rendering**: [hayro](https://github.com/LaurenzV/hayro) 0.4 (pure Rust)
  rasterizes each page fit-to-width; displayed via Slint `Image` from a shared
  pixel buffer.
- **Permissions**: read-only file access (`fs-read` + `fs-access-read`
  templates) — the signed manifest contains no write grants.
- **Testing**: driven end-to-end in the hosted simulator by
  `../ui-automation/tests/view-pdf.sh` (CoreGraphics taps + log assertions).

| Browser | Page view | Error handling |
| --- | --- | --- |
| ![browser](screenshots/browser.png) | ![page](screenshots/page1.png) | ![error](screenshots/error-not-a-pdf.png) |

## Build & run

```bash
nix develop ~/.foundation/sdk/current --command foundation build   # signed hardware bundle
nix develop ~/.foundation/sdk/current --command foundation sim     # hosted simulator
```

See `CLAUDE.md` for architecture and the hayro version pin rationale, and
`NOTES.md` for verified build/sim output and simulator gotchas.
