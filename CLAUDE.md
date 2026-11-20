# PhotoPI: notes for contributors and coding agents

Rust server (axum 0.8, tokio, rusqlite bundled) that implements the subset of the Immich API used by the
Immich mobile app v3.4.2. Reference for every JSON shape: `open-api/immich-openapi-specs.json` in the
Immich repository at tag v3.4.2; the sync protocol: `server/src/services/sync.service.ts`.

## Layout

- `src/main.rs`: config (env), router, `user` sub-commands, startup checks.
- `src/auth.rs`: login, sessions, the `Auth` extractor (cookie `immich_access_token`, header
  `x-immich-user-token`, Bearer).
- `src/assets.rs`: upload (streamed, SHA-1, duplicate check), read, update, trash, delete, trash purge.
- `src/sync.rs`: `POST /sync/stream` (JSON lines) and acks.
- `src/media.rs`: EXIF / ffprobe metadata, dates. `src/thumbs.rs`: thumbnails, thumbhash, worker.
- `src/dto.rs`: rows -> Immich JSON. `src/db.rs`: schema, global `update_id` counter.
- `src/tests.rs`: end-to-end test through the router. `tests/fixtures/`: tiny photo/video.
- `deploy/`: `deploy.sh` (cross-build + install), systemd unit.

## Rules

- Every change the app must see bumps `update_id` inside a `db::write_tx` (BEGIN IMMEDIATE). The sync
  stream relies on commit order = counter order.
- Never create `PHOTO_ROOT` and never write outside it: an unmounted disk must stop the server, not
  fill the SD card. The database and caches live on the SD card, never on the (NTFS) photo disk.
- Deleting: database rows first, then files.
- Keep the API answers exactly in the shape of the OpenAPI spec (required keys present, `null` where
  allowed); the app's generated client is strict.
- Tests and clippy must stay clean: `cargo test`, `cargo clippy --all-targets`.
- Build for the Pi: `cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.36`.
- Docs: English (`*.md`) + French (`*.fr.md`) for the README. Keep docs/DEVLOG.md up to date.
