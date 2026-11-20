# Immich API subset used by the mobile app (v3.4.2)

Reference: fork `abreheret/immich`, branch `photopi`, tag `v3.4.2`
(`mobile/` = Flutter app, `open-api/immich-openapi-specs.json` = JSON schemas,
`server/src/services/sync.service.ts` = reference sync implementation).

How this list was built: every `operationId` of the OpenAPI spec (287) was
searched as a method call in `mobile/lib` (74 hits), plus the raw HTTP calls
the app makes outside the generated client (upload, sync stream, image URLs,
`.well-known`, socket.io).

All paths below are relative to `<server>/api`.

## Key findings

1. **The timeline is local.** The app keeps its own SQLite (drift) database,
   filled by the **sync stream**. No timeline/bucket endpoint is needed: if
   `POST /sync/stream` returns the user's assets, the timeline shows them.
2. **Deduplication is done by the app.** The phone computes the SHA1 of each
   local file and compares it with the `checksum` (base64 SHA1) of the remote
   assets received through sync. `bulk-upload-check` is **not** used. The
   server should still compute the SHA1 during upload and answer `duplicate`
   when the file already exists.
3. **Authentication is a cookie.** The native Android HTTP client (OkHttp,
   `HttpClientManager.kt`) sends `Cookie: immich_access_token=<token>` on every
   request, including background uploads. The server must accept the cookie,
   the `x-immich-user-token` header and `Authorization: Bearer`.
4. **Advertise version 3.4.2.** The app picks sync types from the server
   version (`server_capability.model.dart`) and rejects a server whose major
   version is too far from its own (`version_compatibility.dart`). Reporting
   exactly `3.4.2` makes the app request the types described below.
5. **The websocket is optional.** `/api/socket.io` only gives live refreshes
   (for example, a new asset appearing right after an upload). Without it, the
   app syncs on start, on resume and on pull-to-refresh. Not in the MVP: answer
   404.
6. **No app fork change is strictly required** for login, backup, timeline
   and viewer. The fork is still useful for a distinct `applicationId` and to
   hide screens for features the server does not have.

## Login and startup flow

| Step | Call | MVP answer |
|---|---|---|
| URL entered | `GET <base>/.well-known/immich` (no `/api`) | 404 (the app then assumes `<base>/api`) |
| Reachability | `GET /server/ping` | `{"res":"pong"}` |
| Server info | `GET /server/version` | `{"major":3,"minor":4,"patch":2,"prerelease":null,"featureLevel":0}` (the app only uses `featureLevel` to enable passkeys; 0 = off) |
| | `GET /server/features` | all `false` except `passwordLogin: true`, `trash: true` |
| | `GET /server/config` | `isInitialized: true`, `isOnboarded: true`, `trashDays: 30`, empty strings elsewhere |
| | `GET /server/storage` | disk usage of the photo directory (`statvfs`) |
| Login | `POST /auth/login` `{email,password}` | `LoginResponseDto` + `Set-Cookie: immich_access_token=…` |
| Token check (each start) | `POST /auth/validateToken` | `{"authStatus":true}` or 401 |
| Current user | `GET /users/me` + `GET /users/me/preferences` | `UserAdminResponseDto`; preferences = defaults from `server/src/utils/preferences.ts` |
| Lock screen | `GET /auth/status` | `{"password":true,"pinCode":false,"isElevated":false}` |
| Logout | `POST /auth/logout` | delete the session/token, `{"successful":true,"redirectUri":"/auth/login"}` |

`login` creates a **session** (id + token). Sync checkpoints are stored per
session.

## Backup (upload)

`POST /assets`, `multipart/form-data`:

| Field | Content |
|---|---|
| `assetData` | the file (stream it to disk; compute SHA1 while writing) |
| `filename` | original file name |
| `deviceAssetId`, `deviceId` | phone identifiers (stored, unused) |
| `fileCreatedAt`, `fileModifiedAt` | ISO 8601 UTC |
| `isFavorite` | `"true"`/`"false"` |
| `duration` | `"0"` (the app does not send the real one) |
| `livePhotoVideoId`, `metadata` | iOS only, ignore |

Answer: `201 {"id":"<uuid>","status":"created"}`, or
`200 {"id":"<existing uuid>","status":"duplicate"}` when the SHA1 already
exists for this user. `413` if the file is too large. Uploads also run from
background workers (`background_upload.service.dart`, `background_downloader`
package) with the same request.

## Sync stream (the core of the app)

`POST /sync/stream`, body `{"types":[...], "reset"?:bool}`, answer
`Content-Type: application/jsonlines+json`, one JSON object per line:

```json
{"type":"AssetV2","data":{...},"ack":"AssetV2|<updateId>"}
```

