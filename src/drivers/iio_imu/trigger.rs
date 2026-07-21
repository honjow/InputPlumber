use industrial_io::{Context, Device};

pub fn find_trigger(ctx: &Context, device_name: &str, sample_rate: f64) -> Option<Device> {
    let num_devices = ctx.num_devices();
    let mut matched_trigger: Option<Device> = None;
    let mut hrtimer_trigger: Option<Device> = None;
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

        if matched_trigger.is_none() && name.starts_with(device_name) {
            matched_trigger = Some(dev);
        } else if hrtimer_trigger.is_none() && name.contains("hrtimer") {
            hrtimer_trigger = Some(dev);
        } else if fallback_trigger.is_none() {
            fallback_trigger = Some(dev);
        }
    }

    let selected = matched_trigger.or(hrtimer_trigger).or(fallback_trigger);
    if let Some(ref trig) = selected {
        let name = trig.name().unwrap_or_default();
        try_set_trigger_rate(trig, &name, sample_rate);
        log::info!("Selected trigger: {name}");
    }

    selected
}

fn try_set_trigger_rate(dev: &Device, name: &str, sample_rate: f64) {
    if dev.find_attr("sampling_frequency").is_none() {
        return;
    }
    if let Err(e) = dev.attr_write("sampling_frequency", sample_rate as i64) {
        log::warn!("Failed to set trigger {name} sampling frequency to {sample_rate}: {e}");
    }
}
