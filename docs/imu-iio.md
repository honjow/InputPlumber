# IMU / IIO Gyroscope Support

This document describes how InputPlumber handles IMU (Inertial Measurement Unit)
devices exposed via the Linux IIO (Industrial I/O) subsystem, including findings
from testing on devices using the Intel HID Sensor Hub and AMD Sensor Fusion Hub.

## Driver Architecture

InputPlumber supports three IMU input paths:

| Path | Driver | Typical Device |
|------|--------|----------------|
| IIO – `BmiImu` | `src/input/source/iio/bmi_imu.rs` | ROG Ally (BMI323) |
| IIO – `AccelGyro3d` | `src/input/source/iio/accel_gyro_3d.rs` | Legion Go, MSI Claw |
| hidraw | `src/input/source/hidraw/legos_imu.rs` | Legion Go S |

The driver is selected in `src/input/source/iio.rs` by matching the IIO device
name:

```
bmi323-imu / bmi260 / i2c-BMI* / i2c-BOSC* / i2c-10EC5280*  →  BmiImu
gyro_3d / accel_3d                                            →  AccelGyro3d
```

### Data Flow

```
IIO sysfs (in_anglvel_x_raw × scale + offset)
  → iio_imu::driver::Driver::poll()
    → AccelGyro3dImu / BmiImu  (translate + mount matrix)
      → NativeEvent (Capability::Gyroscope / Accelerometer)
        → CompositeDevice
          → Target Device (deck-uhid / unified_gamepad)
            → Int16Vector3 output
```

### Scale factors applied in `accel_gyro_3d.rs`

Scale factors are derived from the Steam Deck UHID protocol constants
(`src/drivers/steam_deck/driver.rs`):

| Channel | Formula | Value | Notes |
|---------|---------|-------|-------|
| Accel | `1.0 / ACCEL_SCALE` | `÷ 0.0006125` ≈ `× 1632.65` | m/s² → UHID LSB |
| Gyro | `(180/π) / GYRO_SCALE` | `÷ 0.0625` after deg/s | rad/s → °/s → UHID LSB |

These factors are physically derived and apply universally to any device whose
IIO driver correctly exposes SI-unit values (m/s², rad/s). Hardware-specific
sensitivity differences are already absorbed by the per-device `scale` attribute
read from sysfs by `iio_imu/driver.rs`.

---

## Intel HID Sensor Hub (ISH)

Devices using Intel's Integrated Sensor Hub expose IMU data via HID reports
translated into IIO devices by the kernel's `hid_sensor_hub` driver. This is
different from a dedicated BMI chip wired to I2C/SPI.

### Affected Devices

- MSI Claw A1M (Intel Core Ultra, HID Sensor Hub `8087:0AC2`)
- Potentially other Intel-based handhelds

