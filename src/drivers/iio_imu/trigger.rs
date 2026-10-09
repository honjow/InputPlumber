use industrial_io::{Context, Device};

pub fn find_trigger(
    ctx: &Context,
    device_name: &str,
    sample_rate: f64,
    prefer_hrtimer: bool,
) -> Option<Device> {
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

    let selected = select_trigger(
        matched_trigger,
        hrtimer_trigger,
        fallback_trigger,
        prefer_hrtimer,
    );
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

/// Normally prefer the device's hardware trigger. On affected resume paths,
/// prefer hrtimer even when the now-broken hardware trigger is still advertised.
fn select_trigger<T>(
    matched: Option<T>,
    hrtimer: Option<T>,
    fallback: Option<T>,
    prefer_hrtimer: bool,
) -> Option<T> {
    if prefer_hrtimer {
        hrtimer.or(matched).or(fallback)
    } else {
        matched.or(hrtimer).or(fallback)
    }
}

#[cfg(test)]
mod tests {
    use super::select_trigger;

    #[test]
    fn hardware_trigger_is_preferred_on_initial_open() {
        assert_eq!(
            select_trigger(Some("hardware"), Some("hrtimer"), Some("other"), false),
            Some("hardware")
        );
    }

    #[test]
    fn hrtimer_is_preferred_after_bmi_resume() {
        assert_eq!(
            select_trigger(Some("hardware"), Some("hrtimer"), Some("other"), true),
            Some("hrtimer")
        );
    }

    #[test]
    fn unavailable_hrtimer_keeps_existing_fallback_order() {
        assert_eq!(
            select_trigger(Some("hardware"), None, Some("other"), true),
            Some("hardware")
        );
        assert_eq!(
            select_trigger(None, None, Some("other"), true),
            Some("other")
        );
        assert_eq!(select_trigger::<&str>(None, None, None, true), None);
    }
}
