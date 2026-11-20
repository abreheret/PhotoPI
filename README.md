# PhotoPI

*[Version française](README.fr.md)*

A small photo and video backup server for a Raspberry Pi, compatible with the
**Immich mobile app**.

The official [Immich](https://immich.app) server needs Postgres, Redis and a
machine-learning service (6 GB of RAM or more). That is too much for a
Raspberry Pi 4 that already runs other things. PhotoPI is a single Rust binary
with SQLite. It implements only the part of the Immich API that the mobile app
needs to back up and browse your photos, and uses about **30 MB of RAM**.

PhotoPI is not affiliated with the Immich project.

## What works

- Login with email and password (several users possible).
- **Backup** from the app, in the foreground and the background. Files that
  are already on the server are skipped: the app compares SHA-1 checksums.
- **Timeline** in the app, through the Immich sync stream.
- **Thumbnails** and previews for photos and videos (videos through ffmpeg).
  They are prepared in the background right after each upload.
- **Full-screen viewer**, original download, video playback (no transcoding;
  HTTP Range supported).
- Favorites, archive, description.
- **Trash**: a photo deleted from the app goes to the trash for 30 days and can
  be restored. After that, or with "delete permanently", the file is removed
  from the disk.
- Photo metadata: date and time zone, GPS, camera, lens, exposure (EXIF).
  Video metadata: duration, size, frame rate, date, GPS (ffprobe).

Not supported: albums, sharing, partners, faces, smart search, map,
memories, tags, stacks, OAuth, photo editing. The app hides most of these
screens because the server reports them as disabled. The others answer
`501 Not supported by PhotoPI`.

Tested with the app **v3.4.2** (Android). PhotoPI reports itself as server
3.4.2, so that the app uses the matching protocol. A different version of the
app may expect a different API: see [docs/API-SUBSET.md](docs/API-SUBSET.md).

## How files are stored

```
PHOTO_ROOT/<user>/<YYYY>/<MM>/<original file name>
```

The date is when the photo was taken (EXIF), in local time. `<user>` is the
part of the email before `@`. The same file name in the same month gets a
`+1`, `+2`... suffix. Files are never renamed or converted: the folder is a
normal photo library that you can copy, browse or back up with any tool.

The database (SQLite) and the thumbnail cache stay on the SD card
(`~/.local/share/photopi`, `~/.cache/photopi`). Only the photos go to the disk.

## Install on the Raspberry Pi

Requirements on the Pi: 64-bit OS (aarch64), and `sudo apt install ffmpeg`
for video thumbnails and video metadata.

From your computer (needs Rust, `zig` and `cargo-zigbuild` to cross-compile):

```sh
PI=pi@raspberrypi.local ./deploy/deploy.sh
```

The script runs the tests, builds the binary, copies it to `~/photopi/` on the
Pi, and installs and starts the `photopi` systemd service. Check the unit file
first: `deploy/photopi.service`. Set `PHOTO_ROOT` to your photo folder; it must
exist. The service starts only when that folder's disk is mounted, so photos
never go to the SD card by mistake.

Then create your account on the Pi. The password is asked twice and not shown:

```sh
ssh -t pi@raspberrypi.local '~/photopi/photopi user add you@example.com "Your Name"'
```

In a script, pipe the password instead (one line on stdin; spaces are kept).

In the Immich app, enter the server URL `http://raspberrypi.local:2283` (or the
Pi's IP address), then your email and password.

### Settings (environment variables)

| Variable | Default | |
|---|---|---|
| `PHOTO_ROOT` | `/mnt/music/photo` | photo folder (must exist) |
| `PHOTO_DB` | `~/.local/share/photopi/photopi.db` | SQLite database |
| `PHOTO_CACHE` | `~/.cache/photopi` | thumbnails |
| `PHOTO_BIND` | `0.0.0.0:2283` | listen address (2283 = Immich's port) |
| `PHOTO_TRASH_DAYS` | `30` | days before the trash is emptied |
| `RUST_LOG` | `info` | log level |

### Users

```sh
~/photopi/photopi user add <email> <name>   # first user = admin
~/photopi/photopi user passwd <email>       # also logs out every device
~/photopi/photopi user list
```

Run these with the same `PHOTO_DB` as the service. The default value is
correct for the `pi` user.

## Operations

- Logs: `journalctl -u photopi -f`
- Status and memory: `systemctl status photopi`
- Back up the database (`photopi.db`) as well as the photos: it holds the
  accounts, favorites, trash and descriptions. The photos themselves are plain
  files.

## Security

PhotoPI speaks plain HTTP and is meant for your home network. Do not expose
port 2283 to the internet. For remote access, use a VPN (WireGuard,
Tailscale...) or an HTTPS reverse proxy. Passwords are stored as Argon2
hashes, and access tokens as SHA-256 hashes.

## Development

```sh
cargo test      # unit tests + an end-to-end test of the app's flows
cargo clippy --all-targets
```

- [docs/API-SUBSET.md](docs/API-SUBSET.md): the Immich API calls the app makes
  and how PhotoPI answers them.
- [docs/DEVLOG.md](docs/DEVLOG.md): development journal.

## License

MIT, see [LICENSE](LICENSE). Immich belongs to its authors; the Immich app is
AGPL-3.0 and is not part of this repository.
