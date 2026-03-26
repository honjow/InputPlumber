# OXP HID Button Mapping Protocol

Source: `GD32--CH32 Mapping analysis26.1.15.xlsx` (OneXPlayer internal documentation)

This document covers the button mapping protocol for OXP handheld devices using the
CH32 MCU (HID communication, VID: `0x1A86`, PID: `0xFE00`). A GD32 variant exists for
older devices using UART communication — only the CH32 (HID) protocol is relevant to
InputPlumber.

## Overview

The mapping system allows remapping any physical button to a different gamepad button,
keyboard key, macro, turbo function, or secondary (Fn) function. Configuration is sent
via the vendor HID interface (usage page `0xFF00`) using command ID `0xB4`.

## Data Frame Format

A complete mapping update requires **two packets** (64 bytes each). The MCU triggers
an update and save upon receiving the second packet.

### Packet Structure

```
[CID] [0x3F] [0x01] [Query/Modify] [DataLen] [Proto] [Page] [Pri/Sec] [Button Modules...] [0x3F] [CID]
 b1     b2     b3       b4            b5       b6      b7      b8        b9-b62              b63    b64
```

| Byte | Field | Description |
|------|-------|-------------|
| 1 | CID | Command ID: `0xB4` for button mapping |
| 2 | Frame header | Fixed: `0x3F` |
| 3 | Built-in receiver | Fixed: `0x01` |
| 4 | Query/Modify | `0x01` = Query, `0x02` = Modify |
| 5 | Data length | Fixed: `0x38` (56 decimal) |
| 6 | Protocol constant | Fixed: `0x02` |
| 7 | Page number | `0x01` = Page 1 (buttons 0x01-0x09), `0x02` = Page 2 (buttons 0x0A-0x23) |
| 8 | Primary/Secondary | `0x01` = Primary function, `0x02` = Secondary (Fn) function |
| 9-62 | Button modules | 9 buttons × 6 bytes = 54 bytes |
| 63 | Frame trailer | Fixed: `0x3F` |
| 64 | CID (repeat) | `0xB4` |

> **Tested deviation from OXP documentation (bytes 5-7):** The official OXP internal
> spreadsheet (`GD32--CH32 Mapping analysis26.1.15.xlsx`) marks bytes 5-7 as "reserved
> `0x00`". However, testing on X1 Mini (CH32 MCU) confirmed that `0x00 0x00 0x00` does
> **NOT** work — the MCU silently ignores the command. The correct values are
> `0x38 0x02 [page]`, derived from the HHD project. Each byte was individually tested:
>
> - **byte5 = `0x38`**: Required fixed constant. Any other value (0x00, 0x36, 0x3A, 0xFF)
>   causes the command to fail. Likely represents the data payload length (56 bytes).
> - **byte6 = `0x02`**: Required fixed constant. Any other value (0x00, 0x01, 0x03)
>   causes the command to fail. Not a "total page count" — setting it to `0x01` does not
>   allow single-packet updates.
> - **byte7 = page number**: The only variable field. MCU uses this to identify which
>   page's data is being sent. Duplicate page numbers (e.g., two page=1 packets) will
>   not trigger an update.

### Packet 1 (Page 1) — Buttons 0x01 to 0x09

Contains: A, B, X, Y, LB, RB, LT, RT, START

### Packet 2 (Page 2) — Buttons 0x0A to 0x23

Contains: BACK/SELECT, L3, R3, D-Up, D-Down, D-Left, D-Right, M1, M2

### Update Mechanism

> **Tested on X1 Mini (CH32 MCU):**
>
> - Both Page 1 and Page 2 must be sent to trigger an update. Sending only one page
>   caches its data but does not apply changes.
> - **Page 2 receipt triggers the update**: when Page 2 arrives and Page 1 data is in the
>   buffer, the MCU applies both pages together.
> - **Page 1 must be sent before Page 2**. Sending in reverse order (Page 2 first, then
>   Page 1) does not trigger the update correctly.
> - Sending duplicate page numbers (e.g., page=1 twice) does not trigger an update.
> - Invalid page numbers (e.g., page=3) are silently ignored.

