# Mixxx mapping for the Denon DJ SC LIVE 4 (reference)

A copy of the two files of the Mixxx controller mapping for the SC LIVE 4. They are kept here
as a **reference for what the unit sends**, from which the built-in controller profiles
(`backend/src/controller/presets.rs`) were taken. Open Engine Lib does not run them.

| | |
|---|---|
| Source | [mixxxdj/mixxx#17201](https://github.com/mixxxdj/mixxx/pull/17201), "Denon SC Live 4: add controller mapping" |
| Branch | `denon-sc-live-4` of `artmechanix/mixxx` |
| Commit | `5d977dd6bc067df2a0d9388f57927a07490fe857` (the head of the pull request when copied, 2026-10-23) |
| State | **open, not merged** into Mixxx when copied: it may change, move or be rejected, so this copy is the one to go by here |
| Author | Jammes Luckett (pka Sister Transistor) // Art Mechanix; code adapted from the Denon Prime 4 mapping by Levi Williams (Whanake / whanake-music) and RattyDAVE |
| Tried by the author on | an SC LIVE 4 with Engine OS 5.0.4, in Mixxx 2.5.6 (macOS) and the 2.6 beta |
| Forum thread | <https://mixxx.discourse.group/t/denon-sc-live-4-mapping-tested-in-2-5-6-2-6-beta/34288> |
| Licence | GNU General Public License v2 or later, as the header of the script says (Open Engine Lib itself is AGPL-3.0, which GPL-2.0-or-later code may be combined with under GPLv3). The files are unchanged copies, with their headers. |

Files:

- `Denon-SC-Live-4.midi.xml`: the mapping's description for Mixxx (which control is a script
  binding, the options of the Preferences page). Most deck controls are not in it but built in
  the script.
- `Denon-SC-Live-4-scripts.js`: the script. The MIDI numbers are in it as `midi: [status, note]`.

## What was read from it

MIDI as the script gives it: the status byte is `0x90 + channel - 1` for a note (so `0x94` is
channel 5). Each side of the unit has a channel of its own, and both decks of a side (1 and 3,
2 and 4) share it, the unit switching the layer inside:

| Side | Decks | MIDI channel (1-based) | Status of a note on |
|---|---|---|---|
| left | 1, 3 | 5 | `0x94` |
| right | 2, 4 | 6 | `0x95` |
| mixer, library, FX | | 16 | `0x9F` / `0xBF` |

Notes of a side (`Deck` in the script), the ones Open Engine Lib uses marked ✔:

| Note | Control | Used for |
|---|---|---|
| `0x04` / `0x05` | track skip back / forward | ✔ previous / next |
| `0x06` / `0x07` | beat jump back / forward | ✔ seek back / forward |
| `0x09` | cue | ✔ stop |
| `0x0A` | play | ✔ play/pause |
| `0x0F` to `0x16` | the eight performance pads (`0x0E + i`, `i` = 1 to 8) | ✔ cue pads 1 to 8 |
| `0x01` | censor | |
| `0x17`, `0x18` | parameter < / > | |
| `0x1D`, `0x1E` | pitch bend buttons | |
| `0x1F` | deck layer toggle (1/3, 2/4; both layers share the channel) | |
| `0x20`, `0x27` | loop encoder (turned, pressed) | |
| `0x22`, `0x23`, `0x24` | key lock, vinyl mode, slip | |
| `0x25`, `0x26` | loop in / out | |

(Not a note: the tempo fader is control change `0x1F` on the same channel.) Other buttons, such
as sync, are in the script too; the table lists what was read, not everything.

The pads send the same notes in every pad mode (hot cue, loop, roll, slicer...); only what the
mapping does with them differs. Jog wheels, knobs and faders are control changes (`0xB4`,
`0xB5` for the decks, `0xB0` and up for the mixer channels) and are not used.

Not read from the script, and so **not verified** here: which of these the unit sends in the
mode Open Engine Lib is used with (the notes say controller mode), and what a press on cue,
the beat jump buttons or the track skip buttons sends with Shift held.
