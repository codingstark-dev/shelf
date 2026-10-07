# Shelf icons

| File | Purpose |
|---|---|
| `app-icon.png` | 1024x1024 master app icon (824 px squircle, 100 px transparent margin) |
| `app-icon-256.png` | 256 px copy, embedded in the binary for the setup-screen hero (`ui::hero`) |
| `AppIcon.icns` | Built from `app-icon.png` (16-512 @1x/@2x) |
| `build.sh` | Regenerates `AppIcon.icns` and `app-icon-256.png` |

The menu-bar icon is an SF Symbol (`ui::tray_symbol`), not a file.

```sh
cd app/assets/icons && ./build.sh
```
