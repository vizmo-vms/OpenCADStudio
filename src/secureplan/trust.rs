//! Origin trust (BRG-02).
//!
//! A launch from an eligible origin the user has trusted creates a pending
//! pairing silently. A launch from an eligible, untrusted origin asks the user
//! once, showing the exact origin; only one prompt is shown at a time, and
//! after a decline further launches from that origin are ignored for
//! [`DECLINE_COOLDOWN`]. A launch from an ineligible origin opens nothing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::pairing::{is_serialized_origin, LaunchRequest};
use super::settings::Settings;

pub const DECLINE_COOLDOWN: Duration = Duration::from_secs(60);

/// Whether `origin` may pair: `https` always; `http://localhost` and
/// `http://127.0.0.1` (any port) only with the developer setting.
pub fn origin_eligible(origin: &str, developer_loopback_origins: bool) -> bool {
    if !is_serialized_origin(origin) {
        return false;
    }
    let Ok(url) = url::Url::parse(origin) else { return false };
    match url.scheme() {
        "https" => true,
        "http" => developer_loopback_origins && matches!(url.host_str(), Some("localhost" | "127.0.0.1")),
        _ => false,
    }
}

/// What to do with a launch.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Trusted: add the pending pairing now.
    Pair(LaunchRequest),
    /// Untrusted but eligible: the prompt is now showing.
    Prompt,
    /// Ineligible, cooling down, or another origin's prompt is showing.
    Ignore,
}

/// The two prompt buttons; focus starts on the safe one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PromptButton {
    #[default]
    Decline,
    Allow,
}

#[derive(Debug)]
pub struct Prompt {
    pub request: LaunchRequest,
    /// When the launch arrived; its token expires 120 s later regardless of
    /// how long the prompt stays open.
    pub received: Instant,
    pub focus: PromptButton,
}

#[derive(Debug, Default)]
pub struct Trust {
    prompt: Option<Prompt>,
    declined: HashMap<String, Instant>,
}

impl Trust {
    pub fn prompt(&self) -> Option<&Prompt> {
        self.prompt.as_ref()
    }

    pub fn on_launch(&mut self, request: LaunchRequest, settings: &Settings, now: Instant) -> Decision {
        if !origin_eligible(&request.origin, settings.developer_loopback_origins) {
            return Decision::Ignore;
        }
        self.declined.retain(|_, at| now.saturating_duration_since(*at) < DECLINE_COOLDOWN);
        if self.declined.contains_key(&request.origin) {
            return Decision::Ignore;
        }
        if settings.is_trusted(&request.origin) {
            return Decision::Pair(request);
        }
        match &mut self.prompt {
            // A relaunch from the origin being asked about replaces the older
            // launch, so accepting pairs with the newest token.
            Some(prompt) if prompt.request.origin == request.origin => {
                prompt.request = request;
                prompt.received = now;
                Decision::Prompt
            }
            Some(_) => Decision::Ignore,
            None => {
                self.prompt = Some(Prompt { request, received: now, focus: PromptButton::default() });
                Decision::Prompt
            }
        }
    }

    /// The user's answer. Accepting trusts the origin in `settings` (the
    /// caller saves them) and returns the launch to pair with the time it
    /// arrived, so its token keeps its original expiry.
    pub fn answer(&mut self, accept: bool, settings: &mut Settings, now: Instant) -> Option<(LaunchRequest, Instant)> {
        let prompt = self.prompt.take()?;
        if accept {
            settings.trust(&prompt.request.origin);
            Some((prompt.request, prompt.received))
        } else {
            self.declined.insert(prompt.request.origin, now);
            None
        }
    }

