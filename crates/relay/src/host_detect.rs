//! Auto-detect a ranked list of host candidates the phone might use to dial
//! the relay. Pure best-effort: each source can fail independently; we just
//! collect what works.
//!
//! Order roughly mirrors "most likely to work cross-network" first:
//! Tailscale DNS name → Tailscale IPs → Bonjour `.local` → private LAN IPv4s.
//! Loopback is added last as a safety net (only useful for the iOS simulator
//! on the same Mac).
//!
//! Diagnostic logging: detection emits `tracing::info!` lines summarizing
//! what was discovered and `tracing::debug!` lines explaining each failed
//! tailscale-binary probe. This is deliberate — sandboxed launchd contexts
//! can silently lose access to `/Applications/Tailscale.app/...` and the
//! logs are how we tell the user "we looked here, it didn't work, supply
//! `host.tailscale_bin` in config.toml". Set `RUST_LOG=alleycat=debug` to
//! see the per-candidate probe output.

use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::{debug, info};

/// Detect candidate hostnames/IPs the phone could dial to reach this relay,
/// using the default per-OS tailscale binary search.
///
/// Always returns at least one entry (`127.0.0.1` if everything else fails).
/// De-duplicates while preserving the priority order.
pub fn detect_candidates() -> Vec<String> {
    detect_candidates_with(None)
}

/// Like [`detect_candidates`] but lets the caller supply an explicit
/// tailscale binary (typically from `config.toml`'s `[host] tailscale_bin`).
/// When `tailscale_bin` is `Some`, the per-OS auto-discovery list is
/// skipped entirely so the user's override is the only path probed.
pub fn detect_candidates_with(tailscale_bin: Option<&Path>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: Option<String>| {
        if let Some(s) = s {
            let s = s.trim().to_string();
            if !s.is_empty() && !out.iter().any(|existing| existing == &s) {
                out.push(s);
            }
        }
    };

    // 1+2: Tailscale tailnet DNS name + IPs (works cross-network).
    if let Some((bin_used, dns, ips)) = tailscale_self(tailscale_bin) {
        info!(
            tailscale_bin = ?bin_used,
            dns = ?dns,
            ipv4_count = ips.len(),
            "tailscale detected"
        );
        push(dns);
        for ip in ips {
            push(Some(ip));
        }
    } else {
        debug!("tailscale detection produced no candidates");
    }

    // 3: macOS Bonjour `.local` (works on same LAN, iOS resolves natively).
    let bonjour = bonjour_local_name();
    push(bonjour.clone());

    // 4: Private LAN IPv4s from getifaddrs.
    let lan_ips = private_lan_ipv4s();
    let lan_count = lan_ips.len();
    for ip in lan_ips {
        push(Some(ip));
    }

    info!(bonjour = ?bonjour, lan_count, "host_detect summary");

    if out.is_empty() {
        out.push("127.0.0.1".to_string());
    }
    out
}

/// Probe one tailscale binary at `bin` for `status --json`. Returns
/// `(bin, Some(dns), ips)` on success. Emits `debug!` lines explaining
/// every failure mode so a sandboxed daemon's log makes the cause visible.
fn try_tailscale_at(bin: &Path) -> Option<(Option<String>, Vec<String>)> {
    let output = match Command::new(bin).args(["status", "--json"]).output() {
        Ok(o) => o,
        Err(error) => {
            debug!(?bin, %error, "tailscale spawn failed");
            return None;
        }
    };
    if !output.status.success() {
        debug!(
            ?bin,
            exit = ?output.status.code(),
            stderr = %String::from_utf8_lossy(&output.stderr),
            "tailscale exited nonzero"
        );
        return None;
    }
    let v: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(error) => {
            debug!(?bin, %error, "tailscale JSON parse failed");
            return None;
        }
    };
    let dns = v["Self"]["DNSName"]
        .as_str()
        .map(|s| s.trim_end_matches('.').to_string())
        .filter(|s| !s.is_empty());
    let ips = v["Self"]["TailscaleIPs"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str())
                .filter(|s| !s.contains(':')) // IPv4 only — most reliable for QUIC dial
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if dns.is_none() && ips.is_empty() {
        debug!(
            ?bin,
            "tailscale status had no Self.DNSName or IPs (logged out?)"
        );
        return None;
    }
    Some((dns, ips))
}

