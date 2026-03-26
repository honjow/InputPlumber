# OXP HID Vibration Protocol (CID 0xB3)

Reverse-engineered from Windows USB packet captures on X1 Mini (CH32 MCU,
VID: `0x1A86`, PID: `0xFE00`).

## Overview

The OXP vendor HID interface supports vibration control via command ID `0xB3`.
Unlike the button mapping protocol (CID `0xB4`), vibration commands use a
**read-write paired** protocol: each vibration update requires reading the current
state first, then writing the new state with both current and previous values.

## Data Frame Format

Vibration commands use the same `0x3F` framing as button mapping (`0xB4`),
with CID `0xB3` and a two-page structure.

### Packet Structure

```
[CID] [0x3F] [0x01] [Page] [...data...] [0x3F] [CID]
 0xB3                                             0xB3
```

Each vibration update requires **4 packets** in sequence:

| Step | Page | byte5 | Purpose |
|------|------|-------|---------|
| 1 | Page 1 | `0x02` | Read current state (query) |
| 2 | Page 1 | `0x20` | Write state (with current values) |
| 3 | Page 2 | `0x02` | Read current state (query) |
| 4 | Page 2 | `0x20` | Write new vibration values |

- `byte5 = 0x02`: Query/read operation
- `byte5 = 0x20`: Modify/write operation

## Page 1 — State Readback

Page 1 packets carry the current vibration state for verification.

### Page 1 Read (step 1)

```
B3 3F 01 01 00 02 00 00 ... 00 00 00 00 3F B3
                ^  ^
              page byte5=read
```

All data bytes are zero — this is a pure read request.

### Page 1 Write (step 2)

```
B3 3F 01 01 00 20 E3 39 E3 39 E3 39 E3 39 E3 [mode] [left] [right] 00 05 ...
              ^                                 b15    b16    b17
            byte5=write
```

| Byte | Field | Description |
|------|-------|-------------|
| 6-14 | Constants | `E3 39 E3 39 E3 39 E3 39 E3` (fixed pattern) |
| 15 | Mode | Current mode: `0x01`=active, `0x02`=off |
| 16 | Left motor | Current left motor intensity (0-5) |
| 17 | Right motor | Current right motor intensity (0-5) |

## Page 2 — Vibration Control

Page 2 carries the actual vibration command with both new and previous values.

### Page 2 Read (step 3)

```
B3 3F 01 02 38 02 E3 39 E3 39 E3 39 [new_mode] [new_L] [new_R] [prev_mode] [prev_L] [prev_R] 00 05 ...
              ^                        b12        b13     b14      b15        b16       b17
            byte5=read
```

### Page 2 Write (step 4)

```
B3 3F 01 02 38 20 E3 39 E3 39 E3 39 [new_mode] [new_L] [new_R] [prev_mode] [prev_L] [prev_R] 00 05 ...
              ^                        b12        b13     b14      b15        b16       b17
            byte5=write
```

| Byte | Field | Description |
|------|-------|-------------|
| 6-11 | Constants | `E3 39 E3 39 E3 39` (fixed pattern) |
| 12 | New mode | `0x01`=active (non-zero intensity), `0x02`=off (zero intensity) |
| 13 | New left | New left motor intensity (0-5) |
| 14 | New right | New right motor intensity (0-5) |
| 15 | Previous mode | Previous mode value |
| 16 | Previous left | Previous left motor intensity |
| 17 | Previous right | Previous right motor intensity |
| 19 | Unknown | Always `0x05` |

## Intensity Scale

Motor intensity uses a 0-5 scale:

| Value | Intensity |
|-------|-----------|
| 0 | Off |
| 1 | Minimum |
| 2 | Low |
| 3 | Medium |
| 4 | High |
| 5 | Maximum |

## Observed Behavior

On Windows, the driver vibrates the controller once during initialization,
likely to confirm the vibration hardware is functional.

### Captured Data (Manual Sweep)

The following data was captured by manually adjusting vibration intensity
from 0 to 5 and back to 0 in Windows, to generate sufficient packets for
protocol analysis:

```
Step  Mode  Left  Right  (Previous)
 0    ON     1     1     ← from OFF 0,0
 1    ON     2     2     ← from ON  1,1
 2    ON     3     3     ← from ON  2,2
 3    ON     4     4     ← from ON  3,3
 4    ON     5     5     ← from ON  4,4  (peak)
 5    ON     4     4     ← from ON  5,5
 6    ON     3     3     ← from ON  4,4
 7    ON     2     2     ← from ON  3,3
 8    ON     1     1     ← from ON  2,2
 9    OFF    0     0     ← from ON  1,1
```

Each step requires a full 4-packet read-write cycle (page 1 read → page 1
write → page 2 read → page 2 write), totaling **40 packets** for the 10-step
sweep.

## Protocol Comparison with B4 (Button Mapping)

| Feature | B4 (Mapping) | B3 (Vibration) |
|---------|-------------|----------------|
| CID | `0xB4` | `0xB3` |
| Framing | `0x3F` | `0x3F` |
| Pages | 2 (write only) | 2 (read-write paired) |
| Packets per update | 2 | 4 |
| Page 2 byte4-5 | `0x38 0x20` | `0x38 0x02`/`0x38 0x20` |
| State tracking | No | Yes (carries previous values) |
| Persistence | Firmware flash | Session only |

## Implementation Status

Vendor vibration (B3) is **not implemented** in InputPlumber. The Xbox gamepad's
native rumble works independently through the standard XInput path. The B3
protocol was previously attempted but caused the Xbox gamepad to go silent in
non-intercept mode, so it was removed.

The read-write paired protocol (requiring state tracking and 4 packets per
update) is significantly more complex than B4's fire-and-forget approach.
A correct implementation would need to:

1. Track previous motor values
2. Perform page 1 read → page 1 write → page 2 read → page 2 write for each update
3. Possibly run the Windows-style calibration sweep on initialization
