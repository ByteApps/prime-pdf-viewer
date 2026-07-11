# <img src="resources/icon.svg" alt="" width="42" align="top" /> PDF Viewer — a Passport Prime app

A read-only PDF viewer for Foundation's **Passport Prime**, built as a Rust
binary with a **Slint** UI on **KeyOS** (Foundation's Rust microkernel on
Xous). Browse Internal / Airlock / USB storage, tap a PDF, and page through
it rendered on-device — no cloud, no write access.

<p align="center">
  <img src="screenshots/browser.png" alt="File browser" width="280">
  &nbsp;
  <img src="screenshots/page1.png" alt="Page view" width="280">
  &nbsp;
  <img src="screenshots/error-not-a-pdf.png" alt="Error handling" width="280">
</p>

## Details

- **Rendering**: [hayro](https://github.com/LaurenzV/hayro) 0.4 (pure Rust)
  rasterizes each page fit-to-width; displayed via Slint `Image` from a shared
  pixel buffer.
- **Permissions**: read-only file access (`fs-read` + `fs-access-read`
  templates) — the signed manifest contains no write grants.
- **Testing**: driven end-to-end in the hosted simulator by
  `../ui-automation/tests/view-pdf.sh` (CoreGraphics taps + log assertions).

## Build & run

Requires the `foundation` CLI (on `PATH` at `~/.foundation/sdk/bin`) and Nix.
In a non-login shell, source Nix first:

```bash
. '/nix/var/nix/profiles/default/etc/profile.d/nix-daemon.sh'
export PATH="$HOME/.foundation/sdk/bin:$PATH"
```

Then, from this directory (via the SDK's Nix dev shell):

```bash
nix develop ~/.foundation/sdk/current --command foundation sim     # hosted simulator
nix develop ~/.foundation/sdk/current --command foundation build   # compile + sign a hardware bundle
```

> **Hardware sideload** (`foundation sideload`) is **not** possible on a retail
> Prime — it needs dev firmware from Foundation. The simulator is the
> verification target. See `NOTES.md`.

See `CLAUDE.md` for architecture and the hayro version pin rationale, and
`NOTES.md` for verified build/sim output and simulator gotchas.
