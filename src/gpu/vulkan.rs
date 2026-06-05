//! Vulkan fallback probe — last-resort "is there *any* GPU?" check.
//!
//! Calls `vulkaninfo --summary` (much faster than the full
//! `vulkaninfo`) and looks for GPU lines. We parse `deviceName`,
//! `vendorID`, and `deviceID` so we can tag each device with a
//! stable hardware ID (for cross-backend deduplication) even though
//! the summary format is unstable.

use std::process::Command;

use super::GpuDevice;

pub fn probe_devices() -> Option<Vec<GpuDevice>> {
  // Try JSON output first — it has `pciBusLocation` for reliable
  // cross-backend dedup. Fall back to summary-only parsing (which
  // gives `vendorID:deviceID` but no PCI bus) when JSON is
  // unavailable (some drivers suppress JSON output).
  let json_devices = probe_json().unwrap_or_default();
  if !json_devices.is_empty() {
    return Some(json_devices);
  }

  // Fallback: parse --summary for device names and vendorID:deviceID.
  let mut cmd = Command::new("vulkaninfo");
  cmd.arg("--summary");
  let output = super::run_with_timeout(cmd)?;
  if !output.status.success() {
    return None;
  }
  let stdout = String::from_utf8(output.stdout).ok()?;
  let gpus = parse(&stdout);
  if gpus.is_empty() {
    return None;
  }
  // Vulkan can't tell us vendor reliably or memory accurately. We
  // surface it under `Unknown` rather than mislabelling the card as
  // AMD — Intel Arc, llvmpipe (software), and AMD-without-rocm-smi
  // all hit this path on Linux, and the TUI renders
  // `backend  unknown` so the user knows the vendor probe failed.
  Some(
    gpus
      .into_iter()
      .map(|(name, device_id)| GpuDevice {
        name,
        backend: "unknown".into(),
        device_id: Some(device_id),
        ..Default::default()
      })
      .collect(),
  )
}

/// Parse `vulkaninfo -j` JSON for physical devices. Each device
/// carries `pciBusLocation` for cross-backend dedup.
fn probe_json() -> Option<Vec<GpuDevice>> {
  let mut cmd = Command::new("vulkaninfo");
  cmd.arg("-j");
  let output = super::run_with_timeout(cmd)?;
  if !output.status.success() {
    return None;
  }
  let stdout = String::from_utf8(output.stdout).ok()?;
  let gpus = parse_json(&stdout);
  if gpus.is_empty() {
    return None;
  }
  Some(gpus)
}

pub(crate) fn parse(stdout: &str) -> Vec<(String, String)> {
  let mut out = Vec::new();
  let mut current_name = String::new();
  let mut current_vendor = String::new();
  let mut current_device = String::new();
  for line in stdout.lines() {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("deviceName") {
      if let Some(idx) = rest.find('=') {
        current_name = rest[idx + 1..].trim().to_string();
      }
    } else if let Some(rest) = trimmed.strip_prefix("vendorID") {
      if let Some(idx) = rest.find('=') {
        current_vendor = rest[idx + 1..].trim().to_string();
      }
    } else if let Some(rest) = trimmed.strip_prefix("deviceID") {
      if let Some(idx) = rest.find('=') {
        current_device = rest[idx + 1..].trim().to_string();
      }
    }
    // Each GPU block ends at the next "GPU" header.
    // When we see a new GPU or EOF, emit the previous device.
    if trimmed.starts_with("GPU") && !current_name.is_empty() {
      if !current_vendor.is_empty() && !current_device.is_empty() {
        out.push((
          current_name.clone(),
          format!("{current_vendor}:{current_device}"),
        ));
      }
      current_name.clear();
      current_vendor.clear();
      current_device.clear();
    }
  }
  // Emit the last device (no trailing GPU header).
  if !current_name.is_empty() && !current_vendor.is_empty() && !current_device.is_empty() {
    out.push((
      current_name.clone(),
      format!("{current_vendor}:{current_device}"),
    ));
  }
  out
}

/// Parse `vulkaninfo -j` JSON for physical devices. Each device
/// carries `pciBusLocation` for cross-backend dedup.
fn parse_json(stdout: &str) -> Vec<GpuDevice> {
  let v: serde_json::Value = match serde_json::from_str(stdout) {
    Ok(v) => v,
    Err(_) => return Vec::new(),
  };
  let mut out = Vec::new();
  let devices: &[serde_json::Value] = v
    .get("physicalDevices")
    .and_then(|v| v.as_array())
    .map(|arr| arr.as_slice())
    .unwrap_or(&[]);
  for dev in devices {
    let props = dev.get("properties").and_then(|props| props.as_object());
    let name = props
      .and_then(|p| p.get("deviceName"))
      .and_then(|val| val.as_str())
      .map(str::to_string)
      .unwrap_or_default();
    let pci_bus = props
      .and_then(|p| p.get("pciBusLocation"))
      .and_then(|val| val.as_str())
      .map(|s| {
        let trimmed = s.trim().trim_start_matches(':');
        if trimmed.is_empty() {
          s.to_string()
        } else {
          trimmed.to_string()
        }
      });
    let vendor_id = props
      .and_then(|p| p.get("vendorID"))
      .and_then(|vendor| vendor.as_str())
      .map(str::to_string);
    let device_id = props
      .and_then(|p| p.get("deviceID"))
      .and_then(|id_val| id_val.as_str())
      .map(str::to_string);
    let device_id = pci_bus.unwrap_or_else(|| match (vendor_id, device_id) {
      (Some(vendor), Some(id_val)) => format!("{vendor}:{id_val}"),
      _ => String::new(),
    });
    if !name.is_empty() && !device_id.is_empty() {
      out.push(GpuDevice {
        name,
        backend: "unknown".into(),
        device_id: Some(device_id),
        ..Default::default()
      });
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn extracts_device_names_and_ids_from_vulkaninfo_summary() {
    let stdout = "==========\n\
                  GPU0:\n\
                  \tdeviceName       = AMD Radeon RX 7900 XTX\n\
                  \tvendorID         = 0x1002\n\
                  \tdeviceID         = 0x7551\n\
                  \tapiVersion       = 1.3.250\n\
                  GPU1:\n\
                  \tdeviceName       = llvmpipe (LLVM 16.0.6, 256 bits)\n\
                  \tvendorID         = 0x10de\n\
                  \tdeviceID         = 0x2216\n";
    let gpus = parse(stdout);
    assert_eq!(gpus.len(), 2);
    assert_eq!(
      gpus[0],
      ("AMD Radeon RX 7900 XTX".into(), "0x1002:0x7551".into())
    );
    assert_eq!(
      gpus[1],
      (
        "llvmpipe (LLVM 16.0.6, 256 bits)".into(),
        "0x10de:0x2216".into()
      )
    );
  }

  #[test]
  fn empty_summary_yields_no_devices() {
    assert!(parse("").is_empty());
    assert!(parse("WARNING: vulkan loader missing\n").is_empty());
  }
}
