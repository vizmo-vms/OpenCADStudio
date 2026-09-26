//! The `secureplan-test`-only stdin test driver (BRG-01 smoke tests, T1a).
//!
//! Compiled only with `--features secureplan-test`, which no release
//! workflow enables (the SecurePlan CI checks both). It reads commands from
//! stdin, one per line, and acknowledges each on stdout with
//! `secureplan-test: <reply>`. While it runs, SecurePlan CAD opens no native
//! file dialogs. Commands:
//!
//! - `pair <launch URL>`: deliver the launch URL exactly as the OS hand-off
//!   would, so it goes through the same parsing and trust checks.
//! - `import <path>`: import that DWG or DXF into the most recently opened
//!   SecurePlan drawing, as the Import dialog would.
//! - `align [units=mm|cm|m|in|ft] [scale=<mm per unit>] [turns=0-3]
//!   [origin=<x>,<y>] [anchor=<x>,<y>]`: set the alignment. `scale` (with no
//!   `units`) is a calibration. On an empty survey the page goes to the
//!   origin and `origin`/`anchor` are ignored.
//! - `apply [window=<x0>,<y0>,<x1>,<y1>] [damaged=publish]`: Apply with the
//!   default window (the visible extents plus 2%), or the given one, without
//!   the dialog. `damaged=publish` ticks "Publish without N damaged items",
//!   which a drawing with damaged items needs on every Apply.
//! - `status`: print the state of the most recently opened drawing.
//!
//! Outcomes arrive later as `secureplan-test: event <name> [detail]` lines:
//! `opened edit|view`, `loaded`, `imported <format> <version>`,
//! `import-failed <code>`, `aligned <units>`, `apply-sent`,
//! `applied <plan version>`, `apply-failed <code>`, `closed`, and
//! `status …` / `error <reason>` for driver requests.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};

use iced::Task;

use super::align::{mapping_at, top_left_corner, Alignment, Units};
use super::publish::{default_window, visible_extents, Mapping};
use crate::app::{Message, OpenCADStudio};

/// Printed before anything else, so a harness can tell a test build started.
pub const READY: &str = "secureplan-test: driver ready";

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// A driver command for the application.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Import(PathBuf),
    Align { units: Option<Units>, scale: Option<f64>, turns: u8, origin: Option<[f64; 2]>, anchor: Option<[f64; 2]> },
    Apply { window: Option<[f64; 4]>, publish_damaged: bool },
    Status,
}

struct Inbox {
    sender: Mutex<mpsc::Sender<Command>>,
    receiver: Mutex<Option<mpsc::Receiver<Command>>>,
}

fn inbox() -> &'static Inbox {
    static INBOX: OnceLock<Inbox> = OnceLock::new();
    INBOX.get_or_init(|| {
        let (sender, receiver) = mpsc::channel();
        Inbox { sender: Mutex::new(sender), receiver: Mutex::new(Some(receiver)) }
    })
}

/// The command stream, for the application's subscription (once).
pub fn take_commands() -> Option<mpsc::Receiver<Command>> {
    inbox().receiver.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Whether the driver is running (no native dialogs then).
pub fn active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

fn print(line: &str) {
    println!("secureplan-test: {line}");
    let _ = std::io::stdout().flush();
}

/// Report an outcome to the harness.
pub fn event(name: &str, detail: &str) {
    if active() {
        print(format!("event {name} {detail}").trim_end());
    }
}

/// Start reading stdin commands on a background thread.
pub fn start() {
    ACTIVE.store(true, Ordering::SeqCst);
    println!("{READY}");
    let _ = std::io::stdout().flush();
    std::thread::spawn(|| {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            print(&handle(line.trim()));
        }
    });
}

fn pair(value: &str) -> Option<[f64; 2]> {
    let (x, y) = value.split_once(',')?;
    Some([x.trim().parse().ok().filter(|v: &f64| v.is_finite())?, y.trim().parse().ok().filter(|v: &f64| v.is_finite())?])
}

fn window(value: &str) -> Option<[f64; 4]> {
    let values: Vec<f64> = value.split(',').map(|v| v.trim().parse::<f64>().ok().filter(|v| v.is_finite())).collect::<Option<_>>()?;
    <[f64; 4]>::try_from(values).ok()
}

