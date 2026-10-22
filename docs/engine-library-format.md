# Engine DJ library format

Findings from inspecting a real Engine DJ library on an external drive (a Denon SC LIVE 4 standalone-player library, Engine schema 3.0.2). Everything below was observed in that library, either by reading the files directly or by decoding them and cross-checking the result. Where something is inferred rather than verified, it says so.

## Verdict: readable, not encrypted, not obfuscated

| Part | Format | Readable? |
| --- | --- | --- |
| `Database2/*.db` | Plain SQLite 3 (no SQLCipher, no key) | Yes, with any SQLite client |
| Track metadata, playlists, history, smartlists | Ordinary SQL columns, UTF-8 text | Yes |
| Cues, loops, beat grid, waveform | BLOBs: a 4-byte length plus zlib, or raw binary | Yes, after decoding (see below) |
| Album art | PNG BLOBs | Yes |
| Overview `.rgb` files | The same zlib BLOB format as the waveform column | Yes |
| `user.profile` | Plain JSON | Yes |
| `.stems` files | MP4 container with AAC audio | Yes (ffprobe reads them) |

I did not find any encryption, DRM or deliberate obfuscation. All 3,285 tracks that had performance data decoded without a single failure, for every BLOB type. The only "opaque" values are 64-bit random-looking integers (`playedIndicator`, `Information.currentPlayedIndiciator`). They are change/play markers, not scrambled data.

The one real catch: BLOB layouts are undocumented by the vendor, and **endianness is mixed**. Most fields are big-endian, but the beat-grid markers and the loops BLOB are little-endian.

## Directory layout

```
<drive>/
├── Engine Library/
│   ├── Database2/
│   │   ├── m.db            main library (tracks, playlists, smartlists, performance data)
│   │   ├── hm.db           history (played sessions)
│   │   ├── sm.db           same schema, empty here (likely streaming/"smart" library)
│   │   ├── stm.db          same schema, empty here (likely streaming library)
│   │   ├── *.db-journal    SQLite rollback journals (0 bytes when closed cleanly)
│   │   └── OverviewData/<database-uuid>/<trackId>.rgb
│   ├── Artwork/            ~800 JPEGs named by a base64url hash
│   ├── Stems/<trackId> <database-uuid>.stems
│   ├── coredumps/          crash dumps from the player (not part of the library)
│   └── user.profile        player settings (JSON)
└── <music folders>/        the audio files, referenced by relative path
```

The names of `sm.db` and `stm.db` are my guess from their position and emptiness. They share the schema, and each holds its own database UUID.

All four databases share the same `Information` table and schema version (3.0.2), and each has its own UUID. `PRAGMA user_version` is `4194305` (`0x400001`) on all of them.

## Database schema (`m.db`)

### `Information`
One row: `uuid`, `schemaVersionMajor/Minor/Patch`, `currentPlayedIndiciator` (sic), `lastRekordBoxLibraryImportReadCounter`.

### `Track`
One row per track, `id` is `AUTOINCREMENT` and never reused (a trigger aborts inserts that would recycle an id). Notable columns:

| Column | Notes |
| --- | --- |
| `path` | Relative to the `Engine Library` folder, e.g. `../House/foo.mp3`. Unique. |
| `filename`, `fileType`, `fileBytes`, `bitrate`, `length` | `length` is in seconds. `fileType` seen: `flac`, `mp3`, `m4a`, `mp4`. |
| `bpm` (integer) / `bpmAnalyzed` (real) | `bpm` is rounded and can differ from the analysed value. Use `bpmAnalyzed`. |
| `key` | Integer 0–23 (musical key index). I did not verify which index is which key. |
| `rating` | 0–100, mostly in steps of 20 (stars × 20). |
| `title`, `artist`, `album`, `genre`, `comment`, `label`, `composer`, `remixer`, `year` | Plain text. |
| `albumArtId` | Foreign key to `AlbumArt.id` (`ON DELETE RESTRICT`). |
| `dateCreated`, `dateAdded`, `lastEditTime`, `timeLastPlayed` | Unix epoch seconds. |
| `isAnalyzed`, `isBeatGridLocked`, `isPlayed`, `isAvailable`, ... | Booleans as 0/1. |
| `streamingSource`, `uri`, `streamingFlags` | Streaming tracks, e.g. source `TIDAL`, uri `streaming://TIDAL/Track/<id>`. `path` then points at a cached download on the player. |
| `originDatabaseUuid`, `originTrackId` | Which database a track originally came from. Unique together. Tracks keep their origin when libraries are merged, so one database can hold several origin UUIDs. |

