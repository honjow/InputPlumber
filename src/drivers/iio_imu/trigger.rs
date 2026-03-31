use industrial_io::{Context, Device};

/// Attempt to find a suitable trigger for the given IIO device.
///
/// Search order:
/// 1. hrtimer triggers (preferred for precise timing)
/// 2. Device-matched data-ready trigger (e.g. "gyro_3d-dev3" for "gyro_3d")
/// 3. Any other available trigger as fallback
///
/// `device_name` is the IIO device name (e.g. "accel_3d", "gyro_3d",
/// "i2c-BMI0160:00") used to match the corresponding data-ready trigger.
///
/// The trigger's sampling_frequency is set to `sample_rate` if possible.
/// Data-ready triggers don't have this attribute and are used as-is — their
/// rate follows the device's own sampling_frequency.
pub fn find_trigger(ctx: &Context, device_name: &str, sample_rate: f64) -> Option<Device> {
    let num_devices = ctx.num_devices();
    let mut matched_trigger: Option<Device> = None;
    let mut fallback_trigger: Option<Device> = None;

    for i in 0..num_devices {
        let Ok(dev) = ctx.get_device(i) else {
            continue;
        };

        if !dev.is_trigger() {
            continue;
        }

        let name = dev.name().unwrap_or_default();
        log::debug!("Found IIO trigger device: {name}");

        // Prefer hrtimer triggers for precise timing
        if name.contains("hrtimer") {
            try_set_trigger_rate(&dev, &name, sample_rate);
            log::info!("Selected hrtimer trigger: {name}");
            return Some(dev);
        }

        // Prefer the trigger that matches the device name
        // e.g. "gyro_3d-dev3" matches device "gyro_3d"
        if matched_trigger.is_none() && name.starts_with(device_name) {
            matched_trigger = Some(dev);
            continue;
        }

        if fallback_trigger.is_none() {
            fallback_trigger = Some(dev);
        }
    }

    let selected = matched_trigger.or(fallback_trigger);
    if let Some(ref trig) = selected {
        let name = trig.name().unwrap_or_default();
        try_set_trigger_rate(trig, &name, sample_rate);
        if name.starts_with(device_name) {
            log::info!("Selected device-matched trigger: {name}");
        } else {
            log::info!("Selected fallback trigger: {name} (no match for {device_name})");
        }
    }

    selected
}

fn try_set_trigger_rate(dev: &Device, name: &str, sample_rate: f64) {
    if dev.find_attr("sampling_frequency").is_none() {
        log::debug!(
            "Trigger {name} has no sampling_frequency attr (data-ready trigger), skipping"
        );
        return;
    }
    if let Err(e) = dev.attr_write("sampling_frequency", sample_rate as i64) {
        log::warn!("Failed to set trigger {name} sampling frequency to {sample_rate}: {e}");
    }
}
