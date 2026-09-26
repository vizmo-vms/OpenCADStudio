//! Shared native HTTP client configuration.
//!
//! Desktop requests use the operating system's certificate verifier so roots
//! installed by administrators, corporate proxies, and security software are
//! honoured without weakening TLS verification.

#![cfg(not(target_arch = "wasm32"))]

use std::time::Duration;
use ureq::tls::{RootCerts, TlsConfig};

pub(crate) fn agent(timeout: Duration) -> ureq::Agent {
    let tls = TlsConfig::builder()
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .tls_config(tls);
    // SecurePlan CAD may reach only its own GitHub Releases, and follows no
    // redirect it has not checked itself (DSK-02).
    #[cfg(feature = "secureplan")]
    let config = config
        .https_only(true)
        .max_redirects(0)
        .middleware(crate::app::secureplan::hardening::enforce_allowlist);
    config.build().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_uses_platform_verifier_without_disabling_tls() {
        let agent = agent(Duration::from_secs(1));
        let tls = agent.config().tls_config();

        assert!(matches!(tls.root_certs(), &RootCerts::PlatformVerifier));
        assert!(!tls.disable_verification());
    }
}