    /// Move keyboard focus between the two buttons.
    pub fn move_focus(&mut self) {
        if let Some(prompt) = &mut self.prompt {
            prompt.focus = match prompt.focus {
                PromptButton::Decline => PromptButton::Allow,
                PromptButton::Allow => PromptButton::Decline,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::pairing::tests::launch;
    use crate::app::secureplan::vectors;

    const ORIGIN: &str = "https://secureplan.example";

    #[test]
    fn eligibility_follows_the_vectors() {
        for case in vectors::json("launch-urls.json")["origins"].as_array().unwrap() {
            let origin = case["origin"].as_str().unwrap();
            assert_eq!(origin_eligible(origin, false), case["eligibleDefault"].as_bool().unwrap(), "{origin}");
            assert_eq!(origin_eligible(origin, true), case["eligibleDeveloper"].as_bool().unwrap(), "{origin} (developer)");
        }
    }

    #[test]
    fn trusted_origins_pair_silently_and_others_prompt_once() {
        let now = Instant::now();
        let mut trust = Trust::default();
        let mut settings = Settings::default();
        assert_eq!(trust.on_launch(launch(ORIGIN, 1), &settings, now), Decision::Prompt);
        assert_eq!(trust.prompt().unwrap().focus, PromptButton::Decline, "focus starts on the safe choice");
        // One prompt at a time: another origin is ignored, a relaunch replaces.
        assert_eq!(trust.on_launch(launch("https://other.example", 2), &settings, now), Decision::Ignore);
        assert_eq!(trust.on_launch(launch(ORIGIN, 3), &settings, now), Decision::Prompt);
        assert_eq!(trust.prompt().unwrap().request.pairing.expose(), &[3; 16]);
        let (paired, received) = trust.answer(true, &mut settings, now).expect("accepted");
        assert_eq!(paired.pairing.expose(), &[3; 16]);
        assert_eq!(received, now);
        assert!(settings.is_trusted(ORIGIN));
        assert!(trust.prompt().is_none());
        assert!(matches!(trust.on_launch(launch(ORIGIN, 4), &settings, now), Decision::Pair(_)));
        // Revoking makes the origin ask again.
        settings.revoke(ORIGIN);
        assert_eq!(trust.on_launch(launch(ORIGIN, 5), &settings, now), Decision::Prompt);
    }

    #[test]
    fn the_built_in_origin_pairs_without_a_prompt_and_look_alikes_do_not() {
        use crate::app::secureplan::settings::BUILT_IN_ORIGIN;
        let now = Instant::now();
        let mut trust = Trust::default();
        let mut settings = Settings::default();
        assert!(matches!(trust.on_launch(launch(BUILT_IN_ORIGIN, 1), &settings, now), Decision::Pair(_)));
        assert!(trust.prompt().is_none(), "the built-in origin was prompted for");
        // Removing it changes nothing.
        assert!(!settings.revoke(BUILT_IN_ORIGIN));
        assert!(matches!(trust.on_launch(launch(BUILT_IN_ORIGIN, 2), &settings, now), Decision::Pair(_)));
        // An https look-alike on another port or host still asks.
        for (n, look_alike) in
            ["https://secureplan.vizmo.dev:8443", "https://evil-secureplan.vizmo.dev", "https://secureplan.vizmo.dev.example"]
                .into_iter()
                .enumerate()
        {
            assert_eq!(trust.on_launch(launch(look_alike, 3 + n as u8), &settings, now), Decision::Prompt, "{look_alike}");
            trust.answer(false, &mut settings, now);
        }
        // Another scheme is never eligible: no prompt, nothing pairs, even
        // with the developer setting on.
        for developer in [false, true] {
            settings.developer_loopback_origins = developer;
            assert_eq!(trust.on_launch(launch("http://secureplan.vizmo.dev", 9), &settings, now), Decision::Ignore);
            assert!(trust.prompt().is_none());
        }
    }

    #[test]
    fn a_delayed_approval_keeps_the_launch_time() {
        let launched = Instant::now();
        let mut trust = Trust::default();
        let mut settings = Settings::default();
        assert_eq!(trust.on_launch(launch(ORIGIN, 1), &settings, launched), Decision::Prompt);
        let (_, received) = trust.answer(true, &mut settings, launched + Duration::from_secs(90)).unwrap();
        assert_eq!(received, launched, "approval must not restart the token's lifetime");
    }

    #[test]
    fn a_decline_cools_the_origin_down_for_sixty_seconds() {
        let now = Instant::now();
        let mut trust = Trust::default();
        let mut settings = Settings::default();
        assert_eq!(trust.on_launch(launch(ORIGIN, 1), &settings, now), Decision::Prompt);
        assert!(trust.answer(false, &mut settings, now).is_none());
        assert!(!settings.is_trusted(ORIGIN));
        let almost = now + DECLINE_COOLDOWN - Duration::from_millis(1);
        assert_eq!(trust.on_launch(launch(ORIGIN, 2), &settings, almost), Decision::Ignore);
        assert_eq!(trust.on_launch(launch(ORIGIN, 3), &settings, now + DECLINE_COOLDOWN), Decision::Prompt);
    }

    #[test]
    fn ineligible_origins_open_nothing() {
        let now = Instant::now();
        let mut trust = Trust::default();
        let mut settings = Settings::default();
        settings.trust("http://127.0.0.1:8787");
        assert_eq!(trust.on_launch(launch("http://127.0.0.1:8787", 1), &settings, now), Decision::Ignore);
        assert!(trust.prompt().is_none());
        settings.developer_loopback_origins = true;
        assert!(matches!(trust.on_launch(launch("http://127.0.0.1:8787", 2), &settings, now), Decision::Pair(_)));
        assert_eq!(trust.on_launch(launch("http://192.168.1.2:8787", 3), &settings, now), Decision::Ignore);
    }

    #[test]
    fn focus_moves_between_the_buttons() {
        let mut trust = Trust::default();
        trust.on_launch(launch(ORIGIN, 1), &Settings::default(), Instant::now());
        trust.move_focus();
        assert_eq!(trust.prompt().unwrap().focus, PromptButton::Allow);
        trust.move_focus();
        assert_eq!(trust.prompt().unwrap().focus, PromptButton::Decline);
    }
}