Triggers keep `lastEditTime` current, forbid changing or recycling `id`, fill in `originTrackId`/`originDatabaseUuid`, and insert an empty `PerformanceData` row for every new track.

### `PerformanceData`
One row per track (`trackId` is the primary key and a foreign key with cascade). The analysis results are all BLOBs, decoded in the next section: `trackData`, `overviewWaveFormData`, `beatData`, `quickCues`, `loops`. Also `thirdPartySourceId` and `activeOnLoadLoops`.

### `AlbumArt`
`id`, `hash` (40-hex SHA-1 style string, indexed), `albumArt` (a PNG BLOB; NULL for some rows). `Track.albumArtId` points here.

### Playlists
- `Playlist`: `id`, `title`, `parentListId` (0 = top level), `nextListId` (0 = last sibling), `isPersisted`, `lastEditTime`, `isExplicitlyExported`. Siblings form a **singly linked list** through `nextListId` (unique per parent), not an order column.
- `PlaylistEntity`: `id`, `listId`, `trackId`, `databaseUuid`, `nextEntityId` (0 = last track), `membershipReference`. Tracks in a playlist are likewise a linked list, in play order.
- Views `PlaylistPath`, `PlaylistAllParent` and `PlaylistAllChildren` flatten the hierarchy. Triggers keep the lists consistent on insert and delete, and propagate `isPersisted` up and down the tree.

### `Smartlist`
Keyed by `listUuid`. The rules are JSON text in `rules`, e.g. `{"match":"all","rules":[{"col":"genre","con":"LIKE","param":"'%Techno%'","v":"2.20.0"}],"rv":2}`. Parent and next are referenced by path/UUID instead of integer id.

### Other tables
- `PreparelistEntity`: the prepare list (`trackId`, `trackNumber`).
- `Pack`: `packId`, `changeLogDatabaseUuid`, `changeLogId`, `lastPackTime` (looks like cloud/export pack bookkeeping).
- `ChangeLog` is a view that always returns zero rows.

### `hm.db` (history)
Same tables as `m.db`, but the ones that matter are `Historylist` (`sessionId`, `title`, `startTime`, `timezone`, `originDriveName`, `originDatabaseUuid`, `originListId`, `isDeleted`, `editTime`) and `HistorylistEntity` (`listId`, `trackId`, `startTime`). It also carries its own copy of `Track`, `PerformanceData` and `AlbumArt` for the tracks that were played, so history stays valid even if the main library changes.

## BLOB formats

Common wrapper for `trackData`, `beatData`, `quickCues`, `overviewWaveFormData` (and the `.rgb` files): **4-byte big-endian uncompressed length, then a zlib stream** (starts `78 9c`). `zlib.decompress(blob[4:])` works, and the declared length matched the real length for every row.

`loops` is **not compressed**.

Decoded layouts below are from the library inspected. Field names are mine.

### `trackData` (44 bytes decompressed, big-endian)
| Offset | Type | Meaning |
| --- | --- | --- |
| 0 | f64 | sample rate (44100.0) |
| 8 | i64 | total sample count |
| 16 | i32 | key (matches `Track.key`) |
| 20 | 3 × f64 | values in the 0–1 range, probably loudness measures (not verified) |

