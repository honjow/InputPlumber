//! Based on the pattern developed by the hhd project:
//! https://github.com/hhd-dev/hhd/blob/master/src/hhd/controller/lib/hide.py

#[cfg(test)]
pub mod device_test;

pub mod device;

use std::{
    error::Error,
    fs,
    io::ErrorKind,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use tokio::process::Command;
use udev::Enumerator;

use self::device::Device;

const RULE_HIDE_DEVICE_EARLY_PRIORITY: &str = "50";
const RULE_HIDE_DEVICE_LATE_PRIORITY: &str = "96";
const RULES_PREFIX: &str = "/run/udev/rules.d";

/// HideFlags can be used to change the behavior of how devices are hidden.
#[derive(Debug, PartialEq, Eq)]
pub enum HideFlag {
    ChangePermissions,
    MoveSourceDevice,
}

/// Hide the given input device from regular users.
pub async fn hide_device(path: &str, flags: &[HideFlag]) -> Result<(), Box<dyn Error>> {
    // Get the device to hide
    let device = get_device(path.to_string()).await?;
    let name = device.name.clone();
    let Some(parent) = device.get_parent() else {
        return Err("Unable to determine parent for device".into());
    };
    let subsystem = device.subsystem.clone();
    let Some(match_rule) = device.get_match_rule() else {
        return Err("Unable to create match rule for device".into());
    };

    // Create the udev rule content to update permissions on the source node.
    let mut chmod_early_rule = String::new();
    let mut chmod_late_rule = String::new();
    if flags.contains(&HideFlag::ChangePermissions) {
        // Find the chmod command to use for hiding
        let chmod_cmd = if Path::new("/bin/chmod").exists() {
            "/bin/chmod".to_string()
        } else if Path::new("/usr/bin/chmod").exists() {
            "/usr/bin/chmod".to_string()
        } else if Path::new("/run/current-system/sw/bin/chmod").exists() {
            "/run/current-system/sw/bin/chmod".to_string()
        } else {
            let output = Command::new("sh")
                .arg("-c")
                .arg("which chmod")
                .output()
                .await?;
            if !output.status.success() {
                return Err("Unable to determine chmod command location".into());
            }
            str::from_utf8(output.stdout.as_slice())?.trim().to_string()
        };

        // Build the rule content
        chmod_early_rule = format!(
            r#"KERNEL=="js[0-9]*|event[0-9]*", SUBSYSTEM=="{subsystem}", MODE:="0000", GROUP:="root", RUN+="{chmod_cmd} 000 /dev/input/%k", SYMLINK+="inputplumber/by-hidden/%k"
KERNEL=="hidraw[0-9]*", SUBSYSTEM=="{subsystem}", MODE:="0000", GROUP:="root", RUN+="{chmod_cmd} 000 /dev/%k", SYMLINK+="inputplumber/by-hidden/%k"
"#
        );
        chmod_late_rule = format!(
            r#"KERNEL=="js[0-9]*|event[0-9]*", SUBSYSTEM=="{subsystem}", MODE="000", GROUP="root", TAG-="uaccess", RUN+="{chmod_cmd} 000 /dev/input/%k"
KERNEL=="hidraw[0-9]*", SUBSYSTEM=="{subsystem}", MODE="000", GROUP="root", TAG-="uaccess", RUN+="{chmod_cmd} 000 /dev/%k"
"#
        );
    }

    // Create the udev rule content to move the device node
    let mut mv_early_rule = String::new();
    let mut mv_late_rule = String::new();
    if flags.contains(&HideFlag::MoveSourceDevice) {
        // Create the directory to move devnodes to
        tokio::fs::create_dir_all("/dev/inputplumber/sources").await?;

        // Find the mv command to use for hiding
        let mv_cmd = if Path::new("/bin/mv").exists() {
            "/bin/mv".to_string()
        } else if Path::new("/usr/bin/mv").exists() {
            "/usr/bin/mv".to_string()
        } else if Path::new("/run/current-system/sw/bin/mv").exists() {
            "/run/current-system/sw/bin/mv".to_string()
        } else {
            let output = Command::new("sh")
                .arg("-c")
                .arg("which mv")
                .output()
                .await?;
            if !output.status.success() {
                return Err("Unable to determine mv command location".into());
            }
            str::from_utf8(output.stdout.as_slice())?.trim().to_string()
        };

        // Build the rule content
        mv_early_rule = format!(
            r#"KERNEL=="js[0-9]*|event[0-9]*", SUBSYSTEM=="{subsystem}", RUN+="{mv_cmd} /dev/input/%k /dev/inputplumber/sources/%k"
KERNEL=="hidraw[0-9]*", SUBSYSTEM=="{subsystem}", RUN+="{mv_cmd} /dev/%k /dev/inputplumber/sources/%k"
"#
        );
        mv_late_rule = mv_early_rule.clone();
    }

    // Create an early udev rule to hide the device
    let rule = format!(
        r#"# Hides devices stemming from {name}
# Managed by InputPlumber, this file will be autoremoved during configuration changes.
{match_rule}, GOTO="inputplumber_valid"
GOTO="inputplumber_end"
LABEL="inputplumber_valid"
{chmod_early_rule}
{mv_early_rule}
LABEL="inputplumber_end"
"#
    );
    fs::create_dir_all(RULES_PREFIX)?;
    let rule_path = format!(
        "{RULES_PREFIX}/{RULE_HIDE_DEVICE_EARLY_PRIORITY}-inputplumber-hide-{name}-early.rules"
    );
    fs::write(rule_path, rule)?;

    // Create a late udev rule to hide the device. This is needed for devices that
    // are available at boot time because the early rule will not be applied.
    let rule = format!(
        r#"# Hides devices stemming from {name}
# Managed by InputPlumber, this file will be autoremoved during configuration changes.
{match_rule}, GOTO="inputplumber_valid"
GOTO="inputplumber_end"
LABEL="inputplumber_valid"
{chmod_late_rule}
{mv_late_rule}
LABEL="inputplumber_end"
"#
    );
    let rule_path = format!(
        "{RULES_PREFIX}/{RULE_HIDE_DEVICE_LATE_PRIORITY}-inputplumber-hide-{name}-late.rules"
    );
    fs::write(rule_path, rule)?;

    // Reload udev
    reload_children(parent).await?;

    Ok(())
}

/// Unhide the given device
pub async fn unhide_device(path: String) -> Result<(), Box<dyn Error>> {
    // Get the device to unhide. If this fails, continue with a best-effort
    // permission restore so source devices don't remain unusable.
    let device = match get_device(path.clone()).await {
        Ok(device) => Some(device),
        Err(e) => {
            log::warn!("Failed to query udev data for {path}: {e}");
            None
        }
    };

    if let Some(device) = device {
        let parent = device.get_parent();
        let name = device.name;
        let rule_path = format!(
            "{RULES_PREFIX}/{RULE_HIDE_DEVICE_EARLY_PRIORITY}-inputplumber-hide-{name}-early.rules"
        );
        log::debug!("Removing hide rule: {rule_path}");
        if let Err(e) = fs::remove_file(&rule_path) {
            if e.kind() != ErrorKind::NotFound {
                log::warn!("Failed removing hide rule {rule_path}: {e}");
            }
        }
        let rule_path = format!(
            "{RULES_PREFIX}/{RULE_HIDE_DEVICE_LATE_PRIORITY}-inputplumber-hide-{name}-late.rules"
        );
        log::debug!("Removing hide rule: {rule_path}");
        if let Err(e) = fs::remove_file(&rule_path) {
            if e.kind() != ErrorKind::NotFound {
                log::warn!("Failed removing hide rule {rule_path}: {e}");
            }
        }

        // Move the device back
        let src_path = format!("/dev/inputplumber/sources/{name}");
        if PathBuf::from(&src_path).exists() {
            let dst_path = if name.starts_with("event") || name.starts_with("js") {
                format!("/dev/input/{name}")
            } else {
                format!("/dev/{name}")
            };
            log::debug!("Restoring device node path '{src_path}' to '{dst_path}'");
            if let Err(e) = fs::rename(&src_path, &dst_path) {
                log::warn!("Failed to move device node from {src_path} to {dst_path}: {e}");
            }
        }

        // Reload udev if we were able to discover the parent device.
        if let Some(parent) = parent {
            if let Err(e) = reload_children(parent).await {
                log::warn!("Failed reloading udev after unhiding {name}: {e}");
            }
        }
    }

    // Always perform a permission restore pass to avoid lingering MODE=000
    // nodes when rule cleanup/reload partially fails.
    if let Err(e) = restore_hidden_input_permissions().await {
        log::warn!("Failed restoring hidden input permissions for {path}: {e}");
    }
    Ok(())
}

/// Unhide all devices hidden by InputPlumber
pub async fn unhide_all() -> Result<(), Box<dyn Error>> {
    // Remove all created udev rules
    match fs::read_dir(RULES_PREFIX) {
        Ok(entries) => {
            for entry in entries {
                let Ok(entry) = entry else {
                    continue;
                };
                let filename = entry.file_name().to_string_lossy().to_string();
                if !filename.contains("-inputplumber-hide-") {
                    continue;
                }
                let path = entry.path().to_string_lossy().to_string();
                log::debug!("Removing hide rule: {path}");
                if let Err(e) = fs::remove_file(&path) {
                    if e.kind() != ErrorKind::NotFound {
                        log::warn!("Failed removing hide rule {path}: {e}");
                    }
                }
            }
        }
        Err(e) => {
            if e.kind() != ErrorKind::NotFound {
                log::warn!("Failed reading {RULES_PREFIX}: {e}");
            }
        }
    }

    // Move all devices back
    match fs::read_dir("/dev/inputplumber/sources") {
        Ok(entries) => {
            for entry in entries {
                let Ok(entry) = entry else {
                    continue;
                };
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                let name = name.as_str();
                let dst_path = if name.starts_with("event") || name.starts_with("js") {
                    format!("/dev/input/{name}")
                } else {
                    format!("/dev/{name}")
                };
                log::debug!("Restoring device node path {path:?} to '{dst_path}'");
                if let Err(e) = fs::rename(&path, &dst_path) {
                    log::warn!("Failed to move device node from {path:?} to {dst_path}: {e}");
                }
            }
        }
        Err(e) => {
            if e.kind() != ErrorKind::NotFound {
                log::warn!("Failed reading /dev/inputplumber/sources: {e}");
            }
        }
    }

    // Reload udev rules
    if let Err(e) = reload_all().await {
        log::warn!("Failed reloading udev rules while unhiding all devices: {e}");
    }

    // Final fallback in case udev rule reload did not restore permissions.
    if let Err(e) = restore_hidden_input_permissions().await {
        log::warn!("Failed restoring hidden input permissions: {e}");
    }

    Ok(())
}

/// Restore permissions for input device nodes that remain hidden (mode 000).
async fn restore_hidden_input_permissions() -> Result<(), Box<dyn Error>> {
    restore_hidden_nodes_in_dir("/dev/input").await?;
    restore_hidden_nodes_in_dir("/dev").await?;
    Ok(())
}

async fn restore_hidden_nodes_in_dir(dir: &str) -> Result<(), Box<dyn Error>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            if e.kind() == ErrorKind::NotFound {
                return Ok(());
            }
            return Err(e.into());
        }
    };

    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let relevant = name.starts_with("event")
            || name.starts_with("js")
            || (dir == "/dev" && name.starts_with("hidraw"));
        if !relevant {
            continue;
        }
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if (metadata.permissions().mode() & 0o777) != 0 {
            continue;
        }

        let mut permissions = metadata.permissions();
        permissions.set_mode(0o660);
        if let Err(e) = fs::set_permissions(&path, permissions) {
            log::warn!("Failed setting permissions on {path:?}: {e}");
            continue;
        }

        // Best effort: restore group ownership for normal input access.
        let path_str = path.to_string_lossy().to_string();
        let _ = Command::new("chgrp")
            .args(["input", path_str.as_str()])
            .output()
            .await;
        let _ = Command::new("setfacl")
            .args(["-b", path_str.as_str()])
            .output()
            .await;
    }

    Ok(())
}

