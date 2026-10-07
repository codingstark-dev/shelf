# shelf-drive

Runs [Shelf](https://github.com/codingstark-dev/shelf), a drag-and-drop drive for your own
Cloudflare R2 / S3-compatible bucket. macOS only (Apple Silicon or Intel).

```sh
npx shelf-drive
```

The first run downloads Shelf.app for this version from GitHub Releases, verifies its SHA-256,
caches it in `~/Library/Caches/shelf-drive/<version>/` and opens it. Later runs open the cached app.

```
npx shelf-drive --update      # re-download
npx shelf-drive --uninstall   # delete the cache and the start-at-login item
npx shelf-drive --version
```

The app is unsigned for now. If macOS blocks it: System Settings, Privacy & Security, Open Anyway.

Shelf starts at login; turn that off in Settings, or run `npx shelf-drive --uninstall`
(which also deletes the cached app).
