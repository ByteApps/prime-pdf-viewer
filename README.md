# <img src="resources/icon.svg" alt="" width="42" align="top" /> PDF Viewer

**Productivity · Documents** — read PDFs on your Passport Prime, rendered entirely on-device.

Manuals, statements, backup instructions — sometimes the document you need is a PDF, and the safest place to read it is a device that can't leak it. PDF Viewer browses your Prime's Internal, Airlock, and USB storage, opens any PDF with a tap, and pages through it rendered right on the device. No cloud, no companion app, and no write access: it can look at your files but never touch them.

<p align="center">
  <img src="screenshots/browser.png" alt="File browser" width="280">
  &nbsp;
  <img src="screenshots/page1.png" alt="Page view" width="280">
  &nbsp;
  <img src="screenshots/error-not-a-pdf.png" alt="Error handling" width="280">
</p>

## Features

- **All three storage locations** — Internal, Airlock, and USB, with folder navigation; the list shows just folders and PDFs.
- **Fit-to-width pages** — each page rendered on-device, with previous/next buttons and drag-panning for tall pages.
- **Strictly read-only** — the app's signed permission manifest contains no write grants at all; it cannot modify, create, or delete anything.
- **Graceful with bad files** — a corrupt or mislabeled PDF shows a clear error banner and the app keeps running.
- **Offline by design** — Prime has no network stack; documents never leave the device.

## Get it running

With the Foundation SDK installed, build and launch in the simulator with:

```bash
foundation sim
```

See **[DEVELOPMENT.md](DEVELOPMENT.md)** for environment setup, hardware builds, rendering internals, and testing.

## Learn more

- [DEVELOPMENT.md](DEVELOPMENT.md) — building, running, rendering pipeline, permissions, and testing
- [THIRD-PARTY.md](THIRD-PARTY.md) — libraries this app is built on
- [NOTES.md](NOTES.md) — verified build/sim output and simulator gotchas

## License & disclaimer

Licensed under the GNU General Public License v3.0 or later — see [COPYING](COPYING).

This software is provided "as is", without warranty of any kind, express or
implied. Use it at your own risk — to the maximum extent permitted by law, the
authors, copyright holders, and contributors are not liable for any claim,
damages, or other liability, including loss of data, arising from this
software or its use.
