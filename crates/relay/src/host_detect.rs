//! Auto-detect a ranked list of host candidates the phone might use to dial
//! the relay. Pure best-effort: each source can fail independently; we just
//! collect what works.
//!
//! Order roughly mirrors "most likely to work cross-network" first:
//! Tailscale DNS name → Tailscale IPs → Bonjour `.local` → private LAN IPv4s.
//! Loopback is added last as a safety net (only useful for the iOS simulator
//! on the same Mac).

use std::path::PathBuf;
use std::process::Command;

/// Detect candidate hostnames/IPs the phone could dial to reach this relay.
///
/// Always returns at least one entry (`127.0.0.1` if everything else fails).
/// De-duplicates while preserving the priority order.
pub fn detect_candidates() -> Vec<String> {
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
    if let Some((dns, ips)) = tailscale_self() {
        push(dns);
        for ip in ips {
            push(Some(ip));
        }
    }

    // 3: macOS Bonjour `.local` (works on same LAN, iOS resolves natively).
    push(bonjour_local_name());

    // 4: Private LAN IPv4s from getifaddrs.
    for ip in private_lan_ipv4s() {
        push(Some(ip));
    }

    if out.is_empty() {
        out.push("127.0.0.1".to_string());
    }
    out
}

fn tailscale_self() -> Option<(Option<String>, Vec<String>)> {
    let bin = which_tailscale()?;
    let output = Command::new(&bin)
        .args(["status", "--json"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
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
        None
    } else {
        Some((dns, ips))
    }
}

fn which_tailscale() -> Option<PathBuf> {
    if let Ok(p) = which::which("tailscale") {
        return Some(p);
    }
    let candidates: Vec<&'static str> = vec![
        #[cfg(target_os = "macos")]
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        #[cfg(target_os = "macos")]
        "/usr/local/bin/tailscale",
        #[cfg(target_os = "macos")]
        "/opt/homebrew/bin/tailscale",
        #[cfg(target_os = "linux")]
        "/usr/bin/tailscale",
        #[cfg(target_os = "linux")]
        "/usr/local/bin/tailscale",
        #[cfg(target_os = "windows")]
        r"C:\Program Files\Tailscale\tailscale.exe",
        #[cfg(target_os = "windows")]
        r"C:\Program Files (x86)\Tailscale\tailscale.exe",
    ];
    candidates
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
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
}
