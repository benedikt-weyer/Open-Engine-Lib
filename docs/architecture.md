# Architecture

Open Engine Lib is two programs that run side by side and talk over gRPC.

```
┌──────────────────────┐   gRPC (openenginelib.v1)    ┌─────────────────────────┐
│ open-engine-lib      │ ───────────────────────────▶ │ open-engine-backend     │
│ the UI (iced)        │   127.0.0.1:<port>, token    │ library + audio         │
│ spawns the backend ──┼─────── stdin / stdout ─────▶ │                         │
└──────────────────────┘                              └───────────┬─────────────┘
                                                                  │ copies once
                                         Engine Library/Database2 ▼ ~/.cache/open-engine-lib
                                         (m.db, hm.db, …)           (working copy, read here)
                                         audio files ◀── played in place, never copied
```

| Crate | Directory | Role |
| --- | --- | --- |
| `open-engine-lib` | `src/` | The UI: windows, settings, localisation, the library browser. Knows nothing about SQLite or audio. |
| `open-engine-backend` | `backend/` | Reads the Engine library, owns the playback queue, plays audio (rodio). |
| `open-engine-api` | `api/` | The `.proto` files and the generated tonic code both sides use. |

All three share one version (`[workspace.package]` in the root `Cargo.toml`).

## Starting and stopping

1. The UI looks for `open-engine-backend` next to its own executable (override: `OPEN_ENGINE_BACKEND`).
2. It starts it with a random per-launch secret in `OPEN_ENGINE_BACKEND_TOKEN` and keeps the backend's stdin pipe open.
3. The backend listens on `127.0.0.1:0` and prints `LISTENING <address>` as its first stdout line. It prints nothing else on stdout; logs go to stderr.
4. The UI connects and calls `BackendService.GetInfo`. It continues only if `supported_api_versions` contains the version it speaks.
5. Every request has to carry `authorization: Bearer <secret>`, otherwise the backend answers `UNAUTHENTICATED`. This keeps other programs on the machine from using the loopback port.
6. The backend exits when its stdin closes. That happens when the UI exits or is killed, so the backend never outlives it. The UI also kills it when it drops its connection.

If the backend is missing or fails to start, the library page says so and offers to try again.

## The API and its versioning

The protocol lives in `api/proto/openenginelib/v1/`:

- `backend.proto`: `BackendService.GetInfo`, the version handshake.
- `library.proto`: `LibraryService`: `OpenLibrary`, `CloseLibrary`, `SetRating`, `ListTracks` (server streaming, in batches), `ListPlaylists`, `GetAlbumArt`, `GetWaveform`, `GetCues`, `SetCues`, `GetLoops`, `SetLoops`, `OpenLocalFolder`.
- `playback.proto`: `PlaybackService`: `Play`, `TogglePause`, `Stop`, `Next`, `Previous`, `Seek`, `GetStatus`. The backend owns the queue and moves on to the next track by itself; the UI polls `GetStatus` while playing.

Versioning rules:

- The major version is the proto package: `openenginelib.v1`. `GetInfoResponse.supported_api_versions` lists the versions a backend serves.
- Within a version, changes must be additive only (new fields, new RPCs, new enum values). Never renumber, retype or remove anything.
- A breaking change becomes `openenginelib.v2` in `api/proto/openenginelib/v2/`. A backend can serve `v1` and `v2` at the same time, and the UI picks one it supports. `API_VERSION` in `api/src/lib.rs` is what this build of the UI speaks.
- Track and playlist ids are only valid while a library is open; they are indices, not the Engine database's ids.

The `.proto` files are compiled in pure Rust (`protox`), so building needs no `protoc`.

## The menu bar: Open, Reload, Save as, Exit

A **File** menu sits at the top of the window: **Open…** (the same as "Open library…"), **Reload from disk** and **Save as…** (both only while a library is open) and **Exit** (which saves the session first, like closing the window).

**Reload from disk** replaces the working copy by what is on the disk: for an Engine library it is `PullFromSource` (the same as "Replace the working copy with the source" in the sync window, but without the comparison first), after which the UI fetches the library again and keeps the list, the song and its place in it; edits that were not synced are moved to `unsynced-edits` in the cache folder and the menu bar says where. For a folder of music the folder is scanned again. While it runs, nothing else may open or replace the library (`State::locked`, which also covers saving).