### Query Command

To read current mappings, send a packet with byte 4 = `0x01` (query) and byte 8 =
`0x01` (primary) or `0x02` (secondary). Button module area is all zeros.

## Button Module Data (6 bytes per button)

```
[Physical Index] [Mode] [Mapping/KeyCode] [Param1] [Param2] [Param3]
    byte1         byte2     byte3           byte4    byte5    byte6
```

## Physical Button Index Table

| Index | Button | Index | Button |
|-------|--------|-------|--------|
| 0x01 | A | 0x0A | BACK/SELECT |
| 0x02 | B | 0x0B | L3 (Left Stick Click) |
| 0x03 | X | 0x0C | R3 (Right Stick Click) |
| 0x04 | Y | 0x0D | D-Pad Up |
| 0x05 | LB | 0x0E | D-Pad Down |
| 0x06 | RB | 0x0F | D-Pad Left |
| 0x07 | LT | 0x10 | D-Pad Right |
| 0x08 | RT | 0x22 | M1 (Custom Button 1) |
| 0x09 | START | 0x23 | M2 (Custom Button 2) |
| | | 0x24 | M3 (Custom Button 3) |
| | | 0x25 | M4 (Custom Button 4) |
| | | 0x26 | M5 (Custom Button 5) |
| | | 0x27 | M6 (Custom Button 6) |

### Virtual Button Index Table

In gamepad mode, virtual indexes include additional stick directions:

| Index | Virtual Button | Index | Virtual Button |
|-------|---------------|-------|---------------|
| 0x01 | A | 0x11 | Left Stick Up |
| 0x02 | B | 0x12 | Left Stick Up-Right |
| 0x03 | X | 0x13 | Left Stick Right |
| 0x04 | Y | 0x14 | Left Stick Down-Right |
| 0x05 | LB | 0x15 | Left Stick Down |
| 0x06 | RB | 0x16 | Left Stick Down-Left |
| 0x07 | LT (0-255) | 0x17 | Left Stick Left |
| 0x08 | RT (0-255) | 0x18 | Left Stick Up-Left |
| 0x09 | START | 0x19 | Right Stick Up |
| 0x0A | BACK | 0x1A | Right Stick Up-Right |
| 0x0B | L3 | 0x1B | Right Stick Right |
| 0x0C | R3 | 0x1C | Right Stick Down-Right |
| 0x0D | D-Pad Up | 0x1D | Right Stick Down |
| 0x0E | D-Pad Down | 0x1E | Right Stick Down-Left |
| 0x0F | D-Pad Left | 0x1F | Right Stick Left |
| 0x10 | D-Pad Right | 0x20 | Right Stick Up-Left |
| | | 0x22 | HOME (Xbox Button) |

## Mode Definitions

| Mode | Name | byte3 | byte4 | byte5 | byte6 |
|------|------|-------|-------|-------|-------|
| 0x01 | Gamepad | Virtual button index | Turbo: `0x00`=off, `0x01`=semi-auto, `0x02`=full-auto | Turbo press time (10-255 ms) | Turbo interval (10-255 ms) |
| 0x02 | Keyboard | Key code 1 | Key code 2 | Key code 3 | Key code 4 |
| 0x03 | Macro | Macro number | Trigger method | Loop toggle | Loop count |
| 0x04 | Turbo (Global) | Unused (`0x00`) | `0x00`=off, `0x01`=semi-auto, `0x02`=full-auto | Press time (10-255 ms) | Interval (10-255 ms) |
| 0x05 | Fn Key (Secondary) | Unused | Unused | Unused | Unused |

## Mode Details

### Mode 0x01 — Gamepad

Remaps a physical button to any virtual gamepad button. Optionally enables per-button
turbo (rapid fire).

**Examples:**