/// Parse a command line; `Err` says why it is refused.
pub fn parse(line: &str) -> Result<Command, String> {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    let options = || rest.split_whitespace().map(|option| option.split_once('=').ok_or(format!("bad option {option}")));
    match verb {
        "import" if !rest.trim().is_empty() => Ok(Command::Import(PathBuf::from(rest.trim()))),
        "align" => {
            let (mut units, mut scale, mut turns, mut origin, mut anchor) = (None, None, 0u8, None, None);
            for option in options() {
                match option? {
                    ("units", value) => units = Some(Units::parse(value).filter(|u| *u != Units::Unitless).ok_or("bad units")?),
                    ("scale", value) => scale = Some(value.parse::<f64>().ok().filter(|s| *s > 0.0 && s.is_finite()).ok_or("bad scale")?),
                    ("turns", value) => turns = value.parse::<u8>().ok().filter(|t| *t < 4).ok_or("bad turns")?,
                    ("origin", value) => origin = Some(pair(value).ok_or("bad origin")?),
                    ("anchor", value) => anchor = Some(pair(value).ok_or("bad anchor")?),
                    (other, _) => return Err(format!("unknown option {other}")),
                }
            }
            if units.is_none() && scale.is_none() {
                return Err("align needs units= or scale=".into());
            }
            Ok(Command::Align { units, scale, turns, origin, anchor })
        }
        "apply" => {
            let (mut window_cad, mut publish_damaged) = (None, false);
            for option in options() {
                match option? {
                    ("window", value) => window_cad = Some(window(value).ok_or("bad window")?),
                    ("damaged", "publish") => publish_damaged = true,
                    (other, _) => return Err(format!("unknown option {other}")),
                }
            }
            Ok(Command::Apply { window: window_cad, publish_damaged })
        }
        "status" if rest.is_empty() => Ok(Command::Status),
        _ => Err("unknown command".into()),
    }
}

fn handle(line: &str) -> String {
    match line.split_once(' ') {
        Some(("pair", url)) if url.trim().starts_with("secureplan-cad://") => {
            super::deliver_launch(url.trim().to_string());
            return "pair delivered".into();
        }
        Some(("pair", _)) => return "unknown command".into(),
        _ => {}
    }
    match parse(line) {
        Ok(command) => {
            let verb = line.split_whitespace().next().unwrap_or_default().to_string();
            let _ = inbox().sender.lock().unwrap_or_else(|e| e.into_inner()).send(command);
            format!("{verb} queued")
        }
        Err(reason) if reason == "unknown command" => reason,
        Err(reason) => format!("invalid: {reason}"),
    }
}

