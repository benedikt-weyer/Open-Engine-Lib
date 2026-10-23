# Engine DJ library format

Findings from inspecting a real Engine DJ library on an external drive (a Denon SC LIVE 4 standalone-player library, Engine schema 3.0.2). Everything below was observed in that library, either by reading the files directly or by decoding them and cross-checking the result. Where something is inferred rather than verified, it says so.

## Verdict: readable, not encrypted, not obfuscated

| Part | Format | Readable? |
| --- | --- | --- |
| `Database2/*.db` | Plain SQLite 3 (no SQLCipher, no key) | Yes, with any SQLite client |
| Track metadata, playlists, history, smartlists | Ordinary SQL columns, UTF-8 text | Yes |
| Cues, loops, beat grid, waveform | BLOBs: a 4-byte length plus zlib, or raw binary | Yes, after decoding (see below) |
| Album art | 256×256 PNG or JPEG BLOBs in `AlbumArt`, JPEG files in `Artwork/`, or a streaming URL | Yes |
| Overview `.rgb` files | The same zlib BLOB format as the waveform column | Yes |
| `user.profile` | Plain JSON | Yes |
| `.stems` files | Plain MP4 container, one 8-channel AAC stream | Container yes. The audio would not decode in ffmpeg 9.0.2 (see [Stems](#stems)) |
| `coredumps/` | zstd-compressed crash dump, journal and a text metadata file | Yes (zstd + ELF core + text) |

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
│   ├── Artwork/<base64url-sha1>.jpg
│   ├── Stems/<originTrackId> <originDatabaseUuid>.stems
│   ├── coredumps/<product>_-_<version>.<commit>/   crash dumps from the player
│   └── user.profile        player settings (JSON)
├── <music folders>/        the audio files, referenced by relative path
├── SoundSwitch/            SoundSwitch lighting projects (*.ssproj, *.plfix), not Engine data
└── System Volume Information/   Windows, not Engine data
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
`id`, `hash` (indexed), `albumArt` (BLOB). `Track.albumArtId` points here. Rows come in five kinds, told apart by the SQLite type of `hash` and by what `albumArt` holds:

| Kind | `hash` | `albumArt` | Where the image lives | Rows `m.db` / `hm.db` |
| --- | --- | --- | --- | --- |
| Streaming | text `URI:<hex>` | the text `image://streaming/<percent-encoded cover URL>` | Not stored, fetched from the URL | 165 / 2 |
| In-database PNG | text `<hex>` | 256×256 8-bit RGB PNG | In the BLOB | 1,973 / 162 |
| In-database JPEG | text `<hex>` | 256×256 JPEG | In the BLOB | 0 / 162 |
| On disk | **20-byte BLOB** (raw SHA-1 digest) | NULL | `Artwork/<base64url(hash)>.jpg` | 490 / 448 |
| Empty | NULL | NULL | none | 1 / 1 |

#### How the hash is computed

**Text hex format.** Take the SHA-1 digest, split it into five big-endian 32-bit words, format each word as hex **without zero padding** and concatenate. This is why text hashes are sometimes shorter than 40 characters (in `m.db`: 40 chars on 1,405 rows, 39 on 475, 38 on 81, 37 on 10, 36 on 2). A plain `sha1().hexdigest()` only matches when no word starts with a zero nibble. In Python: `"".join(f"{w:x}" for w in struct.unpack(">5I", hashlib.sha1(data).digest()))`.

**What is hashed, per kind** (checked against the real data):

| Kind | Input to SHA-1 | Verified |
| --- | --- | --- |
| Streaming | The stored `albumArt` text, i.e. the whole `image://streaming/...` string, with `URI:` prepended to the result | 167 of 167 rows |
| In-database JPEG | The stored JPEG bytes | 162 of 162 rows |
| On disk | The original cover image as embedded in the audio file (raw digest, not hex) | 5 of 5 tracks tested |
| In-database PNG | **Not identified** (see below) | 0 matches |

**The PNG rows.** The hash is *not* the SHA-1 of any of these: the stored PNG bytes, the decompressed PNG data with or without filter bytes, the decoded pixels in RGB, BGR, RGBA, BGRA or ARGB order (25 images tested), or the cover embedded in the audio file (0 of 60 tracks).

What I could establish:
- For 317 tracks that appear in both databases with different representations (a PNG row in `m.db`, an on-disk row in `hm.db`), the old text hash and the new raw hash map one to one (308 distinct pairs, no fan-out). So the old hash is a function of the cover image, but of some transformed form of it rather than the original bytes.
- That is consistent with the library having been written by older software that hashed a 256×256 thumbnail it generated, then stored it as a PNG. Two PNGs carry a `CREATOR: gd-jpeg ... quality = 100` text chunk, which suggests the thumbnail started life as a JPEG. This is an inference. I could not reproduce the bytes, because there is no tool here that encodes a JPEG exactly like the original software.
- The three storage schemes look like successive versions of the software: PNG rows, then JPEG rows, then files in `Artwork/`. The same track can show both an old PNG row in `m.db` and a new on-disk row in `hm.db`.

For an implementation: treat text hashes as opaque ids. They can be recomputed for streaming and JPEG rows and for new on-disk rows, but not for old PNG rows, so do not generate or "repair" them.

`Track.albumArt` is the constant string `image://planck/0` on 2,455 tracks and NULL on the rest. `Track.albumArtSourceHash` was NULL on every row.

### Playlists
- `Playlist`: `id`, `title`, `parentListId` (0 = top level), `nextListId` (0 = last sibling), `isPersisted`, `lastEditTime`, `isExplicitlyExported`. Siblings form a **singly linked list** through `nextListId` (unique per parent), not an order column.
- `PlaylistEntity`: `id`, `listId`, `trackId`, `databaseUuid`, `nextEntityId` (0 = last track), `membershipReference`. Tracks in a playlist are likewise a linked list, in play order. An entity names its track as `(databaseUuid, trackId)`, which matches the track's `(originDatabaseUuid, originTrackId)`, not necessarily `Track.id`. In the library I inspected the two ids happened to be equal for every track and all 6,264 entities resolved this way, but a reader should resolve through the origin pair (as Open Engine Lib does) because tracks merged in from another database keep their original ids.
- Views `PlaylistPath`, `PlaylistAllParent` and `PlaylistAllChildren` flatten the hierarchy. Triggers keep the lists consistent on insert and delete, and propagate `isPersisted` up and down the tree.

### `Smartlist`
Keyed by `listUuid`. The rules are JSON text in `rules`, e.g. `{"match":"all","rules":[{"col":"genre","con":"LIKE","param":"'%Techno%'","v":"2.20.0"}],"rv":2}`. Parent and next are referenced by path/UUID instead of integer id.

What Open Engine Lib does with it (the first two points are **observed** on one library with five lists, the rest is **inferred**):
- `parentPlaylistPath` is the parent as the titles from the top, each followed by `;` (empty at the top level), and `nextPlaylistPath` names a regular playlist the same way. `nextListUuid` links to the next smartlist.
- A chain of smartlists ends at `nextListUuid = ''`, with `nextPlaylistPath` naming the regular playlist it stands before (empty: after all of them). Regular playlists cannot point at a smartlist, as `Playlist.nextListId` is an integer.
- `param` is an SQL literal, so text is in single quotes and `LIKE` is SQL `LIKE`. Besides `genre`, the columns `title`, `artist`, `album` and `bpm` are evaluated, with `LIKE`, `NOT LIKE`, `=`, `!=`/`<>` and for `bpm` also `<`, `<=`, `>`, `>=`. Any other rule matches no track. `match` is `all` or `any`.
- Open Engine Lib writes a new smart playlist at the top level with a random version 4 UUID, after the smart playlists that are there (the old last one gets `nextListUuid` set to it and `nextPlaylistPath` cleared, the new one takes over its `nextPlaylistPath`, which keeps the two unique constraints satisfied), `lastEditTime` set to the current UTC time and rules in the form above (`"v":"2.20.0"`, `"rv":2`, text parameters in single quotes). **This has not been tried on a real Engine player**; the working copy is only written to the source when the user syncs.
- The `Smartlist` table has no tracks of its own: the tracks of a smart playlist are worked out when the library is opened and are not written back.

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

Nine trailing bytes after the adjusted markers were not identified. Open Engine Lib uses the adjusted list (falling back to the analysed one) and draws beats evenly spaced between markers; beat numbers divisible by 4 start a bar, which matches the first marker's number in the tracks I looked at (-4) but is otherwise an assumption. Sanity check: BPM computed from the first and last marker (`60 × sampleRate × Δbeats / Δsamples`) matched `bpmAnalyzed` within 0.6 BPM for 3,265 of 3,284 tracks. The rest are likely variable-tempo grids or grids edited after analysis, which I did not investigate.

### `quickCues` (big-endian)
`i64 count` (8 in the tracks I decoded), then per cue: `u8 label length`, label bytes, `f64 sample position` (-1.0 means unset), `4 bytes ARGB colour`. Then a 17-byte tail: `f64`, `u8`, `f64`. These look like the main cue point (adjusted and default) with an override flag.

### `loops` (raw, **little-endian**)
`i64 count` (8), then per loop: `u8 label length`, label bytes, `f64 start`, `f64 end` (both -1.0 when unset), `u8 start-set`, `u8 end-set`, `4 bytes ARGB colour`. The parse consumed every byte of the BLOB exactly.

### `overviewWaveFormData` and `OverviewData/*.rgb` (big-endian header)
Header: `i64` (1024), `i64` (1024), `f64` samples per entry (total samples / 1024). Then 3 bytes per entry. There are 1025 entries for a declared count of 1024. The meaning of the 3 bytes per entry (likely low, mid and high band heights) was not verified. The `.rgb` overview files use this same layout (see [OverviewData](#overviewdata)), but their entry values are not the same as the database BLOB.

## Files outside the databases

### Artwork

`Artwork/` holds 808 JPEG files, all 256×256 baseline JFIF (749 colour with DPI density, 33 colour with DPCM density, 22 grayscale, 4 with a JPEG comment segment).

- **Filename:** the **SHA-1 digest (20 bytes) of the original cover image as embedded in the audio file, base64url-encoded without padding** (`-` and `_` alphabet, 27 characters, then `.jpg`). I verified this on 5 of 5 tracks by extracting the embedded picture with ffmpeg, hashing it and finding the file.
- **Link to the database:** an `AlbumArt` row whose `hash` is a 20-byte BLOB and whose `albumArt` is NULL. Base64url-encode the `hash` bytes to get the filename. All 490 such rows in `m.db` and all 448 in `hm.db` resolve to an existing file, and every one of the 808 files is referenced by at least one row, so there are no orphans in this library.
- **Image size:** the files are re-encoded thumbnails, not the original images. The in-database PNGs are 256×256 too.
- Both databases can point at the same file, so the 938 rows share the 808 files.

### OverviewData

`Database2/OverviewData/<database-uuid>/<trackId>.rgb`.

- **Folders:** 16 UUID folders, 4 with files and 12 empty. The folder UUID is a database UUID: either the library's own (`Information.uuid` of `m.db` or `hm.db`) or the UUID of a database a track originally came from (`Track.originDatabaseUuid`). `3ea790a9…` held 450 files and is the origin of 3,273 of the 3,294 tracks. `a422b480…` is `m.db` itself (47 files, while only 21 tracks have it as their origin, so I could not explain the rest), and `febbed2a…` is `hm.db` (43 files). The fourth non-empty folder (82 files) matches no database in this library.
- **File name:** the track id **in the database named by the folder**, i.e. `originTrackId` for an origin UUID. That is why a track's `.rgb` cannot be found by `Track.id` alone.
- **Content:** the same format as `PerformanceData.overviewWaveFormData`: 4-byte big-endian length, then zlib, decompressing to 3,099 bytes (24-byte header and 1,025 three-byte entries). The first 24 bytes (header) were identical to the database BLOB. The entry bytes were not: the maximum of each channel is roughly 0.65× the value in the database BLOB, but the shape is similar. My reading is that it is the same waveform with different gain or normalisation, but I did not establish what each one is used for.
- **Coverage:** 622 files in total. 469 matched a track in `m.db` through `(originDatabaseUuid, originTrackId)`, and none was byte-identical to the database BLOB. The other 153 matched no track in `m.db`. I did not check whether they belong to `hm.db`, so I cannot say if they are stale.

### Stems

`Stems/<originTrackId> <originDatabaseUuid>.stems`, 351 files, 8.2 GB, about 13 to 36 MB each.

- **Key:** the name is `originTrackId` and `originDatabaseUuid` from `Track`, separated by a space. All 351 files matched exactly one `Track` row, and all carry the UUID `3ea790a9…`. There is no stems column or table in `m.db`, so the filename is the only link.
- **Container:** plain MP4 (`ftyp` brand `isom`, compatible `iso2`, `mp41`). Top-level boxes in order: `ftyp`, `free`, `mdat`, `moov`. `moov` holds `mvhd`, a single `trak` and `udta`.
  - `trak` has `tkhd`, `edts/elst`, and `mdia` with `mdhd`, `hdlr` (handler `soun`, name `SoundHandler`), `minf/smhd`, `dinf/dref` and `stbl`.
  - `stbl` has `stsd` (one `mp4a` entry: 8 channels, 16-bit field, 44,100 Hz), `stts`, `stsc`, `stsz`, `stco` and `sgpd`/`sbgp` (`roll` grouping).
  - `udta/meta/ilst` holds only the tag `©too = Lavf58.76.100`.
- **Audio:** **one** AAC-LC stream, 44.1 kHz, 8 channels, about 645 kbit/s, same duration as the track. Written with ffmpeg (`Lavf58.76.100`, encoder `Lavc58.134.100`).
- **Channel configuration:** the AudioSpecificConfig has channel configuration 0 and a program config element (PCE) listing five elements: CPE, SCE, CPE, CPE and SCE (3 pairs and 2 mono channels, 8 channels in total). I expected four stereo pairs, which is what the player's stems feature suggests, but the PCE does not say that.
- **No stem metadata:** there are no stem names, colours or order stored in the file, and I found none in the database. Which channels hold drums, bass, melody and vocals is **unknown**.
- **Decoding:** ffmpeg 9.0.2 identifies the file but could not decode it. About 99.9% of packets failed with `Prediction is not allowed in AAC-LC` / `Reserved bit set`, with both the `aac` and `aac_fixed` decoders. I did not find the cause. One guess is that the real element order in the bitstream differs from the PCE. So I could not confirm the channel layout or play the audio.

### user.profile

JSON, 9.1 KB, one object:

```json
{ "data": { "User": { "childorder": ["Name", "..."], "children": { "<Key>": { "type": 4, "string": "Cue Position" } } } } }
```

`childorder` is the display order of the settings and lists exactly the same 58 keys as `children`. Each entry has a numeric `type` that decides which other field holds the value:

| `type` | Value field | Meaning | Keys (count) |
| --- | --- | --- | --- |
| 0 | `value` (number) | Integer | `TrackEndWarningTime` (1) |
| 1 | `state` (bool) | On/off switch | 21 |
| 4 | `string` | One choice from a fixed list, stored as its display text | 23 |
| 8 | `string` | Free text | `Name` (1) |
| 16 | `color` | `#AARRGGBB` colour | 12 |

The keys, by type:

- **Type 1 (switches):** `OnAirMode`, `NeedleLock`, `ColorizeKey`, `CueToLoopIn`, `SavedLoopBehaviour`, `PausedSavedLoopBehavior`, `LinkBeatJumpLoopSize`, `TimeDisplayMode`, `PadLock`, `IntegrateDemoContent`, `IntegrateSamplerDemoContent`, `DisplayFileName`, `RemoteStartStop`, `PausedHotCueBehaviour`, `PlatterMode`, `MaintainPlayState`, `CensorBehavior`, `MotorizedPlatterTorque`, `TurntableMode`, `StemFxBehavior`, `ShowStemsList`.
- **Type 4 (choices, with the values seen):** `DefaultPlayBackPosition` (`Cue Position`), `LockPlayingDeck` (`On`), `KeyDisplayMode` (`Camelot`), `QuantizeToGrid` (`1`), `LoopOutMode` (`Off`), `SyncMode` (`Beat`), `SyncButtonAction` (`Shift Disable`), `SyncIndicator` (`Beat Phase`), `PitchBendAction`, `PitchBendBehavior`, `KeySyncMode`, `BpmTolerance` (`±3`), `KeyCompatibility`, `DefaultSpeedRange` (`±8`), `DefaultLoopSize` (`4`), `DefaultBeatJump` (`4`), `BPMRange` (`88-175`), `KeyAdjust`, `CollectionBrowseBehavior`, `Product` (`4 Deck Consoles`), `NudgeSensitivity`, `TrackSpeed` (`33 RPM`), `WaveformStyle` (`Tri-Band`).
- **Type 16 (colours):** `PlayerColor1` to `PlayerColor4` and `PlayerColor1A`/`1B` to `4A`/`4B`.

Numbers such as loop size are stored as **strings**, and the allowed values per key are not in the file. The profile is device-specific (`Product`, `TrackSpeed`, platter and turntable keys), so a settings editor should keep unknown keys and types intact.

### coredumps

`coredumps/<ProductName>_-_<version>.<commit>/` (here `SC LIVE 4_-_4.3.4.8e09db28ef`) holds one crash with three files sharing the stem `<process>.<UTC timestamp>`:

| File | Content |
| --- | --- |
| `.metadata` | Plain `KEY=value` text in sections: process (`PID`, `comm`, `signal`, `timestamp`), `# Hardware` (`PRODUCT`, `PCB_REV`, `SERIAL`), `# /etc/az01-release`, `# /etc/os-release`, `# uname -r`, and a `# Planck` JSON block (`Commit`, `ProductCode`, `ProductName`, `Version`, `airAppReleaseType`). |
| `.log.zst` | zstd-compressed system journal of the player, 1,105 lines from kernel boot to the crash. |
| `.core.zst` | zstd-compressed 32-bit ARM ELF core dump of `/usr/SoundSwitch/SoundSwitch -platform offscreen`. |

The crashed process was `SoundSwitch` (the lighting-control app that runs on the player), killed by signal 6 (SIGABRT). The metadata and the log show the player firmware, which is a Buildroot Linux 6.1.111 PREEMPT_RT on ARMv7 with a dm-verity read-only rootfs. These files describe the device, not the library, so a library manager does not need them. The metadata contains the **device serial number** and the log may contain other personal data, so do not publish them or commit them to the repository.

## Gotchas for an implementation

- **Open read-only, or on a copy.** Writing while Engine DJ or the player has the database open risks corruption. The `*.db-journal` files are SQLite rollback journals, so a non-empty one means an interrupted write. A library on a FAT/exFAT drive has no permission protection either.
- **Paths are relative** to the `Engine Library` folder and use `/`. In this library 815 of 3,123 non-streaming tracks did not resolve to an existing file, so a viewer must tolerate missing files (`isAvailable` exists but I did not compare it).
- **Streaming tracks** have `streamingSource` set and a path into the player's own storage, not the drive.
- **Playlist and entity order is a linked list**, not a position column. Writers must also maintain `nextListId`/`nextEntityId` and `isPersisted`, which triggers partly handle.
- **Triggers and ids**: ids are never recycled. Inserting a track must go through SQL so the `PerformanceData` row and origin fields are created.
- **Mixed endianness** in BLOBs, as above.
- **Writing is untested.** I only read. Anything that edits these files should be tried on a copy first, and the BLOB encoders checked byte-for-byte against the originals.

## Method

The four databases were copied to a scratch directory and opened read-only with Python's `sqlite3`. Nothing on the drive was modified. Decoding was cross-checked: beat-grid BPM against `bpmAnalyzed`, sample counts against the track data, and exact-length parsing for the cue and loop BLOBs. The other files were inspected with `ffprobe`/`ffmpeg` (stems, and extracting embedded covers to hash them), `zstd` and `file` (coredumps), and short Python scripts (MP4 boxes, the AAC config, JSON, SHA-1/base64url matching).