/// Locate a working tailscale binary and return `(path, dns, ipv4s)`. Probes
/// the candidates in order — explicit absolute paths first, then `which`
/// fallback — and returns on the first one that produces a usable response.
///
/// The binaries-list-first ordering is deliberate: a stale shim earlier on
/// `$PATH` (npm-style wrappers, brew shims for an uninstalled cask, etc.)
/// can return success without actually talking to tailscaled. The canonical
/// install paths are more reliable.
fn tailscale_self(explicit_bin: Option<&Path>) -> Option<(PathBuf, Option<String>, Vec<String>)> {
    if let Some(bin) = explicit_bin {
        // User-supplied path. Don't fall back if it fails — the user wants
        // determinism, and silently ignoring their config would mask the
        // real problem.
        if !bin.exists() {
            debug!(?bin, "configured host.tailscale_bin does not exist");
            return None;
        }
        let (dns, ips) = try_tailscale_at(bin)?;
        return Some((bin.to_path_buf(), dns, ips));
    }

    let candidates = which_tailscale_candidates();
    if candidates.is_empty() {
        debug!("no tailscale candidate paths to probe");
        return None;
    }
    for c in &candidates {
        if let Some((dns, ips)) = try_tailscale_at(c) {
            return Some((c.clone(), dns, ips));
        }
    }
    debug!(
        probed = candidates.len(),
        "no tailscale candidate succeeded"
    );
    None
}

/// All candidate tailscale binary paths to probe, in priority order.
/// Absolute paths from canonical installs first; `which::which` fallback
/// last so a stale PATH shim doesn't preempt the real binary.
fn which_tailscale_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = default_tailscale_paths()
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    if let Ok(p) = which::which("tailscale")
        && !out.iter().any(|existing| existing == &p)
    {
        out.push(p);
    }
    out
}

/// Per-OS canonical install paths to probe. Pure data — split out so tests
/// don't need a real tailscale install. Always returns absolute paths;
/// callers filter by existence.
fn default_tailscale_paths() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();

    #[cfg(target_os = "macos")]
    {
        v.push(PathBuf::from(
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        ));
        v.push(PathBuf::from("/usr/local/bin/tailscale"));
        v.push(PathBuf::from("/opt/homebrew/bin/tailscale"));
    }

    #[cfg(target_os = "linux")]
    {
        v.push(PathBuf::from("/usr/bin/tailscale"));
        v.push(PathBuf::from("/usr/local/bin/tailscale"));
        v.push(PathBuf::from("/usr/sbin/tailscale"));
        v.push(PathBuf::from("/snap/bin/tailscale"));
        v.push(PathBuf::from("/var/lib/snapd/snap/bin/tailscale"));
    }

    #[cfg(target_os = "windows")]
    {
        v.push(PathBuf::from(r"C:\Program Files\Tailscale\tailscale.exe"));
        v.push(PathBuf::from(
            r"C:\Program Files (x86)\Tailscale\tailscale.exe",
        ));
        // Honor the redirected env vars too — some sysprep'd images set
        // ProgramFiles to a non-default drive.
        if let Ok(pf) = std::env::var("ProgramFiles") {
            v.push(PathBuf::from(pf).join(r"Tailscale\tailscale.exe"));
        }
        if let Ok(pf86) = std::env::var("ProgramFiles(x86)") {
            v.push(PathBuf::from(pf86).join(r"Tailscale\tailscale.exe"));
        }
    }

    v
}

