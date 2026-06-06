// mDNS service advertisement

use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceInfo};

/// Adjectives for name generation.
const ADJECTIVES: &[&str] = &[
    "swift", "bright", "calm", "bold", "keen", "warm", "cool", "quick", "sharp", "soft", "wild",
    "fair", "brave", "clear", "deep", "fine", "glad", "kind", "rare", "true",
];

/// Animals for name generation.
const ANIMALS: &[&str] = &[
    "fox", "owl", "elk", "jay", "ram", "emu", "yak", "cat", "bat", "bee", "ant", "cod", "hen",
    "koi", "pug", "ray", "eel", "asp", "dab", "gar",
];

/// Colors for name generation.
const COLORS: &[&str] = &[
    "red", "blue", "gold", "jade", "onyx", "rose", "sage", "teal", "plum", "lime",
];

/// Generate an adjective-animal-color name with a digit suffix.
/// Example: "swift-fox-blue7"
/// Validates as a DNS label (lowercase, alphanumeric + hyphen, max 63 chars).
pub fn generate_mdns_name() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();

    let adj = ADJECTIVES[rng.gen_range(0..ADJECTIVES.len())];
    let animal = ANIMALS[rng.gen_range(0..ANIMALS.len())];
    let color = COLORS[rng.gen_range(0..COLORS.len())];
    let digit: u8 = rng.gen_range(0..10);

    let name = format!("{adj}-{animal}-{color}{digit}");

    // Validate DNS label constraints
    debug_assert!(is_valid_dns_label(&name));
    name
}

/// Validate that a string is a valid DNS label.
pub fn is_valid_dns_label(label: &str) -> bool {
    if label.is_empty() || label.len() > 63 {
        return false;
    }
    if label.starts_with('-') || label.ends_with('-') {
        return false;
    }
    label
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// mDNS service handle.
pub struct MdnsService {
    daemon: ServiceDaemon,
    service_type: String,
    fullname: String,
}

impl MdnsService {
    /// Stop the mDNS advertisement.
    pub fn stop(&self) -> Result<()> {
        self.daemon
            .unregister(&self.fullname)
            .context("failed to unregister mDNS service")?;
        Ok(())
    }

    /// Republish the mDNS service (unregister then re-register).
    pub fn republish(&self, name: &str, port: u16) -> Result<()> {
        let _ = self.daemon.unregister(&self.fullname);

        let service_info = ServiceInfo::new(
            &self.service_type,
            name,
            &format!("{name}.local."),
            "",
            port,
            None,
        )
        .context("failed to create service info for republish")?;

        self.daemon
            .register(service_info)
            .context("failed to re-register mDNS service")?;

        Ok(())
    }
}

/// Start advertising via mDNS.
/// Registers `<name>.local` as a `_peckboard._tcp.local.` service.
pub fn start_mdns(name: &str, port: u16) -> Result<MdnsService> {
    let service_type = "_peckboard._tcp.local.";
    let daemon = ServiceDaemon::new().context("failed to create mDNS daemon")?;

    let service_info = ServiceInfo::new(
        service_type,
        name,
        &format!("{name}.local."),
        "",
        port,
        None,
    )
    .context("failed to create mDNS service info")?;

    let fullname = service_info.get_fullname().to_string();

    daemon
        .register(service_info)
        .context("failed to register mDNS service")?;

    tracing::info!("mDNS: advertising {name}.local on port {port}");

    Ok(MdnsService {
        daemon,
        service_type: service_type.to_string(),
        fullname,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_mdns_name_format() {
        for _ in 0..100 {
            let name = generate_mdns_name();
            // Should have exactly two hyphens (adj-animal-colorN)
            let parts: Vec<&str> = name.split('-').collect();
            assert_eq!(parts.len(), 3, "name should have 3 parts: {name}");

            // Last part should end with a digit
            assert!(
                parts[2].ends_with(|c: char| c.is_ascii_digit()),
                "should end with digit: {name}"
            );

            // Should be valid DNS label
            assert!(is_valid_dns_label(&name), "invalid DNS label: {name}");
        }
    }

    #[test]
    fn test_generate_mdns_name_is_lowercase() {
        for _ in 0..50 {
            let name = generate_mdns_name();
            assert_eq!(name, name.to_lowercase(), "name should be lowercase");
        }
    }

    #[test]
    fn test_is_valid_dns_label() {
        assert!(is_valid_dns_label("swift-fox-blue7"));
        assert!(is_valid_dns_label("a"));
        assert!(is_valid_dns_label("abc-def"));
        assert!(is_valid_dns_label("a1b2c3"));

        // Invalid cases
        assert!(!is_valid_dns_label(""));
        assert!(!is_valid_dns_label("-start"));
        assert!(!is_valid_dns_label("end-"));
        assert!(!is_valid_dns_label("UPPER"));
        assert!(!is_valid_dns_label("has space"));
        assert!(!is_valid_dns_label("has.dot"));
        assert!(!is_valid_dns_label(
            &"a".repeat(64) // too long
        ));
    }

    #[test]
    fn test_dns_label_max_length() {
        // Our generated names should always be well under 63 chars
        for _ in 0..100 {
            let name = generate_mdns_name();
            assert!(name.len() <= 63, "name too long: {name} ({})", name.len());
        }
    }
}
