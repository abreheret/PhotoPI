# Development log

## 2026-11-20: first phone test, `/.well-known/immich`

The app (v3.4.2) showed "server version is not compatible" and the login failed. The new access log
showed why: after `GET /api/server/ping` (200) the app called `/server/version`, `/auth/login`...
without `/api` (404). `ApiService.resolveEndpoint` adds `/api` only to its ping probe; the real prefix
comes from `GET /.well-known/immich` = `{"api":{"endpoint":"/api"}}`, which PhotoPI did not serve.
Fixed, plus an access log (method, path without query, status, duration). docs/API-SUBSET.md was wrong
on this point and is corrected.

## 2026-11-20: account creation fixed

`user add` used to be fed by `read -rsp ... P; echo "$P" |` in bash. bash `read` strips spaces at the
start and end of the line, so a password with such spaces arrived shorter than typed and was refused as
"too short". `photopi user add` now asks for the password itself on a terminal (hidden, asked twice,
spaces kept) and the error says how many characters were received. Piping one line still works for
scripts. Tested on the Pi through ssh -t with a throwaway database.

## 2026-11-20: v0.1.0, first server

Decision: the official Immich server (Postgres + Redis + ML) is too heavy for a Raspberry Pi 4 (4 GB)
that already runs Plex, qBittorrent and a music server. PhotoPI implements only what the Immich mobile
app needs, in one Rust binary with SQLite.

API study of the app v3.4.2 (fork, branch `photopi`): see [API-SUBSET.md](API-SUBSET.md). Main points:
the timeline is the app's local database filled by `POST /sync/stream`; deduplication is done by the
app (SHA-1); the token travels as the `immich_access_token` cookie; reporting server version 3.4.2
selects the sync types; socket.io is optional.

Implemented: login/logout/sessions, server info, current user and preferences, streamed upload with
SHA-1 and duplicate answer, EXIF (photos) and ffprobe (videos) metadata, date logic (EXIF
DateTimeOriginal + OffsetTimeOriginal, else video creation_time, else the app's fileCreatedAt), files
stored as `<user>/<YYYY>/<MM>/<name>`, sync stream (AuthUserV2, UserV1, AssetV2, AssetDeleteV1,
AssetExifV1, SyncCompleteV1, reset) and acks, thumbnails (preview 1440 px, thumbnail 250 px, JPEG) with
a background worker and thumbhash, original and video with Range, favorites/archive/description, trash
with restore and a purge after 30 days, permanent delete, `user add|passwd|list`.

Tests: 22 (unit tests + an end-to-end test of login, upload, duplicate, sync, acks, thumbnails, Range,
favorite, trash, restore, delete, reset, logout). Clippy clean.

Self-test on the real Pi (throwaway instance on port 2284, test account, data on the NTFS disk, then
removed):
- 4000x3000 JPEG with EXIF orientation 6: stored as 3000x4000, date 14:03 local / 12:03 UTC, time
  zone `UTC+2`, GPS, make/model read; same file again = `duplicate`.
- 3 s 720p MP4: duration 3000 ms, 30 fps, date converted to Paris time.
- Thumbnails OK (photo rotated), original identical, Range 206.
- 12 Mpx thumbnail made on demand: 1.8 s on the Pi, 17 ms from the cache.
- Memory: 30 MB RSS.
- Bugs found and fixed: the video preview was enlarged by ffmpeg (720p -> 1440p); EXIF values stored
  as floats (written by some editors) were ignored. Both now covered by tests with tiny fixtures.

Installed as the `photopi` systemd service (port 2283, `RequiresMountsFor=/mnt/music/photo`, Nice 10,
MemoryMax 768M).

Next:
- Create the real account, connect the app on the phone, back up a test album, check timeline, viewer,
  favorites, trash, re-backup without duplicates.
- Then the app fork: distinct applicationId, hide screens that answer 501 if they break.
- Later: websocket (live refresh after upload), albums.
