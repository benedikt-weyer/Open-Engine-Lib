# SoundSwitch project format

Findings from inspecting the `SoundSwitch` folder of a real drive that is used with a Denon SC LIVE 4 and with SoundSwitch on a laptop. SoundSwitch is the lighting software that runs DMX lights to the music; Engine DJ players run it too (the crash dump found next to the Engine library came from the `SoundSwitch` process on the player).

SoundSwitch's file formats are not documented. **Everything here was worked out from the files**, and the document says for each point whether it was **observed** (checked on the files), **inferred** (a reading that fits what was observed but could not be confirmed) or **unknown**. Nothing here comes from SoundSwitch's documentation or source.

The one thing Open Engine Lib needs from it is whether a track has lighting. How it uses what is below is in [Use in Open Engine Lib](#use-in-open-engine-lib).

## Verdict: partly readable, the lighting itself is not

| Part | What it is | Readable? |
| --- | --- | --- |
| `.ssproj` file inside a project folder | Plain JSON (project id, version, a few settings) | Yes |
| `SoundSwitchTrackMap.bin` | Binary container; the tracks' paths and titles are UTF-16 text, each analysed track has a GUID | The paths, titles and GUIDs, yes. The bytes between them are not decoded |
| `{GUID}.ssfile` | Structured binary, one per analysed track (entropy about 4 bits per byte, so not compressed) | Header only. Contents not decoded |
| `{GUID}.ssa` | Small, high-entropy binary (about 8 bits per byte), one per track that has lighting | **No.** Looks compressed or encrypted. Contents not read |
| `SSAutoLoopN.ssfile` | The same container as a track's `.ssfile`, for the built-in "autoloops" (128 per project) | Header only |
| `SoundSwitchVenues.bin` | Binary container holding the venues, fixtures and scenes, with their names in UTF-16 | The names, yes. The rest not decoded |
| `SoundSwitchAutoLoops.bin`, `...Ex.bin` | The autoloop banks and their names | The names, yes |
| `recordable/*.dat` | Starts with `0xDEADBEEF`; lists control names as ASCII (`SoundSwitch.Controls.AutoLoopsPlayAutoloop3`) | The names, yes. Probably the assignment of controls |
| `*.plfix` | Fixture profiles: a long line of hex digits that are binary data with high entropy (7.76 bits per byte), a multiple of 16 bytes long | **No.** Looks encrypted |

No keys, DRM or anything else that stops reading was needed for the parts a library browser uses. What is not readable (`.ssa`, `.plfix`) was left alone; no attempt was made to break it.

## Directory layout

```
<drive>/
├── Engine Library/                    the Engine DJ library, see engine-library-format.md
├── SoundSwitch/
│   ├── <name>.ssproj/                 one folder per project (a project holds one venue setup)
│   │   ├── .ssproj                    JSON
│   │   ├── SoundSwitchTrackMap.bin    every track SoundSwitch knows, by path
│   │   ├── {GUID}.ssfile              per analysed track
│   │   ├── {GUID}.ssa                 per track with lighting (see below)
│   │   ├── SSAutoLoop1.ssfile ...     128 built-in autoloops
│   │   ├── SoundSwitchVenues.bin      venues, fixtures, scenes
│   │   ├── SoundSwitchAutoLoops.bin   autoloop banks
│   │   ├── SoundSwitchAutoLoopsEx.bin more autoloops
│   │   └── recordable/*.dat           control assignments
│   └── *.plfix                        fixture profiles
└── <music folders>/
```

The drive had two projects. Both have the same project id in their `.ssproj`:

- `default.ssproj`: `"readOnly": true`, version 2.11, 463 analysed tracks, **no `.ssa` files**. This is the one the player was using.
- `Blank Venue export from laptop.ssproj`: no `readOnly` flag, version 2.10 (hotfix 2), 381 analysed tracks, 374 `.ssa` files.

The `.ssproj` file is JSON: `id` (a GUID in braces), `readOnly` (only in some), `spectra` (window settings of SoundSwitch's visualiser) and `version` (`major`, `minor`, `hotfix`, `build`).

## The container

The `.bin` files and every `.ssfile` start with the same eight bytes (**observed**):

| Offset | Bytes | Meaning |
| --- | --- | --- |
| 0 | `AA AA 09 55` | Magic |
| 4 | `03 00 00 00` | Little-endian `u32`, 3: the container version (presumably) |
| 8 | two `u32` | In `.ssfile` and `SoundSwitchVenues.bin` these are close to the file size minus a small number (for one `.ssfile` of 55,550 bytes the first is 55,425). They look like lengths. They were not decoded, and in the track map and autoloop files they are 0 |

Numbers are little-endian. Text is **UTF-16LE with a length prefix**: a `u32` with the number of 16-bit units *including a terminating zero*, then the units, ending in `00 00` (**observed** throughout the track map). The files are written by a Windows program, and paths inside them are Windows paths.

None of the 374 `.ssa` files has this header (their first bytes look random, and their entropy is 7.93 to 7.99 bits per byte), which is part of why they are taken to be compressed or encrypted rather than another structured container.

## The track map

`SoundSwitchTrackMap.bin` (about 1 to 2 MB) holds a record for every track the project has seen, not only the analysed ones (one project holds 3,436 distinct file paths, the other 6,996 path strings: the same library had been imported with different drive letters, so a track can appear once per spelling of its path).

**Observed** layout of a record (the part that matters):

```
04 00 00 00 01          marker: a length of 4 and a flag byte, then
<16 bytes>              a GUID, "GUID 1" (not needed; what it names was not found)
01 00 00 00             then, each preceded by 01 00 00 00 and a length:
  <title>               UTF-16LE string
  <artist>              UTF-16LE string
  <path>                UTF-16LE string: the audio file as Windows sees it
<29 to 45 bytes>        not decoded (contains numbers, some of them doubles)
04 00 00 00 01          the same marker again, and
<16 bytes>              "GUID 2", only for a track that was analysed
01 00 00 00
```

- A path looks like `F:\Music\Deep House\Track.flac` or, when the library had been imported through the Engine library folder, `D:\Engine Library\..\Music\Deep House\Track.flac`. The drive letter is the laptop's and is meaningless on another machine.
- **GUID 2 is the important one.** It is stored the Windows way: the first three fields are little-endian, the last eight bytes as they are. Written as text in upper case, in braces, it is the name of the track's data in the same folder: `{GUID}.ssfile` exists for it (checked: all 463 `.ssfile` of the player's project are named in its track map. In the laptop project 315 of its 381 `.ssfile`, and 314 of its 374 `.ssa`, are named in its track map; the others are not referenced from it, and why was not looked into).
- A track that was not analysed has no GUID 2. The next marker that follows its path (within the 128 bytes that are searched) is then the first GUID of the *next* record, so a GUID must be checked against the files that exist: only GUID 2 names a file, GUID 1 never did.
- The first record of every track map is a demo track of SoundSwitch itself (title "SoundSwitch 2.0 In-App Demo", a path in a Windows temp folder). In the laptop project it has data like any other track; in the player's project it has none.

How to read it (what Open Engine Lib does): look for a drive letter, `:` and `\` in UTF-16 to find a path, read to the 16-bit zero, then look in the next 128 bytes for `04 00 00 00 01` and take the 16 bytes after it as a GUID. Keep it if `{GUID}.ssfile` exists in the project.

## Tracks, analysis and lighting

For a track in the map, in a project folder:

| Files | What it means | Status |
| --- | --- | --- |
| nothing | SoundSwitch knows the track's path but has no data | **observed** |
| `{GUID}.ssfile` | The track was analysed | **observed** that the GUID after a track's path names an existing `.ssfile`, for every track that was checked. That it is the *analysis* is inferred from its structure and size |
| `{GUID}.ssfile` and `{GUID}.ssa` | Lighting has been rendered for the track | **inferred**, see below |

On the drive: the laptop project has 381 `.ssfile` named by a GUID and 374 `.ssa` (every `.ssa` has an `.ssfile` of the same name, the other 7 `.ssfile` have no `.ssa`; the 128 `SSAutoLoopN.ssfile` are not tracks). The player's project has 463 `.ssfile` and no `.ssa` at all.

### Why `.ssa` is taken to be rendered lighting

SoundSwitch's feature for making a light show from a track is called an autoscript, and `.ssa` fits as its abbreviation. What supports the reading, and what does not:

- **For:** an `.ssa` only exists next to the `.ssfile` of the same GUID, never alone (374 of 374), and it was written **within two seconds** of that `.ssfile` (374 of 374), as if both were saved together when the lighting for the track was made. A project whose tracks were merely analysed has none. Some of the fixture files (`.plfix`) date from the same day as the first `.ssa` files, as if the lights were being set up then.
- **Weakly for:** both files grow with the length of the track, but only loosely (correlation of about 0.3 with the length in seconds, over 283 tracks; for the shortest tracks an `.ssfile` is about 39 KB and an `.ssa` 6 to 8 KB, for the longest about 115 KB and 11 to 12 KB). This neither confirms nor contradicts the reading.
- **Against:** nothing found. But the content of `.ssa` was never read, so it is not known to *be* lighting. If SoundSwitch's own tools say otherwise, the reading is wrong.

## `.plfix` fixture profiles

A fixture profile describes the channels of a light. The file is a single line of lowercase hex digits (no line break). Decoded, it is 4,432 bytes: a multiple of 16, with 7.76 bits of entropy per byte and 277 blocks of 16 bytes of which 255 are different. Two versions of one profile start with the same 127 blocks (the next pair with 83), which is what a block cipher with a fixed key and no chaining across blocks, or a fixed starting value, does with a common beginning in the plain data. It was not tried to find out which cipher, and nothing was decrypted.

## Other files, in short

- **`SoundSwitchVenues.bin`** (about 300 KB): 79 readable UTF-16 strings such as fixture models, `Blank Venue`, and scenes (`All Green`, `All White Strobe`, `Custom Cue 4`). The venue, fixtures and scenes live here; the structure around the names was not decoded. A `.backup` of it sits next to it in one project.
- **`SoundSwitchAutoLoops.bin` and `...Ex.bin`** (2 KB and 7 KB): the autoloop banks (`Bank 1 : Dynamic`, `Bank 2 : Upbeat`, `Bank 3 : Smooth`, `Bank 4 : Random`) and their loops (`Dynamic Autoloop 1` and so on). The loops themselves are the `SSAutoLoopN.ssfile` files.
- **`recordable/*.dat`**: four small files that start with `EF BE AD DE` (little-endian `0xDEADBEEF`), then a few bytes that were not decoded, then lists of names like `SoundSwitch.Controls.ColourOverride0` and `SoundSwitch.Controls.AutoLoopsPlayAutoloop12`. They look like the assignments of controls (MIDI or similar) to actions.

## Use in Open Engine Lib

Open Engine Lib shows a **Lighting** column in the track list (`backend/src/soundswitch.rs`):

- It reads every `<drive>/SoundSwitch/*.ssproj` folder, where the drive is the folder that holds the `Engine Library` folder.
- From each track map it takes the paths with their GUID 2, and keeps those whose `.ssfile` exists.
- A track has *lighting rendered* if any project has a `.ssa` for it, *analysed only* if there is an `.ssfile` but never an `.ssa`, and otherwise it is not in SoundSwitch. The tooltip names the projects with lighting.
- Tracks are matched by the path of the audio file relative to the drive: the drive letter is dropped, `\` is read as `/`, `.` and `..` are worked out (so `D:\Engine Library\..\Music\a.flac` and `F:\Music\a.flac` are the same file), and case is ignored, as Windows paths are.
- Nothing is ever written to the SoundSwitch folder.

On the drive this was developed on, 283 of 3,123 local tracks matched as rendered (all from the laptop project), 229 as analysed only, and 2,611 were not in SoundSwitch. Of the 314 `.ssa` named in the laptop project's track map, 283 matched a track of the Engine library. The others, and the 60 `.ssa` that its track map does not name at all, matched nothing; why was not looked into.

## Gotchas for an implementation

- **Do not trust the meaning of `.ssa` more than this document does.** It is an inference.
- **Do not write to a project.** The format is undocumented, SoundSwitch may have it open, and a project can be marked read-only.
- **Read GUIDs with Windows byte order.** Read as plain big-endian bytes they name no file.
- **Check a GUID against the files** (see the track map section), or the next track's first GUID can be taken for this one's.
- **Paths are Windows paths.** Drive letters mean nothing, a path can pass through `Engine Library\..`, and case can differ from the file system's. The same track can appear several times in one map with different spellings.
- **A library on another drive or in another place** has no `SoundSwitch` folder next to it, so nothing is found; that is not an error.
- **Read it when the library is opened.** A project that SoundSwitch changes later is not noticed until the next open.
- **Decode no more than needed.** Only the strings and the marker are understood; the other bytes of a record and the contents of the `.ssfile` are not.

## Not known

- What GUID 1 of a record names.
- The meaning of the numbers between a record's path and its GUID 2 and of the two length-like numbers in the header.
- The contents of `.ssfile` (it is structured, around 55 KB for a four-minute track, with the word `Default` readable in it) and of `.ssa`.
- Whether `.ssa` is the lighting. If a way to open it is found, this should be checked.
- Which project the player actually plays lighting from (the one that is `readOnly`, which has no `.ssa`, is the likely candidate).

## Method

The files were copied to a scratch folder and examined with short Python scripts (byte patterns, UTF-16 strings, entropy, file sizes and times). Track map records were found by their UTF-16 paths and the marker, and every GUID found was checked against the file names in the project folder. The matching to the Engine library was counted by running the backend's SoundSwitch reader against the real drive. The drive was only read.