**Save as…** asks for a folder and calls `SaveLibraryAs`. The backend clones the *working copy* of the databases (the cache, which is what the app reads) into a new `Engine Library` folder inside the chosen one (or into the chosen folder if it is called that), and only when that has succeeded does it make the new place the library's location: it opens the clone, so the app reads from a working copy of the new place from then on, and the recent libraries and the saved session refer to it.

- The audio files are not copied, they stay where they are. The databases record each song's path relative to the library folder, so in the clone those paths are rewritten (in `m.db` and in the history in `hm.db`) to lead to the same files from the new folder (`../Music/a.flac` becomes `../../Music/a.flac` for a library one folder deeper). Streaming tracks have no file here and are left alone.
- The `Artwork` folder is copied too, since some covers only exist there and it is small. `Stems` and `OverviewData` are not.
- The clone is built in a temporary folder next to its place and renamed into it at the end, so the destination never holds a half-made library, even if the program is stopped half way. If anything fails, everything made is taken away again and the library keeps its old place. A destination that already holds a library is refused, as is one that is not empty, and so is the library's own place.
- Playback is not interrupted and track and playlist ids don't change, so the list shown, the selection and the song playing carry on; the UI just fetches the tracks again to learn their new paths. While a save is running the library can't be closed or replaced.
- The clone has the same database identity (UUID) as the original: it is a copy, not a new library.

## Syncing the working copy to the source

The **Sync with the source…** button in the sidebar opens a window that compares the working copy (the cached databases the backend reads and could change) with the source (the library at its location) and can write the working copy to the source. `GetSyncDiff` says what syncing would change, `SyncToSource` does it.

