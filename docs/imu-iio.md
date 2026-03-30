# IMU / IIO Gyroscope Support

This document describes how InputPlumber handles IMU (Inertial Measurement Unit)
devices exposed via the Linux IIO (Industrial I/O) subsystem, including findings
from testing on devices using the Intel HID Sensor Hub.

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

### IIO Devices Created

```
iio:device0  name: gyro_3d           driver: hid_sensor_gyro_3d
iio:device2  name: accel_3d          driver: hid_sensor_accel_3d
iio:device3  name: gravity           (unused by InputPlumber)
iio:device4  name: relative_orientation  (unused by InputPlumber)
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
| MSI Claw 7 / Claw 8 A2VM missing IMU config | Open |
| `_available` attribute WARNs in log (harmless) | Cosmetic |