| Description | byte1 | byte2 | byte3 | byte4 | byte5 | byte6 |
|-------------|-------|-------|-------|-------|-------|-------|
| A → A (default) | 0x01 | 0x01 | 0x01 | 0x00 | 0x00 | 0x00 |
| A → B (remap) | 0x01 | 0x01 | 0x02 | 0x00 | 0x00 | 0x00 |
| B → X (remap) | 0x02 | 0x01 | 0x03 | 0x00 | 0x00 | 0x00 |
| B → B semi-auto turbo | 0x02 | 0x01 | 0x02 | 0x01 | 0x64 | 0x64 |
| B → X full-auto turbo | 0x02 | 0x01 | 0x03 | 0x02 | 0x64 | 0x64 |
| M1 → LT (default) | 0x22 | 0x01 | 0x07 | 0x00 | 0x00 | 0x00 |
| M2 → RT (default) | 0x23 | 0x01 | 0x08 | 0x00 | 0x00 | 0x00 |

- **Semi-auto turbo** (`0x01`): Press and hold to rapid-fire.
- **Full-auto turbo** (`0x02`): Single press triggers continuous rapid-fire until
  pressed again.

### Mode 0x02 — Keyboard

Maps a physical button to up to 4 simultaneous keyboard keys.

> **Key encoding note:** The OXP firmware uses a **proprietary key encoding**
> that differs from standard USB HID Usage Tables. For function keys, the
> formula is `F(n) = 0x59 + n` (e.g., F1=0x5A, F13=0x66, F14=0x67). This is
> confirmed by srsholmes's Apex firmware remap implementation. The OXP internal
> documentation does not explicitly document this encoding.

**Example:**

| Description | byte1 | byte2 | byte3 | byte4 | byte5 | byte6 |
|-------------|-------|-------|-------|-------|-------|-------|
| X → Keyboard 'A' (0x04) | 0x03 | 0x02 | 0x04 | 0x00 | 0x00 | 0x00 |
| M1 → F14 (OXP: 0x67) | 0x22 | 0x02 | 0x01 | 0x67 | 0x00 | 0x00 |
| M2 → F13 (OXP: 0x66) | 0x23 | 0x02 | 0x01 | 0x66 | 0x00 | 0x00 |

OXP proprietary key codes for function keys (`F(n) = 0x59 + n`):

| OXP Code | Key | OXP Code | Key | OXP Code | Key |
|----------|-----|----------|-----|----------|-----|
| 0x5A | F1 | 0x60 | F7 | 0x66 | F13 |
| 0x5B | F2 | 0x61 | F8 | 0x67 | F14 |
| 0x5C | F3 | 0x62 | F9 | 0x68 | F15 |
| 0x5D | F4 | 0x63 | F10 | 0x69 | F16 |
| 0x5E | F5 | 0x64 | F11 | | |
| 0x5F | F6 | 0x65 | F12 | | |

### Mode 0x03 — Macro

Triggers a recorded macro sequence. The macro system uses pass-through macros (MCU
flash is limited; macros are not saved on the MCU).

| Field | Description |
|-------|-------------|
| byte3 | Macro number (slot index) |
| byte4 | Trigger method |
| byte5 | Loop toggle |
| byte6 | Loop count |

### Mode 0x04 — Turbo (Global)

Sets a button as a global turbo modifier key. When pressed together with another
button, that button rapid-fires.

| Field | Description |
|-------|-------------|
| byte3 | Unused (`0x00`) |
| byte4 | `0x00` = off, `0x01` = semi-auto, `0x02` = full-auto |
| byte5 | Press time (10-255 ms) |
| byte6 | Interval (10-255 ms) |

**Warning**: Do not set multiple buttons as global turbo keys — this may cause
anomalies.

### Mode 0x05 — Fn Key (Secondary Function)

Sets a button as a secondary function modifier (like a keyboard Fn key). When held
with another button, that button's secondary function (configured separately) is
triggered.

Secondary functions support 3 types: Xbox button mapping, keyboard key, or macro.
The secondary function mappings are sent with byte 8 = `0x02` in the packet header.

**Warning**: Do not set multiple buttons as Fn keys. Avoid remapping BACK and START
as they are special function keys.

## Default Configuration