**The comparison** looks at the main database `m.db` (opened read-only, with the source attached as `immutable`, so another program's locks and journals are left alone):

- tracks that are only in the working copy (added), only in the source (removed), or in both but different: the fields that changed with the old and new value (title, artist, album, genre, comment, label, composer, remixer, year, BPM, key, rating, path, length, file type, analysed, grid locked) and whether the beatgrid, cues or loops changed;
- playlists that are added, removed, renamed or moved, and for each the tracks added and removed and whether their order changed; smart playlists too (marked as such), with whether their rules changed (the order of smart playlists among each other is not compared);
- which database files differ at all. `hm.db`, `sm.db` and `stm.db` are not itemised: they are written when syncing but only named in the window.

Lists are cut at 200 entries (the counts are complete). Matching is by Engine's own ids, which a working copy shares with its source.

**Syncing** copies the working copy's databases that differ over the source's, with these protections:

- It refuses if the source can't be reached (a drive that is gone) or another program is writing to it right now (a journal or write-ahead log with something in it).
- It notices when the source was changed *outside the app* after the working copy was made (the source's files no longer match what the working copy was made from). Syncing would then undo those changes, so the window says so and requires ticking a box before it can go ahead.
- What it replaces is first kept in `Database2/open-engine-lib-backup` in the source (the latest only, so the drive doesn't fill up).
- Each file is written next to its place and moved over it in one step, so a file is never half written, and if a later file fails the earlier ones are put back from the backup.
- Afterwards the working copy and the source count as the same, so nothing shows as changed from outside.

**Edits:** the app changes the working copy (star ratings, cues and loops, see below) and never the source: an edit reaches the source only through this window. Opening a library re-copies the working copy when the source has changed since. So that this can't throw edits away, the backend leaves an `edited` mark in the cache folder when it changes the working copy, which writing to the source clears. A working copy that has the mark and is replaced because the source changed is first moved to `unsynced-edits/` in the cache folder (the latest only), so the edits can still be recovered from there. The app doesn't say so yet.

## Ratings and SoundSwitch lighting

Two more columns in the track list.

**Rating** is the track's star rating from the library: 0 to 100, 20 to the star, shown as five stars (`★★★☆☆`). Libraries that were imported from elsewhere can hold other values (14, 27, 53 ...), which are rounded to the nearest star.

**Setting the rating:** each star in the list is a button. Clicking the third star gives the track three stars, and clicking the last filled star takes all of them away (a rating of 0). The click shows at once and `SetRating` writes `Track.rating` in the working copy (the rating is checked to be 0 to 100; the app sends 20 per star), which the database's own trigger stamps with a new `lastEditTime`. The source only gets it when you sync, where it shows as a changed `rating`. If the backend can't write it, the stars go back to what they were and the error is shown. Ratings can't be set while the library is being saved elsewhere.

**Lighting** says whether SoundSwitch, the lighting software, has lighting for the track: a filled dot if it was rendered, a ring if SoundSwitch only analysed the track, a dash if the track isn't in SoundSwitch. Pointing at it says so in words and names the projects. Streaming tracks have no mark.

SoundSwitch's files are not documented, so how this is read was worked out from the files on a real drive, and the central step is an **inference**. The full findings are in [`soundswitch-format.md`](soundswitch-format.md), in the way [`engine-library-format.md`](engine-library-format.md) describes the Engine library:

- SoundSwitch keeps projects in `<drive>/SoundSwitch/<name>.ssproj` folders. `SoundSwitchTrackMap.bin` in each lists the tracks it knows by the path of their audio file as Windows saw it (`F:\Music\a.flac`, or through `D:\Engine Library\..\Music\a.flac`), as UTF-16. After the path of an analysed track comes a marker and a GUID.
- That GUID names the track's data in the project folder: `{GUID}.ssfile` exists for every analysed track (checked: all 463 `.ssfile` of the player's project, and 315 of 381 in the other project, are named in their track maps).
- Most also have `{GUID}.ssa`. These are small and compressed or encrypted, so their contents could not be read. They are **taken to be the rendered lighting** (SoundSwitch's "autoscript"), because they are made in the same moment as the `.ssfile` of a track that was given lighting and a project whose tracks were only analysed has none. If that is wrong, the dot means something else.
- A track is matched to the library's by the path of its file relative to the drive, compared without the drive letter, with `\` read as `/`, `..` worked out and upper and lower case treated alike. Every project in the folder counts; a track has lighting if any project has an `.ssa` for it.

On the drive I developed this with, 283 of 3,123 local tracks came out as rendered (all from one project), 229 as analysed only and 2,611 as not in SoundSwitch. SoundSwitch is read when a library is opened; reopen it to see changes.

## The track list

Besides title, artist, album, genre, BPM and length, the list shows where each file is: the path the library records (relative to its `Engine Library` folder, usually starting with `..`) resolved against that folder, so it is a plain path such as `/run/media/you/DRIVE/House/track.flac`. It is worked out from the path alone, without asking the file system. Streaming tracks have no file here and say so. The location is the last column and takes the width the others leave; pointing at it shows the whole path, since a long one is cut off.

The other columns can be resized by dragging the divider after their header (the pointer changes over it). The drag is followed anywhere in the window and ends when the button is let go, which also saves the widths in `<state dir>/open-engine-lib/track-columns.toml`. A column can be between 40 and 1200 pixels wide.

## Waveform and beatgrid

While a song is loaded in the player, the UI shows a DJ-style deck above the player bar: a zoomed waveform (6 seconds) scrolling past a fixed playhead with the beatgrid drawn over it, the first beat of every bar stronger and numbered (`17.1` is the first beat of bar 17, `17.2` the second; beat 0 of the grid is the first beat of bar 1, and beats before it are not numbered), and the whole track below it. Clicking the overview jumps there. How it is colored is a setting (`waveform_colors`, on the Settings page and in the NixOS module) that can also be changed from a menu right under the waveform:

  | Scheme | Look |
  | --- | --- |
  | `mono` | one blue, lighter where there are more highs (the old Pioneer look) |
  | `rgb` | lows red, mids green, highs blue, mixed into one color per slice, as in Serato and rekordbox "RGB" |
  | `stacked` (default) | three layers over each other, lows biggest at the back and highs smallest in front: dark blue, amber and white, as on a CDJ-3000 |
  | `spectrum` | the dominant frequency sets the hue, from red (bass) through green to violet (treble) |

  The height is the loudness of the loudest band in all schemes but `stacked`, where every band has its own layer. The color maths is in `colors.rs`.

- **Waveform:** Engine only stores a coarse overview (1,024 columns for the whole track), too rough to zoom into, so the backend computes its own from the audio file. `GetWaveform` decodes the whole file, splits it into low (below about 600 Hz), mid (about 600 Hz to 4 kHz) and high (above about 4 kHz) with second-order Butterworth filters and keeps the peak of each band per 10 ms. The two split points are settings (`waveform_low_mid_hz` and `waveform_mid_high_hz` in `config.toml` and the NixOS module, 20 to 20000 Hz, the first below the second; nonsense values are replaced by the defaults with a warning on the Settings page). The UI sends them with every `GetWaveform` request, and the backend keeps the measured peaks per file and split. A file with a low sample rate can't hold a high split point, so it is lowered for that file. The config is read at startup, so changing the split needs a restart. A 5–8 minute track takes 1–2 seconds; the backend keeps the last 8. The overview strip is the same data pooled down to 1,200 columns in the UI, by RMS rather than by peak: a bin of a few seconds always contains a peak of every band, so peak pooling made all three bands full height everywhere and the front layer covered the others (in the stacked scheme, a nearly all-white overview).
- **Fixed scaling:** nothing is fitted to the track or to its bands. The backend measures the loudest sample of each band per 10 ms as a share of full scale (a full-scale sine wave in the band is 1). The height drawn is `min(1, peak × gain) ^ 0.8`, the same for every track: a quiet track is drawn lower than a loud one, and a band a track has little of is drawn low. (Mastered music peaks at about 0.9 in the lows, 0.8 in the mids and 0.7 in the highs, with the highs lower on average, so gains of 1 already look natural, with the lows tallest and the highs smallest.) The three gains are settings: `waveform_gain_low`, `waveform_gain_mid` and `waveform_gain_high` (above 0, at most 20; 1 by default), as sliders in a "Waveform levels" section of the Settings page (saved when you let go, and the waveform in the player is redrawn at once) and in `config.toml` and the NixOS module. A gain that makes no sense is replaced by 1 on its own, with a warning on the Settings page. The backend keeps the raw peaks, so changing a gain doesn't analyse the file again, and the overview strip uses the same scale, so it follows the gains too.
- **Beatgrid:** this is Engine's own analysis, read from `PerformanceData.beatData` (adjusted grid if there is one, else the analysed one) and sent as markers. Between two markers the beats are evenly spaced, before the first and after the last they carry on at the nearest spacing, and beats whose number is a multiple of 4 start a bar. Tracks without a grid just have no lines, and the deck says so.
- **Delay:** the sound reaches the speakers a little after the player has played it (the audio buffer, the sound server, Bluetooth...), so a waveform drawn at the reported position runs ahead of what is heard. `waveform_delay_ms` (a slider on the Settings page, `config.toml` and the NixOS module; -500 to 500 ms, 0 by default, saved when you let go) draws the whole deck, the zoomed waveform with its beatgrid and the overview's playhead, that long behind the reported position (`Clock::delayed`); negative moves it ahead. It changes only what is drawn: clicking the overview still seeks to the point clicked.
- **Smooth scrolling:** the backend's position arrives about four times a second, but each report is a little early or late. The UI keeps its own clock (`clock.rs`) that runs on by itself and only leans 15% towards each report, snapping to it only when it is more than 0.3 s away (a seek, another track, a pause). The backend reads the position fresh for every status request. The canvas asks for a redraw every frame while playing.
- **Smooth drawing:** each band is drawn as one filled outline through the (lightly smoothed) columns, not a bar per column, and the window is anti-aliased, so the waveform doesn't shimmer as it scrolls.
- **Checked on a real library:** for every track tried the grid's tempo equals the library's BPM, and the strongest kick onsets of four-on-the-floor tracks fall 10–30 ms after the grid line, which is the detection window's own lag, so the lines sit on the beats.
- **Cues and loops:** the cues (hot cues) and saved loops of the track are drawn over both views, see [Cues](#cues) and [Loops](#loops).
- **Not done:** no scrubbing by dragging, no zoom control, no looping of the playback itself, and no waveform for streaming tracks (they have no file).

## Cues

A track has eight cue pads (Engine's hot cues). Each can hold a cue: a position, a name and a color. They are stored in `PerformanceData.quickCues` (see `docs/engine-library-format.md`); `backend/src/cues.rs` reads and writes that blob, and keeps the part of it that is not about the pads (the main cue point) as it was. The main cue point itself is not handled; for loops see [Loops](#loops).

- **Reading:** `ListTracks` gives every track's `cue_count`, which the track list shows in its **Cues** column (a dash for none). `GetCues` gives a track's cues with their positions in seconds (the blob has samples; the sample rate comes from `trackData`, else `beatData`). The UI asks for the cues of the track in the player once, when it starts playing.
- **Showing:** on the zoomed waveform a cue is a line in its color with a flag at the bottom with the pad's number and its name (the name is left out when it is Engine's default `Cue N`). The overview has a line with a small square on top. The cues are drawn anew every frame over the cached pieces of the waveform, so changing them doesn't redraw the waveform.
- **Pads:** under the waveform there is a row of eight pads. A pad with a cue is shown in the cue's color, and clicking it jumps there; right-clicking it removes the cue. Clicking an empty pad sets a cue at the playhead, with Engine's name and color for that pad (`Cue 3`, and a color per pad). "Edit cues…" opens a window.
- **Editing:** the window has a row per pad with the color to choose from, the name, the position as `m:ss.mmm` (or plain seconds), and buttons to take the position from the playhead, to jump there and to remove the cue; an empty pad has "Add at playhead". Nothing is written until **Save**, which sends all eight pads at once. The playhead used is the one drawn (the waveform delay is taken into account), so a cue lands where it was heard.
- **Writing:** `SetCues` replaces the cues of a track in the working copy: the slots in the request are the ones the track has afterwards, and the others are emptied (so adding, changing and removing are all the same call, with the list from `GetCues` changed). It checks that the slots are 0 to 7 and not repeated, the positions are within the track and the names at most 255 bytes, and refuses (`FAILED_PRECONDITION`) a track that has no analysis to keep cues in (`PerformanceData` row or sample rate missing) and a `quickCues` blob it can't read, which it never writes over. A track with a row but no cue blob gets a new one (eight empty slots). The source only gets the change when you sync, where it shows as changed "cues". Cues can't be changed while the library is being saved elsewhere, nor while another change is being written.
- **Not done:** only the track in the player can have its cues edited (that is where the waveform and the playhead are). A cue is not snapped to the beatgrid. Writing cues has not been tried on a real player, though a blob that is read and written again has the same content as it had, and the backend reads the cues of a real library correctly (`reads_the_cues_of_a_real_library`, ignored).

## Loops

A track also has eight loop pads (Engine's saved loops), stored in `PerformanceData.loops` (uncompressed and little-endian, see `docs/engine-library-format.md`). `backend/src/loops.rs` is the codec, written like `cues.rs`; the library code that reads the blobs, counts and writes is shared with the cues. Saved loops are only data here: the player does not loop (playing a loop and the active loop on load are not handled).

- **Reading:** `ListTracks` gives `loop_count`, shown in the **Loops** column. `GetLoops` gives the loops with start and end in seconds. A slot counts as a loop when both ends are set and the end is after the start, so a half-set slot is neither shown nor counted.
- **Showing:** on the zoomed waveform a loop is the stretch it covers tinted in its color with a line at each end and a flag above the cues' flags (`L1`, with the name when it isn't Engine's default `Loop N`). The overview has the stretch tinted with a bar along its bottom.
- **Pads:** a second row of eight pads under the cue pads. A pad with a loop is shown in the loop's color; clicking jumps to its start, right-clicking removes it. Clicking an empty pad saves a loop at the playhead that is four beats long (at the tempo of the track's beatgrid, else of the library, else 2 seconds), moved back if it would pass the end of the track. "Edit loops…" opens a window like the cues' one, with a start and an end per loop, each with a button to take it from the playhead, and "Go to the start". Nothing is written until **Save**, and the end must be after the start.
- **Writing:** `SetLoops` replaces the loops of a track in the working copy like `SetCues` does: the slots in the request are the loops the track has afterwards, loops in other slots are removed, and a slot that was only half set is left alone. It checks the slots (0 to 7, once each), the positions (the start 0 or more, the end after the start and not past the end of the track) and the names (255 bytes), and refuses a track without analysis or with a `loops` blob it can't read. The source gets the change when you sync, where it shows as changed "loops". Not while the library is being saved elsewhere or while another change to the loops is being written.
- **Not done:** as for the cues: only the track in the player, no snapping to the beatgrid, and writing has not been tried on a real player. The backend reads the loops of a real library correctly (`reads_the_loops_of_a_real_library`, ignored).

## Local music

Besides Engine libraries, a plain folder of music files can be opened. The **Local music** section of the sidebar lists the folders that were added (kept in `<state dir>/open-engine-lib/local-folders.toml`, in the order they were added); **Add local music folder…** asks for a folder, adds it to the list and opens it, clicking a folder opens it, and the × next to it takes it out of the list (closing it if it is the open one). **Close folder** in the page header closes the open one.

- **One library at a time:** a folder of music replaces the open library, like another Engine library would (the backend holds one). Closing it leaves no library open, and the next start doesn't reopen it; if the app is left with a folder open, the next start goes back to it (`Session.folder`).
- **Tracks:** `OpenLocalFolder` scans the folder (`backend/src/folder.rs`): every file below it that the player can decode (`mp3`, `flac`, `wav`, `ogg`, `m4a`, `mp4`; by extension) is a track. Hidden files and folders (names starting with a dot, like the `._` files macOS leaves on drives) are skipped, and so are links to folders. The tags give the title (else the file name), artist, album, genre and BPM (`BPM` or `TBPM`/`tmpo`), and the file's properties the length; they are read with the `lofty` crate on as many threads as there are cores, without covers. A file that can't be read is still a track, with only its name. A real USB drive with 5,300 files in 3,800 folders took about 20 seconds cold, which the UI shows as "Opening…" meanwhile.
- **Folders:** each folder with music below it is a playlist, in the tree in the sidebar like the playlists of an Engine library, and shows the tracks of all folders below it too (so a folder like `Artist` shows all its albums). "All tracks" is every file, including those directly in the chosen folder. Folders and files are ordered by name, ignoring case.
- **What is missing:** a folder has nothing but what the files say. The track list leaves out the Rating, Cues, Loops, Lighting and Origin columns, there are no cue and loop pads or covers, the waveform has no beatgrid, and nothing can be edited or synced: the buttons for playlists, smart playlists, syncing and saving origins are not shown, and the backend refuses (`FAILED_PRECONDITION`) `SaveLibraryAs`, the sync and playlist calls, `SetRating`, `SetCues` and `SetLoops` for such a library. Nothing is written to the folder or copied.
- **Not done:** covers from the files' embedded pictures, a rescan when the folder changes (close and open it again), and audio formats the player can't decode (AIFF, Opus, ...).

## Restoring the last session

The UI remembers what was open in `<state dir>/open-engine-lib/session.toml`: the library, the playlist (as the titles from the top level down, since indices change between opens), the song (by its `Track.path`, which the library keeps unique and which stays the same across opens) and, if the song was loaded in the player, how far into it. At startup it opens that library and goes back to the playlist and song. A song that was loaded is loaded again **paused** at its position (`PlayRequest.start_paused` and `start_position_ms`), so the app never starts playing by itself. If the playlist or song no longer exists, it falls back to the list of all tracks or to no selection.

- The song that is playing wins over the one that is merely selected, and it brings the list it is playing from.
- The session is saved when something changes, and a playing song's position at most every 5 seconds. Closing the window saves it once more, exactly.
- Closing a library with "Close library" clears the session, so the next start doesn't reopen it. A library that fails to open (say the drive is unplugged) leaves it alone.
- A library path given on the command line wins and nothing is restored.
- Two settings control this (Settings page, `config.toml`, or the NixOS module): `reopen_library` and `restore_position`, both on by default. With `restore_position` off, only the library is reopened.

## Cover images

Tracks carry an `album_art_id` (tracks of one album share it) and the UI fetches the image with `GetAlbumArt` only for what is on screen: the rows it has built plus the playing track. The UI keeps at most 300 covers and forgets the oldest. The backend finds the image the way the library stores it (see `docs/engine-library-format.md`): an image in the `AlbumArt` table, or a JPEG in the library's `Artwork` folder named after the row's raw hash. That folder is read in place, like the audio files, not from the cache. Covers of streaming tracks are only URLs, so they have no image and `GetAlbumArt` answers `NOT_FOUND`. In the library I inspected, 2,570 distinct covers were 1,914 PNGs, 490 JPEGs and 166 unavailable (165 streaming and one empty row).

## The library cache

The backend never works on a library in place: it may sit on a removable drive, and Engine DJ or a player may be using it. On `OpenLibrary` it copies the library's databases (`Database2/*.db`, plus a non-empty `-journal`/`-wal`/`-shm` so SQLite can recover an interrupted write) into a cache folder and reads that copy. Audio files, artwork, stems and overview data are not copied: they are far too large, and tracks are played from where they are.

- The cache lives in `$OPEN_ENGINE_LIB_CACHE`, else `<user cache dir>/open-engine-lib/libraries/<folder name>-<hash of path>/`.
- A `manifest` file lists the copied files with their size and modification time. If the source databases still match it, the copy is reused as it is; otherwise the databases are copied again from scratch (but a copy with edits that were never synced is kept in `unsynced-edits/` first, see "Syncing the working copy to the source").
- The copy is opened read-write so that SQLite can roll back a copied hot journal. Edits (ratings, cues, loops) are written to the copy, and nothing is written to the source library except by syncing.
- `OpenLibraryResponse.cache_path` tells the UI where the copy is.

## Tests

- `cargo test --workspace` runs everything.
- `backend/tests/grpc.rs` starts the real backend process and drives it over gRPC with a fixture library: the token check, the handshake, the cache, tracks and playlists, play errors, and shutdown when the UI goes away. It needs no sound card.
- `serves_covers_from_a_real_library` in the same file is ignored by default and silent: it fetches every cover of a real library and checks the image types (`... --test grpc covers -- --ignored --nocapture`).
- `serves_waveforms_from_a_real_library` (ignored, silent) fetches the waveforms of a real library's first tracks and reports timings, the grid's tempo against the library's, and where the kick onsets fall relative to the beats.
- `plays_from_a_real_library` in the same file is ignored by default. It opens a real library and plays a track through the real audio device (it is audible):
  `OPEN_ENGINE_TEST_LIBRARY=/run/media/you/DRIVE cargo test -p open-engine-backend --test grpc -- --ignored --nocapture`

## Open Engine Lib's own database

Things that are Open Engine Lib's own live in `open-engine-lib.db`, a SQLite file **in the `Database2` folder of each library**, next to Engine's databases (`backend/src/origins.rs`). The working copy has its own, copied and kept like the others, so the file takes part in everything that handles the databases: opening copies it, "Sync with the source" lists it among the changed databases (with how many origins are new or changed) and writes it to the source with a backup, "Save as" clones it. A library has none until an origin is saved. Whether Engine DJ and the players are happy with an extra `.db` file in `Database2` has **not** been checked on real hardware.

It has one table so far, `Origin(databaseUuid, trackId, origin, savedAt)`, with the key `(databaseUuid, trackId)` being the track's `(originDatabaseUuid, originTrackId)` in Engine's database. That pair is the same in every library a track was copied to (as long as Engine kept it when copying, which has not been checked).

`MigrateOrigins` saves the location of the file of every track of the open library (streaming tracks and tracks without an origin pair are skipped) as its origin in the working copy, but only for tracks that have none: a saved origin is never replaced. `ListTracks` gives each track's origin, and the track list shows it in its Origin column. The first time a library gets the file, the origins that earlier versions kept for all libraries in the user data dir (`$OPEN_ENGINE_LIB_DATA`) are taken over for its tracks; that old file is only read.

## Syncing playlists to another library

"Sync to another library" (`GetLibrarySyncDiff` / `SyncToLibrary`, `backend/src/mirror.rs`) makes the playlists and smart playlists of another library the same as those of the open library (its working copy, edits included). It goes one way and only touches the other library's `m.db`:

- Tracks are matched by Engine's `(originDatabaseUuid, originTrackId)`, else by a file name that only one track of each library has. Tracks and audio files are **never copied**: a track the other library lacks is left out of its playlists and counted (and listed) as missing.
- Playlists are matched by their titles from the top (`House / Deep`), smart playlists likewise. A playlist the other library lacks is created, one it has gets the tracks (in the open library's order) and smart playlists get the rules; the order of the playlists among each other is not changed in existing places. What only the other library has stays unless "also delete" is ticked.
- The other library is never edited in place: its `m.db` is copied to a scratch folder next to the library cache, changed there with the same `Library` calls that the playlist editing uses, and for a push the result replaces the original (written beside it and moved over it) after the original was copied to `open-engine-lib-backup/`. A diff is the same work on the copy, thrown away, so it lists what a push does. Another program writing to the library (a journal in its `Database2`) refuses both.
- New `PlaylistEntity` rows are written with `membershipReference = 0`. What that column means is not known (it is 0 for about half of the entries of the real library inspected and 1 for the rest).
- Origins go along: for each track of the open library that has an origin and a match in the other library, the other library's `open-engine-lib.db` gets that origin under its own track key, replacing a different one (the open library is the master). It is listed as a count, and only written, with a backup, when something differs; `m.db` is only replaced when the playlists changed.

## Progress

A sync can take minutes when audio files are copied. `SyncToSource` and `SyncToLibrary` stay ordinary calls that return when they are done; while one runs, the UI asks `GetProgress` (every 250 ms, from a subscription that exists only while a sync window is writing). The backend keeps one `Progress` (`backend/src/progress.rs`) that the running sync updates: a phase (preparing a copy of the other library's database, copying the tracks, changing the copy, writing the databases), the bytes and files done and to do in that phase, and the file being copied. Every copy that matters goes through `Progress::copy`, which copies in 1 MiB chunks, so the bar moves smoothly even for one big file. The bar shows the current phase, not the whole sync: a phase without bytes (changing the copy) only has its name. A diff is not reported on. There is no way to cancel a sync.

`OpenLocalFolder` reports to the same `Progress`, and the UI polls `GetProgress` while a library is opening (the window shows the bar in place of the plain "Opening…" text). The scan has two phases: *scanning* (walking the folders, `done_files` counts the audio files found, no total) and *reading tags* (`done_files` of `total_files`, which is what the bar fills by). Opening an Engine library reports nothing, so its window stays as it was.

## The other way round (back-sync)

Both sync windows can also work in the other direction, chosen with a radio button in the window (each window starts in the usual direction):

- **Sync with the source**, *Replace the working copy with the source* (`PullFromSource`, `GetSyncDiff` with `reverse`): the source's databases (all `*.db` files, the origins too) are copied over the working copy. Edits in the working copy that were not synced are first moved to `unsynced-edits` in the cache folder (the response says where), exactly as when opening a library whose source changed. The diff for this direction is the usual one with the roles swapped: "added" is what the source has and the working copy lacks, and a field change shows the working copy's value and what it would become. Having been changed outside is no reason to hold back here, so the tick for that does not appear. Nothing is written to the source.
- **Sync to another library**, *Copy from the other library into the open one* (`reverse` in `GetLibrarySyncDiff` and `SyncToLibrary`): the roles of `mirror` are swapped. The other library is only read (its databases are copied to the scratch folder and opened as a library there) and the thing that is changed is a copy of the open library's *working copy*, which then replaces the working copy (the old one is kept in `before-back-sync` in the cache folder and the working copy is noted as edited, so that "Sync with the source" writes it on). The options mean the same with the roles swapped: the stars, locations and origins of the other library's tracks are written to the open library's tracks (a location is written as the way from the changed library's `Engine Library` folder to the same file, since a path is relative to its own library: `../Engine-Tracks/…` on a stick is `../../stick/Engine-Tracks/…` in a library beside it; this goes for both directions), "copy tracks" copies the other library's files to `Engine-Tracks` beside the *open* library's `Engine Library` folder and adds tracks the open library lacks, and playlists only the open library has are deleted when asked.

After either, the backend has read the new working copy by itself (playback is not stopped) and the UI fetches the tracks and playlists again, keeping the list, the song and the scroll position it had, but not starting anything. Track ids of tracks that were there stay the same; added tracks come after them.

## Syncing to somewhere without a library

The sidebar button is just "Sync to…". Its list has the recent libraries and *every* connected drive (one with a library is chosen by the library, one without says that a new one is made there), and "Choose a folder…" opens a folder dialog for any folder. When the chosen place has no library (`create_if_missing` in `GetLibrarySyncDiffRequest` and `SyncToLibraryRequest`; the UI always sets it), the diff says `target_is_new` and the window says that a new, empty library is made at `<place>/Engine Library` when you sync (at the place itself if it is called `Engine Library`); the tick for copying tracks starts on, since a library without tracks is of little use. The diff makes nothing. The push makes the library:

- `backend/src/new_library.rs` creates the databases from the *schema of the open library's own databases* (every table, index, view and trigger of its `m.db`, `hm.db`, `sm.db` and `stm.db` that exist, from `sqlite_master`, and `user_version`), with no rows but an `Information` row that has a UUID of its own and the schema versions of the source. Engine's schema is not written down elsewhere in this project, so this is the only way to get one that matches. Whether Engine DJ and the players accept a library made this way, without the `Artwork`, `Stems`, `user.profile` and so on that Engine adds, has **not** been tried on real hardware.
- The sync then runs as for an existing library, on that new database: the playlists, the tracks (rows and, if ticked, files in `Engine-Tracks` beside the `Engine Library` folder) and the origins. All of the new databases are written to the drive, nothing is replaced, so there is no backup. If that fails, the folder that was made for it is taken away again, and so are the copied files.
- Not the other way round (there is nothing to copy from), and covers that are files in the open library's `Artwork` folder are not copied for added tracks.