### `beatData` (decompressed)
| Offset | Type | Meaning |
| --- | --- | --- |
| 0 | f64 BE | sample rate |
| 8 | f64 BE | total samples |
| 16 | u8 | beat grid is set (1) |
| 17 | i64 BE | number of default markers |
| 25 | N × 24 bytes | markers, **little-endian**: f64 sample offset, i64 beat number, i32 beats until next marker, i32 unknown |
| ... | i64 BE | number of adjusted markers, followed by the same marker layout |

Nine trailing bytes after the adjusted markers were not identified. Sanity check: BPM computed from the first and last marker (`60 × sampleRate × Δbeats / Δsamples`) matched `bpmAnalyzed` within 0.6 BPM for 3,265 of 3,284 tracks. The rest are likely variable-tempo grids or grids edited after analysis, which I did not investigate.

### `quickCues` (big-endian)
`i64 count` (8 in the tracks I decoded), then per cue: `u8 label length`, label bytes, `f64 sample position` (-1.0 means unset), `4 bytes ARGB colour`. Then a 17-byte tail: `f64`, `u8`, `f64`. These look like the main cue point (adjusted and default) with an override flag.

### `loops` (raw, **little-endian**)
`i64 count` (8), then per loop: `u8 label length`, label bytes, `f64 start`, `f64 end` (both -1.0 when unset), `u8 start-set`, `u8 end-set`, `4 bytes ARGB colour`. The parse consumed every byte of the BLOB exactly.

### `overviewWaveFormData` and `OverviewData/*.rgb` (big-endian header)
Header: `i64` (1024), `i64` (1024), `f64` samples per entry (total samples / 1024). Then 3 bytes per entry. There are 1025 entries for a declared count of 1024. The meaning of the 3 bytes per entry (likely low, mid and high band heights) was not verified. A sample `.rgb` file decompressed to exactly the same size as the database BLOB, so the `.rgb` files appear to duplicate it.

## Files outside the databases

- **`Artwork/`**: JPEGs named with a base64url hash. I did not check how they relate to `AlbumArt.hash`.
- **`OverviewData/<uuid>/<trackId>.rgb`**: one folder per origin database UUID, file named by the track id in that database.
- **`Stems/<trackId> <uuid>.stems`**: an MP4 container with a single AAC stream of **8 channels** (presumably four stereo stems). ffprobe opens it directly. Stem names/order were not checked.
- **`user.profile`**: JSON, `{"data":{"User":{"childorder":[...],"children":{...}}}}`. Each setting is an object like `{"string":"88-175","type":4}` or `{"state":true,"type":1}`.
- **`coredumps/`**: player crash dumps (`.core.zst`, `.log.zst`, `.metadata`). The metadata names the hardware and firmware that wrote the library. Not part of the library format.

## Gotchas for an implementation

- **Open read-only, or on a copy.** Writing while Engine DJ or the player has the database open risks corruption. The `*.db-journal` files are SQLite rollback journals, so a non-empty one means an interrupted write. A library on a FAT/exFAT drive has no permission protection either.
- **Paths are relative** to the `Engine Library` folder and use `/`. In this library 815 of 3,123 non-streaming tracks did not resolve to an existing file, so a viewer must tolerate missing files (`isAvailable` exists but I did not compare it).
- **Streaming tracks** have `streamingSource` set and a path into the player's own storage, not the drive.
- **Playlist and entity order is a linked list**, not a position column. Writers must also maintain `nextListId`/`nextEntityId` and `isPersisted`, which triggers partly handle.
- **Triggers and ids**: ids are never recycled. Inserting a track must go through SQL so the `PerformanceData` row and origin fields are created.
- **Mixed endianness** in BLOBs, as above.
- **Writing is untested.** I only read. Anything that edits these files should be tried on a copy first, and the BLOB encoders checked byte-for-byte against the originals.

## Method

The four databases were copied to a scratch directory and opened read-only with Python's `sqlite3`. Nothing on the drive was modified. Decoding was cross-checked: beat-grid BPM against `bpmAnalyzed`, sample counts against the track data, and exact-length parsing for the cue and loop BLOBs.
