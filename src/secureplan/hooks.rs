//! The application seam: every hook the upstream app calls under
//! `cfg(feature = "secureplan")`. With no SecurePlan activity they leave the
//! editor exactly as upstream draws and runs it.
//!
//! Hook points in upstream files:
//! - `src/main.rs`: a process started by links alone decides before any
//!   window whether to start, and then starts without the editor window;
//! - `src/app/mod.rs`: the `Message::SecurePlan` variant, the `secureplan`
//!   state field, the window title, and booting without the editor window;
//! - `src/app/update/mod.rs`: `Message::SecurePlan` dispatch, keyboard
//!   capture while a SecurePlan dialog is open, and the prompt window's close
//!   button;
//! - `src/app/view/mod.rs`: the subscription, the prompt-only window, the
//!   viewport overlay layer and the dialog layer above the in-canvas modals;
//! - `src/app/startup.rs`, `src/ui/window/options.rs`,
//!   `src/io/file_association.rs`: no file-association prompt, control or
//!   registration;
//! - `src/ui/ribbon/mod.rs`: the SecurePlan ribbon tab;
//! - `src/app/commands/mod.rs`: SecurePlan commands and the command guard;
//! - `src/io/xref.rs`, `src/io/mod.rs`, `src/scene/model/image_model.rs`,
//!   `src/scene/model/pdf_raster.rs`, `src/scene/text/font_face.rs`,
//!   `src/scene/text/shx.rs`, `src/scene/view/render.rs`,
//!   `src/scene/model/material_model.rs`, `src/scene/centerline.rs`,
//!   `src/app/annotation_data.rs`: the external-resource guard;
//! - `src/io/mod.rs`, `src/app/automation.rs`, `src/main.rs`: diagnostics
//!   without file paths.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use iced::{Element, Subscription, Task};

use super::bridge::{self, Bridge, BridgeEvent};
use super::guards::{command_verb, CommandGuard};
use super::pairing::{self, LaunchRequest};
use super::redact::Redacted;
use super::settings::{self, Settings};
use super::trust::{Decision, PromptButton, Trust};
use super::ui::trust_dialog::{self, DialogKey};
use crate::app::{Message, OpenCADStudio};

/// Messages routed to the SecurePlan integration.
#[derive(Debug, Clone)]
pub enum Msg {
    /// A `secureplan-cad:` launch URL, from the OS hand-off or the test driver.
    Launch(Redacted<String>),
    Bridge(BridgeEvent),
    TrustAnswer(bool),
    DialogKey(DialogKey),
    /// A link-started process has waited as long as its pairings can live.
    ColdStartExpired,
}

/// SecurePlan state held by the application.
#[derive(Debug)]
pub struct State {
    /// Refuses file-writing commands (PLOT, WBLOCK, Save, …) for bound
    /// documents once configured.
    pub command_guard: CommandGuard,
    pub settings: Settings,
    /// Where `settings` are saved (the user's config directory).
    pub settings_path: Option<PathBuf>,
    pub trust: Trust,
    /// The listener; the process-wide one unless a test supplies its own.
    pub bridge: Option<Arc<Bridge>>,
    /// Started by a `secureplan-cad:` link alone: no editor window until a
    /// session opens.
    pub cold_start: bool,
    /// The window showing only the trust prompt while there is no editor window.
    pub prompt_window: Option<iced::window::Id>,
}

impl Default for State {
    fn default() -> Self {
        let settings_path = settings::path();
        Self {
            command_guard: CommandGuard::default(),
            settings: settings_path.as_deref().map(Settings::load_from).unwrap_or_default(),
            settings_path,
            trust: Trust::default(),
            bridge: None,
            cold_start: COLD_START.load(Ordering::SeqCst),
            prompt_window: None,
        }
    }
}

// ── Starting from a link ────────────────────────────────────────────────────

static COLD_START: AtomicBool = AtomicBool::new(false);

/// How long a link-started process waits for a session: a pairing's lifetime
/// plus a handshake begun just before it expired.
pub const COLD_START_WAIT: Duration = Duration::from_secs(pairing::TOKEN_TTL.as_secs() + bridge::HANDSHAKE_TIMEOUT.as_secs());