> For AMD-based devices (e.g. MSI Claw A8 BZ2EM), see the
> [AMD Sensor Fusion Hub](#amd-sensor-fusion-hub-sfh) section below.

### IIO Devices Created

The HID Sensor Hub creates 4 IIO devices. The device numbering is **stable
across suspend/resume cycles** but **may change across reboots**.

Observed mappings (examples — check actual device on each boot):

```
# After one boot:
iio:device0 = gyro_3d
iio:device1 = accel_3d
iio:device2 = relative_orientation
iio:device3 = gravity

# After a different boot:
iio:device0 = accel_3d
iio:device1 = gravity
iio:device2 = relative_orientation
iio:device3 = gyro_3d
```

> **Note**: Accelerometer and gyroscope are exposed as **separate IIO devices**
> (unlike Legion Go which combines both in one device). InputPlumber creates one
> `AccelGyro3dImu` driver instance per device; each instance only emits events
> for the sensor types it actually finds channels for.

Sysfs path example:
```
/sys/devices/pci0000:00/.../HID-SENSOR-200076.14.auto/iio:device1/
```

### Attribute Differences vs. BMI / Legion Go

| Attribute | BMI / Legion Go | HID Sensor Hub |
|-----------|----------------|----------------|
| `in_anglvel_x_scale` | ✅ per-channel | ❌ missing |
| `in_anglvel_scale` | — | ✅ global only |
| `in_anglvel_x_scale_available` | ✅ | ❌ missing |
| `in_anglvel_sampling_frequency_available` | ✅ | ❌ missing |
| `in_anglvel_sampling_frequency` | per-channel | global only |

The missing `_available` attributes cause `WARN` messages in InputPlumber logs
at startup, but **do not prevent data from being read**. The global scale and
raw values are still accessible and libiio reads them correctly.

### Sampling Rate

The HID Sensor Hub **defaults to 10 Hz** on startup, which is insufficient for
smooth gyro-based gaming input.

**Tested maximum rates (MSI Claw A1M):**

| Sensor | Max Achievable | Notes |
|--------|---------------|-------|
| `gyro_3d` | **100 Hz** | Writing values > 100 is clamped to 100 |
| `accel_3d` | **500 Hz** | Writing 400 rounds up to 500 |

Data validity confirmed: raw values change each poll at 100 Hz and respond
correctly to physical motion.

#### How InputPlumber sets the sampling rate

`src/drivers/iio_imu/driver.rs` applies the following priority at init:

1. **YAML `sample_rate`** — explicit value from the device config (highest
   priority)
2. **Hardware `_available` list** — if the hardware exposes a
   `sampling_frequency_available` attribute, the maximum value is used
3. **Built-in default (200 Hz)** — used when neither source is available;
   hardware will silently clamp to its own maximum (e.g. 100 Hz for the Claw
   gyro)

The rate is written with a two-step fallback:
- Per-channel attribute (`in_anglvel_x_sampling_frequency`) — BMI / Legion Go
- Device-level global attribute (`in_anglvel_sampling_frequency`) — HID Sensor
  Hub

To pin a specific rate in the YAML config:

```yaml
- group: imu
  iio:
    name: gyro_3d
    sample_rate: 100
    mount_matrix:
      x: [0, 1, 0]
      y: [-1, 0, 0]
      z: [0, 0, -1]
```

---

## Device Configuration (YAML)

IMU source devices are declared under `source_devices` with `group: imu`:

```yaml
# IIO IMU with mount matrix
- group: imu
  iio:
    name: gyro_3d
    mount_matrix:
      x: [0,  1, 0]
      y: [-1, 0, 0]
      z: [0,  0, -1]
```

### MSI Claw A1M (`50-msi_claw_a1m.yaml`)

Mount matrix verified on hardware: standard identity matrix, no explicit
configuration needed. The driver defaults to identity when no `mount_matrix`
is specified.

```yaml
- group: imu
  iio:
    name: accel_3d
- group: imu
  iio:
    name: gyro_3d
```

### MSI Claw A8 BZ2EM (`50-msi_claw_a8_bz2e.yaml`)

Uses AMD SFH (Sensor Fusion Hub). Mount matrix verified: identity (no explicit
configuration needed).

```yaml
- group: imu
  iio:
    name: accel_3d
- group: imu
  iio:
    name: gyro_3d
```

> **Kernel patch required** — see [AMD SFH Precision Fix](#amd-sfh-precision-fix-kernel-patch-required).

### MSI Claw 7 / Claw 8 A2VM

These models currently have **no IMU configuration**. IMU support needs to be
added once the mount matrix is verified.

---

## Known Issues / TODO

| Issue | Status |
|-------|--------|
| Startup sampling rate is 10 Hz for HID Sensor Hub devices | Fixed |
| `iio_imu/driver.rs` does not write sampling frequency at init | Fixed |
| `AccelGyro3dImu` emits zero values when accel/gyro are separate IIO devices | Fixed |
| Incorrect scaling factors in `AccelGyro3dImu` | Fixed |
| MSI Claw A1M mount matrix needs physical verification | Fixed – identity matrix |
| MSI Claw A8 BZ2EM IMU support | Fixed — kernel patch required for precision |
| MSI Claw 7 / Claw 8 A2VM missing IMU config | Open |
| `_available` attribute WARNs in log (harmless) | Cosmetic |
| **Suspend/resume: raw attr reads block after wake** | **Fixed — logind signal + poll backoff fallback** |
| **gyro_3d stalls at non-100 Hz sampling rates** | **Hardware limitation — do not change** |

### Suspend/Resume Problem (Fixed)

**Confirmed facts (tested 2026-03-31 on MSI Claw A1M):**

1. **Device numbering is stable across suspend/resume** — only changes on
   reboot.
2. **Without InputPlumber running**: manually setting `sampling_frequency` and
   reading `in_anglvel_x_raw` via `cat` works normally before AND after
   suspend/resume. Reads return in <10 ms. No blocking, no stale data.
3. **With InputPlumber running**: after suspend/resume, `raw` attr reads become
   extremely slow. Measured timings:
   - `gyro_3d` channels: **2500–5000 ms per read** (sometimes times out entirely)
   - `accel_3d` channels: **73–199 ms per read** (10–20× slower than normal)
   Restarting the service alone does not fix it; stopping the service, waiting
   a few seconds, then starting again does.
4. **Changing `sampling_frequency` while IPB is polling**: causes reads to freeze
   immediately. Changing back to the original value un-freezes them.

**What this tells us:**

- The problem is **IPB's continuous rapid polling competing with the kernel's
  HID Sensor Hub for the same HID transport** during and after resume.
- Each `raw` sysfs read triggers a synchronous HID Get-Feature transaction.
  IPB polls 3 gyro + 3 accel channels in a tight loop, so 6 HID transactions
  run back-to-back. After resume, the sensor hub needs time to re-initialise
  its power state; IPB's rapid polling during this window floods the HID
  transport and prevents the hub from completing its resume sequence.
- Simply not running InputPlumber avoids the problem entirely, which means the
  kernel's own resume path works fine — InputPlumber's polling interferes with
  it somehow.
- `gyro_3d` is affected more than `accel_3d`, possibly because they share a
  single HID Sensor Hub and the gyro driver's resume takes longer.

**Solution (two layers):**

**Primary — logind PrepareForSleep signal:**
The Manager subscribes to `org.freedesktop.login1.Manager.PrepareForSleep` on
the system D-Bus. When the signal fires (`true` = going to sleep), the Manager
sends `SystemSleep` through the existing command chain:
`Manager → CompositeDevice → SourceDevices (suspend)`.
Each SourceDriver sets `is_suspended = true` and skips `poll()` entirely.
When the signal fires again (`false` = waking up), `SystemWake` flows through
the same chain and SourceDrivers resume polling. This is self-contained — no
external systemd service required.

The existing `inputplumber-suspend.service` (calling `HookSleep`/`HookWake`
via busctl) is still supported as a compatible fallback for systems where the
D-Bus signal path doesn't work. Both paths produce the same `SystemSleep` /
`SystemWake` commands internally.

**Fallback — poll timing backoff (driver.rs):**
If the logind signal is missed or delayed, `Driver::poll()` measures the
wall-clock time of each poll cycle. If it exceeds `POLL_SLOW_THRESHOLD`
(1 s), it sleeps for `POLL_BACKOFF_SLEEP` (3 s) to let the hub finish its
resume initialisation.

Tested values for the fallback:
- 1 s backoff: not enough, hub stays slow
- 3 s backoff: works reliably
- Probe-based recovery (read every 500 ms until fast): **fails** — any read
  during the resume window keeps the hub in a degraded state indefinitely

**Failed approaches (do not repeat):**

- IIO triggered buffer mode (`IioRawBuffer` with `/dev/iio:deviceX`): buffer
  data flow stops after resume and cannot be reliably re-established. Repeated
  `buffer/enable` 0→1 toggling damages the HID Sensor Hub state, requiring a
  full system reboot to recover. **Reverted — do not re-attempt without kernel
  driver changes.**

### gyro_3d Non-100 Hz Sampling Rate Stall (Hardware Limitation)

**Tested 2026-03-31 on MSI Claw A1M — no IPB involved (pure Python repro).**

The HID Sensor Hub's `gyro_3d` only works reliably at 100 Hz. Setting any
other sampling frequency (e.g. 90 → actual 90.9, 80 → 83.3, 50) causes raw
attribute reads to return a **frozen value** when read in rapid succession.

| Sampling rate | Rapid reads (no delay) | 200 ms interval |
|---------------|----------------------|-----------------|
| 100 Hz | Normal (unique values) | Normal |
| 90.9 Hz | **Frozen** | Normal |
| 50 Hz | **Frozen** | Normal |

At 50 Hz the minimum usable read interval is ~200 ms (i.e. max ~5 reads/sec),
which is far too slow for IMU use. This is a HID Sensor Hub hardware/driver
limitation, not an InputPlumber bug — the same behaviour reproduces with a
trivial Python script and no InputPlumber running.

`accel_3d` is not affected; it supports 500 Hz and works fine with rapid reads
at any configured rate.

**Recommendation:** Do not set `sample_rate` for `gyro_3d` on MSI Claw devices.
The default init path writes 200 Hz which the hardware clamps to 100 Hz — this
is the only rate that works correctly with rapid polling.

---

## AMD Sensor Fusion Hub (SFH)

AMD-based handhelds (e.g. MSI Claw A8 BZ2EM, Lenovo Legion Go) use the AMD
Sensor Fusion Hub (`amd_sfh` kernel module) instead of Intel ISH. The SFH
connects via PCIe and exposes the same `accel_3d` / `gyro_3d` IIO device names,
so InputPlumber's `AccelGyro3d` driver handles them identically.

### Differences from Intel ISH

| Aspect | Intel ISH | AMD SFH |
|--------|-----------|---------|
| Bus | HID over USB/I2C | HID over PCIe |
| Default sampling rate | 10 Hz | 0 Hz (must be set explicitly) |
| `_available` sysfs attrs | Missing | Missing |
| Suspend/resume sensitivity | High (see above) | TBD |

### AMD SFH Precision Fix (Kernel Patch Required)

**Tested 2026-03-31 on MSI Claw A8 BZ2EM.**

The stock `amd_sfh` kernel driver (`sfh1_1/amd_sfh_desc.c`) contains integer
division errors when converting firmware float values to HID report integers:

| Sensor | Stock divisor | Correct divisor | Precision loss |
|--------|--------------|----------------|---------------|
| Accelerometer | `/100` | `/10` | 10× (z-axis reads −0.98 instead of −9.81 m/s²) |
| Gyroscope | `/1000` | `/10` | 100× (dynamic range ±1.43° instead of ±143°/s) |

The HID report descriptor uses Unit Exponent −2 (0.01 units), so the firmware
values (already in appropriate scale) only need `/10` to produce correct HID
report values. The stock `/100` and `/1000` divisors discard too much precision
via integer truncation.

**Fix:** In `drivers/hid/amd-sfh-hid/sfh1_1/amd_sfh_desc.c`, change:

```c
// Accelerometer: /100 → /10
acc_input.in_accel_x_value = amd_sfh_float_to_int(accel_data.acceldata.x) / 10;
acc_input.in_accel_y_value = amd_sfh_float_to_int(accel_data.acceldata.y) / 10;
acc_input.in_accel_z_value = amd_sfh_float_to_int(accel_data.acceldata.z) / 10;

// Gyroscope: /1000 → /10
gyro_input.in_angel_x_value = amd_sfh_float_to_int(gyro_data.gyrodata.x) / 10;
gyro_input.in_angel_y_value = amd_sfh_float_to_int(gyro_data.gyrodata.y) / 10;
gyro_input.in_angel_z_value = amd_sfh_float_to_int(gyro_data.gyrodata.z) / 10;
```

This fix also applies to the Lenovo Legion Go (same AMD SFH driver).