impl OpenCADStudio {
    /// Carry out a driver command on the most recently opened drawing.
    pub(crate) fn secureplan_driver(&mut self, command: Command) -> Task<Message> {
        let Some((tab_id, index)) = self
            .secureplan
            .sessions
            .bound
            .last()
            .and_then(|bound| Some((bound.tab_id, self.secureplan_tab_index(bound.tab_id)?)))
        else {
            event("error", "no SecurePlan drawing is open");
            return Task::none();
        };
        self.active_tab = index;
        match command {
            Command::Status => {
                let bound = self.secureplan.sessions.by_tab(tab_id).expect("found above");
                let detail = format!(
                    "connected={} mode={} dirty={} modified={} original-pending={} damaged={} aligned={} base={} plan={} busy={}",
                    bound.connected(),
                    if bound.mode == super::session::Mode::Edit { "edit" } else { "view" },
                    self.secureplan_has_unapplied(index),
                    self.secureplan_modified(index),
                    bound.pending_original.is_some(),
                    bound.lost_entities,
                    bound.alignment.is_some(),
                    bound.base_identity.chars().take(12).collect::<String>(),
                    bound.plan_version.map_or("none".to_string(), |v| v.to_string()),
                    bound.busy.map_or("none", |b| b.operation),
                );
                event("status", &detail);
                Task::none()
            }
            Command::Import(path) => {
                if let Err(reason) = self.secureplan_can_edit_survey() {
                    event("error", &reason);
                    return Task::none();
                }
                self.secureplan_import_path(tab_id, &path)
            }
            Command::Align { units, scale, turns, origin, anchor } => {
                let Some(extents) = visible_extents(&self.tabs[index].scene) else {
                    event("error", "the drawing has nothing to align");
                    return Task::none();
                };
                let bound = self.secureplan.sessions.by_tab(tab_id).expect("found above");
                let window = default_window(extents);
                let (units, scale) = match (units, scale) {
                    (Some(units), _) => (units, units.mm_per_unit().unwrap_or(1.0)),
                    (None, Some(scale)) => (Units::Unitless, scale),
                    (None, None) => return Task::none(),
                };
                let mapping = if super::session::survey_is_empty(bound) {
                    mapping_at(window, scale, turns, [0.0, 0.0])
                } else {
                    Mapping {
                        cad_origin: origin.unwrap_or(top_left_corner(window, turns)),
                        anchor_mm: anchor.unwrap_or([0.0, 0.0]),
                        scale_mm_per_cad_unit: scale,
                        quarter_turns: turns,
                    }
                };
                self.secureplan_set_alignment(tab_id, Alignment { units, mapping });
                Task::none()
            }
            Command::Apply { window, publish_damaged } => {
                let allowed = self.secureplan_can_edit_survey().and_then(|()| {
                    self.secureplan.sessions.by_tab(tab_id).map_or(Ok(()), super::session::apply_allowed)
                });
                if let Err(reason) = allowed {
                    event("error", &reason);
                    return Task::none();
                }
                let dialog = self.secureplan_apply_dialog(index);
                let Some(mut dialog) = dialog else {
                    event("error", "align the drawing first");
                    return Task::none();
                };
                if let Some(window) = window {
                    for (field, value) in window.into_iter().enumerate() {
                        dialog.form.set_number(field, value);
                    }
                }
                if publish_damaged {
                    dialog.acknowledge_damaged();
                }
                match dialog.plan() {
                    Ok(plan) => self.secureplan_build_apply(dialog, plan),
                    Err(reason) => {
                        event("apply-failed", &reason);
                        Task::none()
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_delivers_a_launch_and_other_input_is_refused() {
        assert_eq!(handle("pair secureplan-cad://pair?v=1"), "pair delivered");
        assert_eq!(handle("pair https://example.com"), "unknown command");
        assert_eq!(handle("rm -rf /"), "unknown command");
    }

    #[test]
    fn the_driver_imports_aligns_and_applies_without_dialogs() {
        use crate::app::secureplan::session::tests::Harness;
        use crate::app::secureplan::{overlay, testutil};
        let mut h = Harness::new("driver");
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", serde_json::Value::Null, "none", "import");
        let file = h.dir().join("synthetic.dxf");
        std::fs::write(&file, testutil::synthetic_dxf()).unwrap();
        let _ = h.app.secureplan_driver(parse(&format!("import {}", file.display())).unwrap());
        assert!(h.bound().pending_original.is_some());
        let _ = h.app.secureplan_driver(parse("align units=mm").unwrap());
        assert!(h.bound().alignment.is_some());
        h.app.secureplan.dialog = None;
        let _ = h.app.secureplan_driver(parse("apply").unwrap());
        assert!(h.app.secureplan.dialog.is_none(), "no dialog");
        let (request, transfers) = h.receive("applyRequest");
        assert_eq!(transfers.len(), 4, "drawing, original, PDF and snap file");
        assert_eq!(request["cadUnits"], "mm");
        let _ = h.app.secureplan_driver(Command::Status);
    }

    #[test]
    fn import_align_and_apply_commands_parse() {
        assert_eq!(parse("import /tmp/synthetic plan.dxf"), Ok(Command::Import(PathBuf::from("/tmp/synthetic plan.dxf"))));
        assert_eq!(
            parse("align units=ft turns=1 anchor=100,200"),
            Ok(Command::Align { units: Some(Units::Ft), scale: None, turns: 1, origin: None, anchor: Some([100.0, 200.0]) })
        );
        assert_eq!(parse("align scale=20"), Ok(Command::Align { units: None, scale: Some(20.0), turns: 0, origin: None, anchor: None }));
        assert_eq!(parse("apply"), Ok(Command::Apply { window: None, publish_damaged: false }));
        assert_eq!(parse("apply window=0,0,100,50"), Ok(Command::Apply { window: Some([0.0, 0.0, 100.0, 50.0]), publish_damaged: false }));
        assert_eq!(parse("apply damaged=publish"), Ok(Command::Apply { window: None, publish_damaged: true }));
        assert_eq!(parse("status"), Ok(Command::Status));
        for bad in ["align", "align units=furlong", "align units=mm turns=4", "apply window=1,2,3", "import", "status now"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(handle("align units=parsec").starts_with("invalid:"));
    }
}