/// What a process started only by `secureplan-cad:` links does, decided
/// before any window exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdStart {
    /// Every link is invalid or from a website that may never pair: open nothing.
    Exit,
    /// Start without the editor window: a trusted website pairs silently and
    /// the editor appears when its session opens; a new one shows only the
    /// trust prompt.
    Windowless,
}

pub fn cold_start(urls: &[String], settings: &Settings) -> ColdStart {
    let eligible = urls
        .iter()
        .filter_map(|url| pairing::parse_launch_url(url).ok())
        .any(|request| super::trust::origin_eligible(&request.origin, settings.developer_loopback_origins));
    if eligible {
        ColdStart::Windowless
    } else {
        ColdStart::Exit
    }
}

/// For `main`, when the process was started by links alone: whether to start
/// at all. Starting, the application boots without the editor window.
pub fn begin_cold_start(urls: &[String]) -> bool {
    let settings = settings::path().as_deref().map(Settings::load_from).unwrap_or_default();
    let start = cold_start(urls, &settings) == ColdStart::Windowless;
    COLD_START.store(start, Ordering::SeqCst);
    start
}

fn after(wait: Duration, message: Message) -> Task<Message> {
    let (sender, receiver) = iced::futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(wait);
        let _ = sender.send(());
    });
    Task::perform(async move { receiver.await.ok() }, move |_| message.clone())
}

// ── Launch inbox ────────────────────────────────────────────────────────────

struct Inbox {
    sender: Mutex<mpsc::Sender<String>>,
    receiver: Mutex<Option<mpsc::Receiver<String>>>,
}

fn inbox() -> &'static Inbox {
    static INBOX: OnceLock<Inbox> = OnceLock::new();
    INBOX.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        Inbox { sender: Mutex::new(sender), receiver: Mutex::new(Some(receiver)) }
    })
}

/// Hand a launch URL to the running application (OS hand-off, test driver).
pub fn deliver_launch(url: String) {
    let _ = inbox().sender.lock().unwrap_or_else(|e| e.into_inner()).send(url);
}

/// Forward a blocking std receiver into an iced subscription stream.
fn forward<T: Send + 'static>(
    receiver: Option<mpsc::Receiver<T>>,
    wrap: fn(T) -> Message,
) -> impl iced::futures::Stream<Item = Message> {
    iced::stream::channel(16, move |output: iced::futures::channel::mpsc::Sender<Message>| async move {
        if let Some(receiver) = receiver {
            std::thread::spawn(move || {
                use iced::futures::SinkExt;
                let mut output = output;
                for item in receiver {
                    if pollster::block_on(output.send(wrap(item))).is_err() {
                        break;
                    }
                }
            });
        }
        std::future::pending::<()>().await;
    })
}

fn bridge_events() -> impl iced::futures::Stream<Item = Message> {
    forward(bridge::global().and_then(|bridge| bridge.take_events()), |event| {
        Message::SecurePlan(Msg::Bridge(event))
    })
}

fn launches() -> impl iced::futures::Stream<Item = Message> {
    let receiver = inbox().receiver.lock().unwrap_or_else(|e| e.into_inner()).take();
    forward(receiver, |url| Message::SecurePlan(Msg::Launch(url.into())))
}

fn dialog_key(event: iced::Event, _status: iced::event::Status, _window: iced::window::Id) -> Option<Message> {
    match event {
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
            DialogKey::from_key(&key, modifiers).map(|key| Message::SecurePlan(Msg::DialogKey(key)))
        }
        _ => None,
    }
}

impl OpenCADStudio {
    pub(crate) fn secureplan_update(&mut self, msg: Msg) -> Task<Message> {
        match msg {
            Msg::Launch(url) => self.secureplan_launch(url.expose()),
            Msg::TrustAnswer(accept) => self.secureplan_answer_trust(accept),
            Msg::DialogKey(key) => match key {
                DialogKey::Next | DialogKey::Previous => {
                    self.secureplan.trust.move_focus();
                    Task::none()
                }
                DialogKey::Activate => {
                    let accept = self.secureplan.trust.prompt().is_some_and(|p| p.focus == PromptButton::Trust);
                    self.secureplan_answer_trust(accept)
                }
                DialogKey::Cancel => self.secureplan_answer_trust(false),
            },
            Msg::Bridge(event) => self.secureplan_bridge_event(event),
            // Nothing opened in time: leave without ever showing the editor.
            Msg::ColdStartExpired if self.main_window.is_none() => self.exit_app(),
            Msg::ColdStartExpired => Task::none(),
        }
    }