fn bonjour_local_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("scutil")
            .args(["--get", "LocalHostName"])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let name = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if name.is_empty() {
            return None;
        }
        Some(format!("{name}.local"))
    }
    #[cfg(target_os = "linux")]
    {
        if !linux_avahi_running() {
            return None;
        }
        let h = hostname::get().ok()?.into_string().ok()?;
        let h = h.trim().to_string();
        if h.is_empty() {
            return None;
        }
        Some(format!("{h}.local"))
    }
    #[cfg(target_os = "windows")]
    {
        // Bonjour-for-Windows is rare; let user supply via host.overrides.
        None
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn linux_avahi_running() -> bool {
    std::path::Path::new("/var/run/avahi-daemon/socket").exists()
        || std::path::Path::new("/run/avahi-daemon/socket").exists()
}

fn private_lan_ipv4s() -> Vec<String> {
    let Ok(addrs) = local_ip_address::list_afinet_netifas() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (iface, addr) in addrs {
        if !is_lan_interface(&iface) {
            continue;
        }
        let std::net::IpAddr::V4(v4) = addr else {
            continue;
        };
        if !is_private_v4(v4) {
            continue;
        }
        out.push(v4.to_string());
    }
    out
}

#[cfg(unix)]
fn is_lan_interface(name: &str) -> bool {
    // Keep wired/wifi (en*), Thunderbolt-bridge (en*), and eth* on Linux.
    // Skip tunnels (utun, ipsec), cellular (pdp_ip), AWDL/llw, hotspot (ap*),
    // bridges (bridge*), and VM/container interfaces (vmnet*, vboxnet*, docker*).
    if name.starts_with("utun")
        || name.starts_with("ipsec")
        || name.starts_with("pdp_ip")
        || name.starts_with("awdl")
        || name.starts_with("llw")
        || name.starts_with("ap")
        || name.starts_with("bridge")
        || name.starts_with("vmnet")
        || name.starts_with("vboxnet")
        || name.starts_with("docker")
        || name == "lo"
        || name.starts_with("lo0")
    {
        return false;
    }
    name.starts_with("en") || name.starts_with("eth") || name.starts_with("wlan")
}

#[cfg(target_os = "windows")]
fn is_lan_interface(name: &str) -> bool {
    // Windows interfaces use friendly names like "Ethernet", "Wi-Fi",
    // "Loopback Pseudo-Interface 1", "vEthernet (WSL)". Use a deny-list of
    // substrings rather than a starts_with allow-list.
    let lower = name.to_ascii_lowercase();
    ![
        "loopback",
        "vethernet",
        "virtualbox",
        "hyper-v",
        "bluetooth",
        "wsl",
        "tap",
        "tun",
        "npcap",
    ]
    .iter()
    .any(|bad| lower.contains(bad))
}

fn is_private_v4(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    // 10/8
    o[0] == 10
        // 172.16/12
        || (o[0] == 172 && (16..=31).contains(&o[1]))
        // 192.168/16
        || (o[0] == 192 && o[1] == 168)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_returns_at_least_loopback_when_everything_fails() {
        // Even on a hermetic CI box with no network and no Tailscale, we
        // should still return a usable string the caller can fall back to.
        let candidates = detect_candidates();
        assert!(!candidates.is_empty(), "expected at least one candidate");
    }

    #[test]
    fn private_v4_classifier() {
        assert!(is_private_v4("10.0.0.1".parse().unwrap()));
        assert!(is_private_v4("172.16.0.1".parse().unwrap()));
        assert!(is_private_v4("172.31.255.255".parse().unwrap()));
        assert!(is_private_v4("192.168.1.1".parse().unwrap()));
        assert!(!is_private_v4("172.32.0.1".parse().unwrap()));
        assert!(!is_private_v4("8.8.8.8".parse().unwrap()));
        assert!(!is_private_v4("100.64.0.1".parse().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn lan_interface_filter() {
        assert!(is_lan_interface("en0"));
        assert!(is_lan_interface("en1"));
        assert!(is_lan_interface("eth0"));
        assert!(!is_lan_interface("utun5"));
        assert!(!is_lan_interface("ipsec0"));
        assert!(!is_lan_interface("pdp_ip0"));
        assert!(!is_lan_interface("awdl0"));
        assert!(!is_lan_interface("bridge0"));
        assert!(!is_lan_interface("vmnet1"));
        assert!(!is_lan_interface("docker0"));
        assert!(!is_lan_interface("lo0"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn lan_interface_filter_windows() {
        assert!(is_lan_interface("Ethernet"));
        assert!(is_lan_interface("Ethernet 2"));
        assert!(is_lan_interface("Wi-Fi"));
        assert!(!is_lan_interface("Loopback Pseudo-Interface 1"));
        assert!(!is_lan_interface("vEthernet (WSL)"));
        assert!(!is_lan_interface("vEthernet (Default Switch)"));
        assert!(!is_lan_interface("VirtualBox Host-Only Network"));
        assert!(!is_lan_interface("Bluetooth Network Connection"));
        assert!(!is_lan_interface("TAP-Windows Adapter V9"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_avahi_running_is_callable() {
        // Just make sure the helper compiles and returns a bool. Whether
        // avahi is actually running on the test host is environment-specific;
        // we do not assert a value, only that the function does not panic.
        let _ = linux_avahi_running();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn default_tailscale_paths_macos_includes_canonical_install() {
        let paths = default_tailscale_paths();
        let strs: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        assert!(
            strs.iter()
                .any(|s| s.contains("/Applications/Tailscale.app"))
        );
        assert!(strs.iter().any(|s| s == "/usr/local/bin/tailscale"));
        assert!(strs.iter().any(|s| s == "/opt/homebrew/bin/tailscale"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn default_tailscale_paths_linux_includes_snap_and_sbin() {
        let paths = default_tailscale_paths();
        let strs: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        assert!(strs.iter().any(|s| s == "/usr/bin/tailscale"));
        assert!(strs.iter().any(|s| s == "/usr/local/bin/tailscale"));
        assert!(strs.iter().any(|s| s == "/usr/sbin/tailscale"));
        assert!(strs.iter().any(|s| s == "/snap/bin/tailscale"));
        assert!(
            strs.iter()
                .any(|s| s == "/var/lib/snapd/snap/bin/tailscale")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn which_tailscale_falls_through_when_path_empty() {
        // Wipe PATH so `which::which` cannot find anything; the function
        // should still scan the canonical install list and degrade
        // gracefully when none exist on the test host.
        // SAFETY: tests in this module do not concurrently mutate PATH;
        // CI typically does not have tailscale installed under
        // /snap/bin etc., so this exercises the empty-result path.
        let saved = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", "") };
        let candidates = which_tailscale_candidates();
        // If a canonical install happens to exist on the runner we get
        // a non-empty list; otherwise empty. Either is fine — the contract
        // is that the function does not panic and never returns a path
        // that doesn't exist on disk.
        for c in &candidates {
            assert!(c.exists(), "candidate {c:?} should exist on disk");
        }
        match saved {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn default_tailscale_paths_windows_includes_program_files() {
        let saved_pf = std::env::var_os("ProgramFiles");
        let saved_pf86 = std::env::var_os("ProgramFiles(x86)");
        unsafe { std::env::set_var("ProgramFiles", r"D:\Apps") };
        unsafe { std::env::set_var("ProgramFiles(x86)", r"D:\Apps86") };
        let paths = default_tailscale_paths();
        let strs: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        assert!(
            strs.iter()
                .any(|s| s.contains(r"Program Files\Tailscale\tailscale.exe"))
        );
        assert!(
            strs.iter()
                .any(|s| s.contains(r"D:\Apps\Tailscale\tailscale.exe"))
        );
        assert!(
            strs.iter()
                .any(|s| s.contains(r"D:\Apps86\Tailscale\tailscale.exe"))
        );
        match saved_pf {
            Some(v) => unsafe { std::env::set_var("ProgramFiles", v) },
            None => unsafe { std::env::remove_var("ProgramFiles") },
        }
        match saved_pf86 {
            Some(v) => unsafe { std::env::set_var("ProgramFiles(x86)", v) },
            None => unsafe { std::env::remove_var("ProgramFiles(x86)") },
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn which_tailscale_falls_through_when_path_empty() {
        let saved = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", "") };
        let candidates = which_tailscale_candidates();
        for c in &candidates {
            assert!(c.exists(), "candidate {c:?} should exist on disk");
        }
        match saved {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }

    #[test]
    fn explicit_bin_missing_returns_none() {
        let bogus = std::path::PathBuf::from("/this/path/should/not/exist/tailscale-9d3f1ab");
        assert!(tailscale_self(Some(&bogus)).is_none());
    }
}