The app sends the `ack` strings back through `POST /sync/ack {"acks":[...]}`
once it has stored a batch. The server stores **one checkpoint per
(session, type)** and next time only sends rows with `updateId > checkpoint`.
`updateId` is opaque to the app (Immich uses UUIDv7): a zero-padded
monotonic counter is enough, as long as string order = time order.
`DELETE /sync/ack {"types":[...]}` deletes checkpoints (forces a re-send).

Types the v3.4.2 app requests when the server reports 3.4.2:

| Requested type | What PhotoPI sends |
|---|---|
| `AuthUsersV2` | one `AuthUserV2` (the logged-in user) |
| `UsersV1` | one `UserV1` (same user) |
| `AssetsV2` | `AssetV2` for each new/changed asset, `AssetDeleteV1` for permanently deleted ones |
| `AssetExifsV1` | `AssetExifV1` (dimensions, date, GPS, camera, from the EXIF read at upload) |
| everything else (`AssetEditsV1`, `AssetMetadataV1`, `PartnersV1`, `PartnerAssets*`, `Albums*`, `AlbumUsersV1`, `AlbumToAssetsV1`, `Memories*`, `MemoryToAssets*`, `StacksV1`, `PartnerStacksV1`, `UserMetadataV1`, `PeopleV1`, `AssetFacesV3`, `AssetOcrV1`) | nothing |

Last line, always: `{"type":"SyncCompleteV1","data":{},"ack":"SyncCompleteV1|<nowId>"}`.
If the session must restart from scratch (`reset: true`, unknown checkpoint):
send only `{"type":"SyncResetV1","data":{},"ack":"SyncResetV1|reset"}` and
end the stream. Receiving a `SyncResetV1` ack in `/sync/ack` = delete all
checkpoints of the session.

`AssetV2` (all keys required, `null` allowed where marked `?`):
`id`, `ownerId`, `originalFileName`, `checksum` (base64 SHA1), `type`
(`IMAGE`|`VIDEO`), `visibility` (`timeline`), `isFavorite`, `isEdited`
(false), `createdAt?`, `deletedAt?`, `fileCreatedAt?`, `fileModifiedAt?`,
`localDateTime?`, `duration?` (ms), `width?`, `height?`, `thumbhash?`
(base64, can be null), `libraryId?`, `livePhotoVideoId?`, `stackId?` (null).

`AuthUserV2`: `id`, `name`, `email`, `isAdmin`, `hasProfileImage` (false),
`profileChangedAt`, `deletedAt?`, `oauthId?`, `pinCode?`, `storageLabel?`,
`quotaSizeInBytes?`, `quotaUsageInBytes`, `avatarColor?`.
`UserV1`: `id`, `name`, `email`, `hasProfileImage`, `profileChangedAt`,
`deletedAt?`, `avatarColor?`.
`AssetExifV1`: `assetId` + nullable fields (`exifImageWidth`,
`exifImageHeight`, `fileSizeInByte`, `orientation`, `dateTimeOriginal`,
`modifyDate`, `timeZone`, `latitude`, `longitude`, `make`, `model`,
`lensModel`, `fNumber`, `focalLength`, `iso`, `exposureTime`, `city`,
`state`, `country`, `description`, `projectionType`, `profileDescription`,
`rating`, `fps`). Full schemas: `components.schemas` in the spec.

## Viewer (URLs built in `utils/image_url_builder.dart`)

| URL | MVP |
|---|---|
| `GET /assets/{id}/thumbnail?size=thumbnail&edited=…` | small JPEG/WebP (~250 px), cached on the SD card |
| `GET /assets/{id}/thumbnail?size=preview&edited=…` | large JPEG (~1440 px), cached |
| `GET /assets/{id}/original?edited=…` | original file, `Range` support |
| `GET /assets/{id}/video/playback` | original video, `Range` support (no transcoding) |
| `GET /assets/{id}` | `AssetResponseDto` (asset details screen) |

## Trash and deletion (after the MVP)

`DELETE /assets {"ids":[...],"force":bool}`: without `force`, set `deletedAt`
(trash) and send an `AssetV2` with `deletedAt`; with `force` or after
`trashDays`, delete the file and send `AssetDeleteV1`.
`POST /trash/restore/assets`, `POST /trash/restore`, `POST /trash/empty`.
Do this before enabling automatic backup for real: a delete on the phone must
never silently destroy the only copy.

## Everything else

The other operations found in the app (albums, shared links, partners,
people, tags, stacks, memories, search, map, activities, passkeys, OAuth,
PIN code, edits, folders, profile image, libraries) are hidden or inactive
when `/server/features` says `false`. PhotoPI answers `501` with
`{"message":"Not supported by PhotoPI"}`, or an empty list for list
endpoints if a screen breaks on 501 (to be checked on the phone).