    /// Whether the editor window is still waiting for a session.
    fn secureplan_windowless(&self) -> bool {
        self.secureplan.cold_start && self.main_window.is_none()
    }

    /// The small window that shows only the trust prompt.
    fn secureplan_open_prompt_window(&mut self) -> Task<Message> {
        if self.secureplan.prompt_window.is_some() {
            return Task::none();
        }
        let (id, open) = iced::window::open(iced::window::Settings {
            size: iced::Size::new(560.0, 320.0),
            position: iced::window::Position::Centered,
            resizable: false,
            exit_on_close_request: false,
            ..Default::default()
        });
        self.secureplan.prompt_window = Some(id);
        Task::batch([open.map(|_| Message::Noop), iced::window::gain_focus(id)])
    }

    fn secureplan_close_prompt_window(&mut self) -> Task<Message> {
        match self.secureplan.prompt_window.take() {
            Some(id) => iced::window::close(id),
            None => Task::none(),
        }
    }

    /// The prompt-only window's close button declines, like Escape.
    pub(crate) fn secureplan_window_close_requested(&mut self, id: iced::window::Id) -> Option<Task<Message>> {
        (self.secureplan.prompt_window == Some(id)).then(|| self.secureplan_answer_trust(false))
    }

    /// The view of a SecurePlan-owned window, if `id` is one.
    pub(crate) fn secureplan_window_view(&self, id: iced::window::Id) -> Option<Element<'_, Message>> {
        if self.secureplan.prompt_window != Some(id) {
            return None;
        }
        let base: Element<'_, Message> = iced::widget::Space::new().width(iced::Length::Fill).height(iced::Length::Fill).into();
        Some(match self.secureplan.trust.prompt() {
            Some(prompt) => trust_dialog::view(base, prompt),
            None => base,
        })
    }

    fn secureplan_bridge(&mut self) -> Option<Arc<Bridge>> {
        if self.secureplan.bridge.is_none() {
            self.secureplan.bridge = bridge::global_arc();
            self.secureplan_sync_trust();
        }
        self.secureplan.bridge.clone()
    }

    /// Tell the bridge which origins may pair now. Revoking an origin or
    /// turning the developer setting off drops its pending pairings and
    /// handshakes and closes its sessions (BRG-02).
    fn secureplan_sync_trust(&self) {
        let Some(bridge) = &self.secureplan.bridge else { return };
        let settings = &self.secureplan.settings;
        let allowed = settings
            .trusted_origins
            .iter()
            .filter(|origin| super::trust::origin_eligible(origin, settings.developer_loopback_origins))
            .cloned()
            .collect();
        bridge.set_trusted_origins(allowed);
    }

    fn secureplan_launch(&mut self, url: &str) -> Task<Message> {
        let windowless = self.secureplan_windowless();
        let Ok(request) = pairing::parse_launch_url(url) else {
            self.command_line.push_warning("SecurePlan: ignored an invalid SecurePlan CAD link.");
            return Task::none();
        };
        let received = Instant::now();
        let decision = self.secureplan.trust.on_launch(request, &self.secureplan.settings, received);
        if !windowless {
            if let Decision::Pair(request) = decision {
                self.secureplan_pair(request, received);
            }
            return Task::none();
        }
        // No editor yet: a trusted website pairs silently, a new one gets the
        // prompt on its own; either way the process leaves if no session
        // opens while the pairing can live.
        let shown = match decision {
            Decision::Pair(request) => {
                self.secureplan_pair(request, received);
                Task::none()
            }
            Decision::Prompt => self.secureplan_open_prompt_window(),
            Decision::Ignore => Task::none(),
        };
        Task::batch([shown, after(COLD_START_WAIT, Message::SecurePlan(Msg::ColdStartExpired))])
    }

    fn secureplan_answer_trust(&mut self, accept: bool) -> Task<Message> {
        let closed = self.secureplan_close_prompt_window();
        let settings = &mut self.secureplan.settings;
        let Some((request, received)) = self.secureplan.trust.answer(accept, settings, Instant::now()) else {
            // Declined with no editor open: nothing more will happen.
            if self.secureplan_windowless() {
                return Task::batch([closed, self.exit_app()]);
            }
            return closed;
        };
        self.secureplan_save_settings();
        self.secureplan_pair(request, received);
        closed
    }

    fn secureplan_save_settings(&mut self) {
        let saved = match &self.secureplan.settings_path {
            Some(path) => self.secureplan.settings.save_to(path),
            None => Err(std::io::Error::other("no configuration directory")),
        };
        if saved.is_err() {
            self.command_line.push_error("SecurePlan: the settings could not be saved.");
        }
    }

    fn secureplan_pair(&mut self, request: LaunchRequest, received: Instant) {
        match self.secureplan_bridge() {
            Some(bridge) => {
                self.secureplan_sync_trust();
                bridge.add_pending_received(request, received);
            }
            None => self.command_line.push_error(
                "SecurePlan CAD could not listen on 127.0.0.1:47815–47819. Close other copies of SecurePlan CAD and try again.",
            ),
        }
    }

    fn secureplan_bridge_event(&mut self, event: BridgeEvent) -> Task<Message> {
        match event {
            BridgeEvent::Opened { origin, .. } => {
                self.command_line.push_info(&format!("SecurePlan: connected to {origin}."));
                // The editor appears once there is a session to work in.
                if self.secureplan_windowless() {
                    return Task::batch([self.open_main_window(), self.focus_cmd_input()]);
                }
            }
            BridgeEvent::Closed { .. } => self.command_line.push_info("SecurePlan: disconnected."),
            // Session messages and transfers are handled by the session tasks.
            BridgeEvent::Message { .. } | BridgeEvent::Transfer { .. } => {}
        }
        Task::none()
    }

    pub(crate) fn secureplan_subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![Subscription::run(bridge_events), Subscription::run(launches)];
        if self.secureplan_dialog_open() {
            subscriptions.push(iced::event::listen_with(dialog_key));
        }
        Subscription::batch(subscriptions)
    }

    /// Whether a SecurePlan dialog owns the keyboard.
    pub(crate) fn secureplan_dialog_open(&self) -> bool {
        self.secureplan.trust.prompt().is_some()
    }

    /// Non-entity layer drawn over the drawing viewport, below the viewport's
    /// input layers. `None` draws nothing.
    pub(crate) fn secureplan_viewport_overlay(&self) -> Option<Element<'_, Message>> {
        None
    }

    /// SecurePlan dialogs stacked above the editor and its in-canvas modals.
    pub(crate) fn secureplan_view_layer<'a>(&'a self, base: Element<'a, Message>) -> Element<'a, Message> {
        match self.secureplan.trust.prompt() {
            Some(prompt) => trust_dialog::view(base, prompt),
            None => base,
        }
    }

    /// Run a SecurePlan command, or refuse a guarded one. `None` lets the
    /// command run normally.
    pub(crate) fn secureplan_dispatch(&mut self, command: &str) -> Option<Task<Message>> {
        let tab = self.tabs[self.active_tab].id;
        if let Err(refused) = self.secureplan.command_guard.check(tab, command) {
            self.command_line.push_error(&refused.to_string());
            return Some(Task::none());
        }
        let argument = command.split_whitespace().nth(1);
        match command_verb(command).as_str() {
            // Registration as the .dwg/.dxf opener is not part of SecurePlan CAD (DSK-05).
            "FILEASSOC" => self.command_line.push_error("FILEASSOC is not available in SecurePlan CAD."),
            "SECUREPLANTRUST" => {
                let origins = &self.secureplan.settings.trusted_origins;
                let listing = if origins.is_empty() { "none".to_string() } else { origins.join(", ") };
                self.command_line.push_output(&format!("SecurePlan trusted websites: {listing}"));
                let developer = if self.secureplan.settings.developer_loopback_origins { "on" } else { "off" };
                self.command_line.push_output(&format!("Developer loopback origins: {developer}"));
            }
            "SECUREPLANREVOKE" => match argument {
                Some(origin) if self.secureplan.settings.revoke(origin) => {
                    self.secureplan_save_settings();
                    self.secureplan_sync_trust();
                    self.command_line.push_output(&format!("SecurePlan no longer trusts {origin}."));
                }
                _ => self.command_line.push_error("Usage: SECUREPLANREVOKE <trusted website origin>"),
            },
            "SECUREPLANDEVORIGINS" => match argument.map(str::to_ascii_uppercase).as_deref() {
                Some(value @ ("ON" | "OFF")) => {
                    self.secureplan.settings.developer_loopback_origins = value == "ON";
                    self.secureplan_save_settings();
                    self.secureplan_sync_trust();
                    self.command_line.push_output(&format!(
                        "Developer loopback origins (http://localhost, http://127.0.0.1): {}",
                        value.to_ascii_lowercase()
                    ));
                }
                _ => self.command_line.push_error("Usage: SECUREPLANDEVORIGINS ON|OFF"),
            },
            _ => return None,
        }
        Some(Task::none())
    }

    pub(crate) fn secureplan_window_title(&self) -> String {
        let product = format!("{} {}", super::APP_NAME, super::VERSION);
        match self.tabs.get(self.active_tab) {
            Some(tab) => {
                let dot = if tab.dirty { "● " } else { "" };
                format!("{dot}{product} - {}", tab.tab_display_name())
            }
            None => product,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::secureplan::bridge::tests::{connect_web, next_event, test_bridge, ORIGIN};
    use crate::app::secureplan::pairing::tests::launch;

    fn app_with_drawing() -> OpenCADStudio {
        let mut app = OpenCADStudio::new_for_test();
        app.automation_op(r#"{"op":"new"}"#);
        app.secureplan.settings = Settings::default();
        app.secureplan.settings_path = Some(
            std::env::temp_dir()
                .join(format!("secureplan_hooks_{}_{}", std::process::id(), line!()))
                .join("secureplan.json"),
        );
        app
    }

    fn active_command(app: &OpenCADStudio) -> Option<&'static str> {
        app.tabs[app.active_tab].active_cmd.as_ref().map(|cmd| cmd.name())
    }

    fn launch_url(request: &LaunchRequest) -> String {
        use crate::app::secureplan::channel::b64url_encode;
        format!(
            "secureplan-cad://pair?v=1&origin={}&pairing={}&token={}&survey={}&intent=edit",
            url::form_urlencoded::byte_serialize(request.origin.as_bytes()).collect::<String>(),
            b64url_encode(request.pairing.expose()),
            b64url_encode(request.token.expose()),
            request.survey.expose(),
        )
    }

    #[test]
    fn command_guard_blocks_a_refused_command_for_a_bound_tab() {
        let mut app = app_with_drawing();
        let tab = app.tabs[app.active_tab].id;
        app.secureplan.command_guard.refuse("LINE");
        app.secureplan.command_guard.bind_tab(tab);
        let _ = app.dispatch_command("LINE");
        assert_eq!(active_command(&app), None, "refused command started");
        let _ = app.dispatch_command("_line");
        assert_eq!(active_command(&app), None, "prefixed refused command started");

        // Other commands, and the same command in an unbound tab, still run.
        let _ = app.dispatch_command("CIRCLE");
        let circle = active_command(&app).expect("CIRCLE runs in a bound tab");
        app.secureplan.command_guard.unbind_tab(tab);
        let _ = app.dispatch_command("LINE");
        let line = active_command(&app).expect("LINE runs in an unbound tab");
        assert_ne!(line, circle);
    }

    #[test]
    fn file_associations_can_never_be_registered_or_removed() {
        let mut app = app_with_drawing();
        assert!(!app.pending_startup_modals.contains(&crate::app::ModalKind::AssocPrompt), "startup queued the association prompt");
        app.queue_startup_prompts();
        assert!(!app.pending_startup_modals.contains(&crate::app::ModalKind::AssocPrompt));
        assert_ne!(app.active_modal, Some(crate::app::ModalKind::AssocPrompt));
        let before = app.file_assoc_enabled;
        for command in ["FILEASSOC", "FILEASSOC 1", "FILEASSOC 0", "_fileassoc 0"] {
            let _ = app.dispatch_command(command);
            assert_eq!(active_command(&app), None, "{command} started a prompt");
            assert_eq!(app.file_assoc_enabled, before, "{command} changed the setting");
        }
        // The platform calls themselves refuse, so no other path can register,
        // unregister or install thumbnails either.
        assert!(crate::io::file_association::register_as_handler().is_err());
        assert!(crate::io::file_association::unregister_handler().is_err());
        assert!(pollster::block_on(crate::io::file_association::set_default_app()).is_err());
    }

    #[test]
    fn title_names_the_secureplan_product() {
        let app = app_with_drawing();
        assert!(app.secureplan_window_title().contains("SecurePlan CAD 0.1.0"));
    }

    #[test]
    fn keyboard_trust_prompt_pairs_a_new_origin() {
        let mut app = app_with_drawing();
        let bridge = Arc::new(test_bridge(bridge::PING_INTERVAL));
        let events = bridge.take_events().unwrap();
        app.secureplan.bridge = Some(Arc::clone(&bridge));
        let request = launch(ORIGIN, 60);
        let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
        assert!(app.secureplan_dialog_open(), "an untrusted origin prompts");
        // Keys meant for the drawing are swallowed while the prompt is open.
        let _ = app.update(Message::CommandInput("LINE".into()));
        assert!(app.command_line.input.is_empty());
        let _ = app.update(Message::SecurePlan(Msg::DialogKey(DialogKey::Next)));
        let _ = app.update(Message::SecurePlan(Msg::DialogKey(DialogKey::Activate)));
        assert!(!app.secureplan_dialog_open());
        assert!(app.secureplan.settings.is_trusted(ORIGIN));
        let saved = Settings::load_from(app.secureplan.settings_path.as_ref().unwrap());
        assert!(saved.is_trusted(ORIGIN), "trust persisted");
        let _web = connect_web(bridge.port(), &request).expect("the accepted launch pairs");
        assert!(matches!(next_event(&events), BridgeEvent::Opened { .. }));
        std::fs::remove_dir_all(app.secureplan.settings_path.clone().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn escape_declines_and_nothing_pairs() {
        let mut app = app_with_drawing();
        let bridge = Arc::new(test_bridge(bridge::PING_INTERVAL));
        app.secureplan.bridge = Some(Arc::clone(&bridge));
        let request = launch(ORIGIN, 61);
        let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
        let _ = app.update(Message::SecurePlan(Msg::DialogKey(DialogKey::Activate)));
        assert!(!app.secureplan_dialog_open(), "Enter on the default button declines");
        assert!(!app.secureplan.settings.is_trusted(ORIGIN));
        assert!(connect_web(bridge.port(), &request).is_err());
    }

    #[test]
    fn revoking_by_command_invalidates_a_pending_pairing() {
        let mut app = app_with_drawing();
        let bridge = Arc::new(test_bridge(bridge::PING_INTERVAL));
        app.secureplan.bridge = Some(Arc::clone(&bridge));
        app.secureplan.settings.trust(ORIGIN);
        let request = launch(ORIGIN, 62);
        let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
        assert!(!app.secureplan_dialog_open(), "a trusted origin pairs silently");
        let _ = app.dispatch_command(&format!("SECUREPLANREVOKE {ORIGIN}"));
        assert!(connect_web(bridge.port(), &request).is_err(), "the pending pairing survived the revoke");
        std::fs::remove_dir_all(app.secureplan.settings_path.clone().unwrap().parent().unwrap()).ok();
    }

    #[test]
    fn trust_commands_revoke_and_toggle_the_developer_setting() {
        let mut app = app_with_drawing();
        app.secureplan.settings.trust(ORIGIN);
        let _ = app.dispatch_command("SECUREPLANDEVORIGINS on");
        assert!(app.secureplan.settings.developer_loopback_origins);
        let _ = app.dispatch_command(&format!("SECUREPLANREVOKE {ORIGIN}"));
        assert!(!app.secureplan.settings.is_trusted(ORIGIN));
        let saved = Settings::load_from(app.secureplan.settings_path.as_ref().unwrap());
        assert!(saved.developer_loopback_origins && !saved.is_trusted(ORIGIN));
        std::fs::remove_dir_all(app.secureplan.settings_path.clone().unwrap().parent().unwrap()).ok();
    }
    #[test]
    fn a_link_start_decides_before_any_window() {
        let mut settings = Settings::default();
        let url = |origin: &str| launch_url(&launch(origin, 90));
        assert_eq!(cold_start(&[url(ORIGIN)], &settings), ColdStart::Windowless);
        // A website that may never pair, or no valid link at all: nothing opens.
        assert_eq!(cold_start(&[url("http://secureplan.example")], &settings), ColdStart::Exit);
        assert_eq!(cold_start(&[url("http://localhost:5173")], &settings), ColdStart::Exit);
        assert_eq!(cold_start(&["secureplan-cad://pair?v=1".to_string()], &settings), ColdStart::Exit);
        assert_eq!(cold_start(&[], &settings), ColdStart::Exit);
        settings.developer_loopback_origins = true;
        assert_eq!(cold_start(&[url("http://localhost:5173")], &settings), ColdStart::Windowless);
    }

    fn link_started_app(pairing: u8) -> (OpenCADStudio, Arc<Bridge>, mpsc::Receiver<BridgeEvent>, LaunchRequest) {
        let mut app = app_with_drawing();
        app.secureplan.cold_start = true;
        let bridge = Arc::new(test_bridge(bridge::PING_INTERVAL));
        let events = bridge.take_events().unwrap();
        app.secureplan.bridge = Some(Arc::clone(&bridge));
        (app, bridge, events, launch(ORIGIN, pairing))
    }

    #[test]
    fn a_trusted_link_start_shows_the_editor_only_when_the_session_opens() {
        let (mut app, bridge, events, request) = link_started_app(91);
        app.secureplan.settings.trust(ORIGIN);
        let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
        assert_eq!(app.main_window, None, "the editor opened before the session");
        assert_eq!(app.secureplan.prompt_window, None, "a trusted website was prompted for");
        let _web = connect_web(bridge.port(), &request).expect("the link pairs silently");
        let opened = next_event(&events);
        assert!(matches!(opened, BridgeEvent::Opened { .. }));
        let _ = app.update(Message::SecurePlan(Msg::Bridge(opened)));
        assert!(app.main_window.is_some(), "the editor opens with the session");
    }

    #[test]
    fn a_new_website_link_start_shows_only_the_trust_prompt() {
        // Declined, by button or by closing the prompt's window: nothing else appears.
        for decline in [Message::SecurePlan(Msg::TrustAnswer(false)), Message::WindowCloseRequested(iced::window::Id::unique())] {
            let (mut app, bridge, _events, request) = link_started_app(92);
            let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
            let prompt = app.secureplan.prompt_window.expect("the prompt has its own window");
            assert_eq!(app.main_window, None, "the editor opened behind the prompt");
            assert!(app.secureplan_window_view(prompt).is_some());
            let decline = match decline {
                Message::WindowCloseRequested(_) => Message::WindowCloseRequested(prompt),
                other => other,
            };
            let _ = app.update(decline);
            assert_eq!(app.secureplan.prompt_window, None);
            assert_eq!(app.main_window, None);
            assert!(!app.secureplan.settings.is_trusted(ORIGIN));
            assert!(connect_web(bridge.port(), &request).is_err());
        }
        // Accepted: the prompt goes, and the editor waits for the session.
        let (mut app, bridge, events, request) = link_started_app(93);
        let _ = app.update(Message::SecurePlan(Msg::Launch(launch_url(&request).into())));
        let _ = app.update(Message::SecurePlan(Msg::TrustAnswer(true)));
        assert_eq!(app.secureplan.prompt_window, None);
        assert_eq!(app.main_window, None);
        let _web = connect_web(bridge.port(), &request).expect("the accepted link pairs");
        let opened = next_event(&events);
        let _ = app.update(Message::SecurePlan(Msg::Bridge(opened)));
        assert!(app.main_window.is_some());
        std::fs::remove_dir_all(app.secureplan.settings_path.clone().unwrap().parent().unwrap()).ok();
    }
}