### Packet 1 (Primary Function)

```
B4 3F 01 02 38 02 01 01           ← byte5-7: 0x38 0x02 0x01(page 1)
01 01 01 00 00 00    A → A
02 01 02 00 00 00    B → B
03 01 03 00 00 00    X → X
04 01 04 00 00 00    Y → Y
05 01 05 00 00 00    LB → LB
06 01 06 00 00 00    RB → RB
07 01 07 00 00 00    LT → LT
08 01 08 00 00 00    RT → RT
09 01 09 00 00 00    START → START
3F B4
```

### Packet 2 (Primary Function)

```
B4 3F 01 02 38 02 02 01           ← byte5-7: 0x38 0x02 0x02(page 2)
0A 01 0A 00 00 00    BACK → BACK
0B 01 0B 00 00 00    L3 → L3
0C 01 0C 00 00 00    R3 → R3
0D 01 0D 00 00 00    D-Up → D-Up
0E 01 0E 00 00 00    D-Down → D-Down
0F 01 0F 00 00 00    D-Left → D-Left
10 01 10 00 00 00    D-Right → D-Right
22 01 07 00 00 00    M1 → LT (factory default)
23 01 08 00 00 00    M2 → RT (factory default)
3F B4
```

Note: Factory default maps M1 → LT and M2 → RT in gamepad mode (`0x01`). InputPlumber's
`INIT_CMD_2` changes M1/M2 to keyboard mode (`0x02`) with keycodes F14/F13
(`0x22 0x02 0x01 0x67 0x00 0x00` for M1→F14, `0x23 0x02 0x01 0x66 0x00 0x00` for M2→F13),
which disables the LT/RT mirroring on the Xbox gamepad while assigning unique keycodes.
The OXP key encoding for function keys uses the formula `F(n) = 0x59 + n`
(F13=0x66, F14=0x67), which differs from USB HID standard codes (F13=0x68, F14=0x69).

## GD32 vs CH32 Differences

The GD32 MCU variant (used in X1 Pro 370 and older devices) uses UART communication
instead of HID. The protocol is similar but uses different framing:

| Feature | GD32 (UART) | CH32 (HID) |
|---------|-------------|------------|
| Communication | Serial port (115200/8/2/Even/None) | USB HID (VID:1A86 PID:FE00) |
| Command ID | `0xF5` | `0xB4` |
| Frame header | `0xF5 0x3F` | `0xB4 0x3F 0x01` |
| Query command ID | `0xF4` | `0xB4` (byte4=0x01) |
| Button modules | Same 6-byte format | Same 6-byte format |
| Modes | Same (0x01-0x05) | Same (0x01-0x05) |

The button module data format (6 bytes per button) and mode definitions are identical
between GD32 and CH32 variants.

## InputPlumber Implementation Notes

Our `INIT_CMD_1` and `INIT_CMD_2` in `driver.rs` use the correct header format (derived
from the HHD project):

```
Working header:     B4 3F 01 02 38 20 [page] 01
OXP doc header:     B4 3F 01 02 00 00 00     01  ← DOES NOT WORK on X1 Mini
```

The OXP internal documentation (`GD32--CH32 Mapping analysis26.1.15.xlsx`) marks bytes
5-7 as "reserved `0x00`", but this is **incorrect** for the X1 Mini (CH32 MCU).
Individual byte testing on X1 Mini confirmed:

- `0x38 0x20 [page]` — works (current InputPlumber implementation)
- `0x38 0x02 [page]` — also works (used by HHD's hid_v1.py)
- `0x00 0x00 0x00` — silently ignored, no mapping update occurs

Byte 5 (`0x38`) is required and likely represents the data payload length (56 bytes).
Byte 6 accepts both `0x02` and `0x20`; the firmware readback on X1 Mini returns `0x20`,
which is what InputPlumber uses. The HHD project uses `0x02` and it also works.

The official document may describe the GD32 (UART) variant's protocol, or it may simply
be outdated. The CH32 (HID) firmware requires at minimum the `0x38` prefix on byte 5.