/// Trigger udev to evaluate rules on the children of the given parent device path
async fn reload_children(parent: String) -> Result<(), Box<dyn Error>> {
    log::debug!("Reloading udev rules: udevadm control --reload-rules");
    let _ = Command::new("udevadm")
        .args(["control", "--reload-rules"])
        .output()
        .await?;

    for action in ["remove", "add"] {
        log::debug!("Retriggering udev rules: udevadm trigger --action {action} -b {parent}");
        let _ = Command::new("udevadm")
            .args(["trigger", "--action", action, "-b", parent.as_str()])
            .output()
            .await?;
    }

    Ok(())
}

/// Trigger udev to evaluate rules on the children of the given parent device path
async fn reload_all() -> Result<(), Box<dyn Error>> {
    log::debug!("Reloading udev rules: udevadm control --reload-rules");
    let _ = Command::new("udevadm")
        .args(["control", "--reload-rules"])
        .output()
        .await?;

    log::debug!("Retriggering udev rules: udevadm trigger");
    let _ = Command::new("udevadm").arg("trigger").output().await?;

    Ok(())
}

/// Returns device information for the given device path using udevadm.
pub async fn get_device(path: String) -> Result<Device, Box<dyn Error>> {
    let mut device = Device::default();
    let output = Command::new("udevadm")
        .args(["info", path.as_str()])
        .output()
        .await?;
    let output = String::from_utf8(output.stdout)?;

    for line in output.split('\n') {
        if line.starts_with("P: ") {
            let line = line.replace("P: ", "");
            device.path = line;
            continue;
        }
        if line.starts_with("M: ") {
            let line = line.replace("M: ", "");
            device.name = line;
            continue;
        }
        if line.starts_with("R: ") {
            let line = line.replace("R: ", "");
            let number = line.parse().unwrap_or_default();
            device.number = number;
            continue;
        }
        if line.starts_with("U: ") {
            let line = line.replace("U: ", "");
            device.subsystem = line;
            continue;
        }
        if line.starts_with("T: ") {
            let line = line.replace("T: ", "");
            device.device_type = line;
            continue;
        }
        if line.starts_with("D: ") {
            let line = line.replace("D: ", "");
            device.node = line;
            continue;
        }
        if line.starts_with("I: ") {
            let line = line.replace("I: ", "");
            device.network_index = line;
            continue;
        }
        if line.starts_with("N: ") {
            let line = line.replace("N: ", "");
            device.node_name = line;
            continue;
        }
        if line.starts_with("L: ") {
            let line = line.replace("L: ", "");
            let priority = line.parse().unwrap_or_default();
            device.symlink_priority = priority;
            continue;
        }
        if line.starts_with("S: ") {
            let line = line.replace("S: ", "");
            device.symlink.push(line);
            continue;
        }
        if line.starts_with("Q: ") {
            let line = line.replace("Q: ", "");
            let seq = line.parse().unwrap_or_default();
            device.sequence_num = seq;
            continue;
        }
        if line.starts_with("V: ") {
            let line = line.replace("V: ", "");
            device.driver = line;
            continue;
        }
        if line.starts_with("E: ") {
            let line = line.replace("E: ", "");
            let mut parts = line.splitn(2, '=');
            if parts.clone().count() != 2 {
                continue;
            }
            let key = parts.next().unwrap();
            let value = parts.last().unwrap();
            device.properties.insert(key.to_string(), value.to_string());
            continue;
        }
    }

    Ok(device)
}

/// Returns a list of devices in the given subsystem that have a devnode property.
pub fn discover_devices(subsystem: &str) -> Result<Vec<udev::Device>, Box<dyn Error>> {
    let mut enumerator = Enumerator::new()?;
    enumerator.match_subsystem(subsystem)?;

    log::debug!("Started udev {subsystem} enumerator.");

    let mut node_devices = Vec::new();
    let devices = enumerator.scan_devices()?;
    for device in devices {
        log::trace!(
            "Udev {subsystem} enumerator found device: {:?}",
            device.sysname()
        );

        if device.devnode().is_none() {
            log::debug!("No devnode found for device: {:?}", device);
        };

        node_devices.push(device);
    }

    Ok(node_devices)
}
