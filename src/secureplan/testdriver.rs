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
//! - `apply [window=<x0>,<y0>,<x1>,<y1>] [damaged=publish] [view=model |
//!   view=layout:<name>]`: Apply without the Apply dialog (its progress
//!   dialog shows, and needs no answer). Model space (the
//!   default, or `view=model`) publishes the default window (the visible
//!   extents plus 2%) or the given one; `view=layout:<name>` publishes that
//!   paper layout's whole sheet through its reference viewport. It must come
//!   last: the name is the rest of the line, spaces included, and `window`
//!   does not apply. A layout that cannot be published gives
//!   `apply-failed <reason>`. `damaged=publish` ticks "Publish without N
//!   damaged items", which a drawing with damaged items needs on every Apply.
//! - `status`: print the state of the most recently opened drawing.
//! - `select all|none|layer=<name>`: select every visible model-space
//!   object, none, or those on one layer (the name is the rest of the line).
//! - `convert walls|route`: convert the selection as **Create SecurePlan
//!   walls** or **Create cable route** would (CNV-01, CNV-02).
//! - `export [format=dwg|dxf] [version=AC10nn] [loss=accept] path=<file>`:
//!   answer the export dialog of the web's `exportRequest`: the format and
//!   version (the applied drawing's own by default), "Export without the
//!   objects listed", and the file to write instead of the native Save
//!   dialog. `path=` comes last: the path is the rest of the line.
//!   `export cancel` cancels it.
//!
//! Outcomes arrive later as `secureplan-test: event <name> [detail]` lines:
//! `opened edit|view`, `loaded`, `imported <format> <version>`,
//! `import-failed <code>`, `aligned <units>`, `apply-sent`,
//! `applied <plan version>`, `apply-failed <code>`, `closed`,
//! `selected <n>`, `convert-sent <walls|route> <n> rejected=<n>`,
//! `convert-refused <reason>`, `converted <n>`, `convert-cancelled`,
//! `convert-failed <code>`, `export-ready`, `export-loss <n>`,
//! `exported <format> <version>`, `export-cancelled`, `export-failed <code>`,
//! and `status …` / `error <reason>` for driver requests. No event carries a
//! path, a file name or drawing content.

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
    /// `layout`: publish that paper layout instead of model space.
    Apply { window: Option<[f64; 4]>, publish_damaged: bool, layout: Option<String> },
    Status,
    /// `None`: nothing; `Some(None)`: everything visible; `Some(Some(layer))`: one layer.
    Select(Option<Option<String>>),
    Convert(super::convert::Kind),
    /// Answer the export dialog: save to `path`, or cancel when `None`.
    Export { path: Option<PathBuf>, format: Option<super::session::Format>, version: Option<acadrust::DxfVersion>, accept_loss: bool },
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
            // `view=layout:<name>` comes last; the name may contain spaces.
            let (rest, layout) = match rest.split_once("view=layout:") {
                Some((before, name)) if !name.trim().is_empty() => (before, Some(name.trim().to_string())),
                Some(_) => return Err("bad view".into()),
                None => (rest, None),
            };
            let (mut window_cad, mut publish_damaged) = (None, false);
            for option in rest.split_whitespace().map(|option| option.split_once('=').ok_or(format!("bad option {option}"))) {
                match option? {
                    ("window", value) => window_cad = Some(window(value).ok_or("bad window")?),
                    ("damaged", "publish") => publish_damaged = true,
                    ("view", "model") => {}
                    (other, _) => return Err(format!("unknown option {other}")),
                }
            }
            if layout.is_some() && window_cad.is_some() {
                return Err("window applies to model space only".into());
            }
            Ok(Command::Apply { window: window_cad, publish_damaged, layout })
        }
        "status" if rest.is_empty() => Ok(Command::Status),
        "select" => match rest.trim() {
            "all" => Ok(Command::Select(Some(None))),
            "none" => Ok(Command::Select(None)),
            layer => match layer.strip_prefix("layer=") {
                Some(name) if !name.is_empty() => Ok(Command::Select(Some(Some(name.to_string())))),
                _ => Err("select all, none or layer=<name>".into()),
            },
        },
        "convert" => super::convert::Kind::parse(rest.trim()).map(Command::Convert).ok_or_else(|| "convert walls or route".into()),
        "export" if rest.trim() == "cancel" => Ok(Command::Export { path: None, format: None, version: None, accept_loss: false }),
        "export" => {
            // `path=` comes last; the path may contain spaces.
            let (rest, path) = match rest.split_once("path=") {
                Some((before, path)) if !path.trim().is_empty() => (before, PathBuf::from(path.trim())),
                _ => return Err("export needs path=<file> (last)".into()),
            };
            let (mut format, mut version, mut accept_loss) = (None, None, false);
            for option in rest.split_whitespace().map(|option| option.split_once('=').ok_or(format!("bad option {option}"))) {
                match option? {
                    ("format", "dwg") => format = Some(super::session::Format::Dwg),
                    ("format", "dxf") => format = Some(super::session::Format::Dxf),
                    ("version", value) => version = Some(acadrust::DxfVersion::parse(value).ok_or("bad version")?),
                    ("loss", "accept") => accept_loss = true,
                    (other, _) => return Err(format!("unknown option {other}")),
                }
            }
            if version.is_some() && format.is_none() {
                return Err("version needs format".into());
            }
            Ok(Command::Export { path: Some(path), format, version, accept_loss })
        }
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
            Command::Select(which) => {
                let scene = &mut self.tabs[index].scene;
                scene.deselect_all();
                let count = match which {
                    None => 0,
                    Some(None) => scene.select_all_visible(),
                    Some(Some(layer)) => {
                        let handles: Vec<_> = scene
                            .document
                            .entities()
                            .filter(|e| e.common().owner_handle == scene.document.header.model_space_block_handle && e.common().layer == layer)
                            .map(|e| e.common().handle)
                            .collect();
                        scene.select_entities(&handles);
                        scene.selected.len()
                    }
                };
                event("selected", &count.to_string());
                Task::none()
            }
            Command::Convert(kind) => self.secureplan_open_convert(Some(kind)),
            Command::Export { path: None, .. } => {
                self.secureplan_show_waiting_export();
                if matches!(self.secureplan.dialog, Some(super::ui::Dialog::Export(_))) {
                    self.secureplan_export_cancel();
                } else {
                    event("error", "no export is waiting");
                }
                Task::none()
            }
            Command::Export { path: Some(path), format, version, accept_loss } => {
                match self.secureplan_export_to(path, format, version, accept_loss) {
                    Ok(task) => task,
                    Err(reason) => {
                        event("error", &reason);
                        Task::none()
                    }
                }
            }
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
            Command::Apply { window, publish_damaged, layout } => {
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
                if !dialog.select_view(layout.as_deref()) {
                    event("apply-failed", "the drawing has no layout by that name");
                    return Task::none();
                }
                if let Some(window) = window {
                    dialog.set_window(window);
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
        // Only the progress dialog, which needs no answer.
        assert!(h.app.secureplan.dialog.is_none() && h.app.secureplan.apply_progress.as_ref().is_some_and(|d| !d.finished()), "no dialog to answer");
        let (request, transfers) = h.receive("applyRequest");
        assert_eq!(transfers.len(), 4, "drawing, original, PDF and snap file");
        assert_eq!(request["view"]["kind"], "model");
        assert_eq!(request["cadUnits"], "mm");
        let _ = h.app.secureplan_driver(Command::Status);
    }

    #[test]
    fn the_driver_applies_a_paper_layout() {
        use crate::app::secureplan::session::tests::Harness;
        use crate::app::secureplan::{overlay, testutil};
        let mut h = Harness::new("driver_layout");
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", serde_json::Value::Null, "none", "import");
        let file = h.dir().join("synthetic-layout.dxf");
        std::fs::write(&file, testutil::synthetic_layout_dxf()).unwrap();
        let _ = h.app.secureplan_driver(parse(&format!("import {}", file.display())).unwrap());
        let _ = h.app.secureplan_driver(parse("align units=mm").unwrap());
        h.app.secureplan.dialog = None;
        let _ = h.app.secureplan_driver(parse("apply view=layout:Sheet A1").unwrap());
        // Only the progress dialog, which needs no answer.
        assert!(h.app.secureplan.dialog.is_none() && h.app.secureplan.apply_progress.as_ref().is_some_and(|d| !d.finished()), "no dialog to answer");
        let (request, transfers) = h.receive("applyRequest");
        assert_eq!(request["view"]["kind"], "layout");
        assert_eq!(request["view"]["layoutName"], "Sheet A1");
        assert!(request["view"]["viewportHandle"].as_str().is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_hexdigit())));
        assert_eq!(request["view"].as_object().unwrap().len(), 3, "exactly kind, layoutName and viewportHandle");
        let placement = &request["placement"];
        assert_eq!((placement["widthPt"].as_u64(), placement["heightPt"].as_u64()), (Some(2384), Some(1684)), "the A1 sheet at its size");
        // An empty survey: the sheet's top-left is at the origin.
        let (width_mm, height_mm) = (placement["widthMm"].as_f64().unwrap(), placement["heightMm"].as_f64().unwrap());
        assert!((placement["centerX"].as_f64().unwrap() - width_mm / 2.0).abs() < 1e-6);
        assert!((placement["centerY"].as_f64().unwrap() - height_mm / 2.0).abs() < 1e-6);
        let bytes_of = |key: &str| transfers[&request[key].as_u64().unwrap()].1.clone();
        let snap = bytes_of("snapTransferId");
        assert!(crate::app::secureplan::snap::tests::read(&snap, (2384, 1684)).is_ok_and(|g| !g.segments.is_empty()));
        let pdf = lopdf::Document::load_mem(&bytes_of("pdfTransferId")).unwrap();
        let media = crate::app::secureplan::publish::tests::page_dict(&pdf).get(b"MediaBox").unwrap().as_array().unwrap().len();
        assert_eq!(media, 4);
        assert_eq!(transfers[&request["drawing"]["transferId"].as_u64().unwrap()].1, testutil::synthetic_layout_dxf(), "unedited: the imported bytes");
    }

    #[test]
    fn a_drawing_of_fills_only_applies_through_its_layout() {
        use crate::app::secureplan::session::tests::Harness;
        use crate::app::secureplan::{overlay, testutil};
        use acadrust::entities::{BoundaryEdge, BoundaryPath, Hatch, PolylineEdge};
        use acadrust::types::{Vector2, Vector3};
        // Model space holds one solid hatch and nothing else; the layout's
        // 1:10 viewport shows it.
        let mut scene = crate::scene::Scene::new();
        scene.document.header.insertion_units = 4;
        let mut path = BoundaryPath::new();
        let corners = [[59000.0, -1000.0], [63000.0, -1000.0], [63000.0, 3000.0], [59000.0, 3000.0]].map(|[x, y]| Vector2::new(x, y));
        path.add_edge(BoundaryEdge::Polyline(PolylineEdge::new(corners.to_vec(), true)));
        let mut hatch = Hatch::new();
        hatch.is_solid = true;
        hatch.paths.push(path);
        scene.add_entity(acadrust::EntityType::Hatch(hatch));
        let mut viewport = testutil::plan_viewport((420.0, 300.0), 0.0);
        viewport.view_target = Vector3::new(61000.0, 1000.0, 0.0);
        viewport.view_height = 1800.0;
        testutil::add_layout(&mut scene, vec![viewport]);
        let bytes = crate::io::save_to_bytes(&scene.document, "dxf", acadrust::DxfVersion::AC1032).unwrap();

        let mut h = Harness::new("driver_fills_only");
        h.open(None, overlay::tests::overlay_bytes(&[]), "edit", serde_json::Value::Null, "none", "import");
        let file = h.dir().join("fills-only.dxf");
        std::fs::write(&file, bytes).unwrap();
        let _ = h.app.secureplan_driver(parse(&format!("import {}", file.display())).unwrap());
        let _ = h.app.secureplan_driver(parse("align units=mm").unwrap());
        assert!(h.bound().alignment.is_some(), "a drawing of fills can be aligned");
        h.app.secureplan.dialog = None;
        let _ = h.app.secureplan_driver(parse("apply view=layout:Sheet A1").unwrap());
        let (request, _) = h.receive("applyRequest");
        assert_eq!(request["view"]["kind"], "layout");
    }

    #[test]
    fn the_driver_converts_a_selection_and_exports_to_a_path() {
        use crate::app::secureplan::session::tests::{sample, Harness, BASE};
        use crate::app::secureplan::session::Format;
        use crate::app::secureplan::{export, overlay, testutil};
        use serde_json::json;
        let mut h = Harness::new("driver_convert_export");
        h.survey_empty = false;
        let mut plan = sample("openSession-edit")["cadPlan"].clone();
        let plan_mapping = json!({ "cadOrigin": [0.0, 18000.0], "anchorMm": [0.0, 0.0], "scaleMmPerCadUnit": 1.0, "quarterTurns": 0 });
        plan["mapping"] = plan_mapping.clone();
        h.open(Some(("synthetic.dxf", Format::Dxf, testutil::synthetic_dxf())), overlay::tests::overlay_bytes(&[]), "edit", plan, BASE, "edit");
        let _ = h.app.secureplan_driver(parse("select all").unwrap());
        assert!(!h.app.tabs[h.app.active_tab].scene.selected.is_empty());
        let _ = h.app.secureplan_driver(parse("convert walls").unwrap());
        let (request, _) = h.receive("convertRequest");
        // The outline's four sides, the room's four and the diagonal; the
        // column and the door swing are cut into chords under 150 mm.
        assert_eq!(request["candidates"]["walls"].as_array().unwrap().len(), 9);
        assert!(request["candidates"]["rejected"].as_array().unwrap().iter().all(|r| r["reason"] == "tooShort"));

        let payload = export::tests::sample_payload();
        let drawing_id = h.transfer("plan.dxf", "image/vnd.dxf", &testutil::synthetic_dxf());
        let payload_id = h.transfer("export.json", export::MEDIA_TYPE, payload.to_string().as_bytes());
        h.send(json!({ "type": "exportRequest", "requestId": "x1", "snapshot": payload["snapshot"], "mapping": plan_mapping, "drawingTransferId": drawing_id, "payloadTransferId": payload_id }));
        let file = h.dir().join("driver export.dwg");
        let _ = h.app.secureplan_driver(parse(&format!("export format=dwg version=AC1027 path={}", file.display())).unwrap());
        let (result, _) = h.receive("exportResult");
        assert_eq!((result["status"].as_str(), result["format"].as_str(), result["formatVersion"].as_str()), (Some("written"), Some("dwg"), Some("AC1027")));
        let bytes = std::fs::read(&file).unwrap();
        assert_eq!(&bytes[..6], b"AC1027");
        assert!(crate::io::load_bytes("x.dwg", bytes).unwrap().layers.contains("SECUREPLAN-CAMERA"));
    }

    #[test]
    fn import_align_and_apply_commands_parse() {
        assert_eq!(parse("import /tmp/synthetic plan.dxf"), Ok(Command::Import(PathBuf::from("/tmp/synthetic plan.dxf"))));
        assert_eq!(
            parse("align units=ft turns=1 anchor=100,200"),
            Ok(Command::Align { units: Some(Units::Ft), scale: None, turns: 1, origin: None, anchor: Some([100.0, 200.0]) })
        );
        assert_eq!(parse("align scale=20"), Ok(Command::Align { units: None, scale: Some(20.0), turns: 0, origin: None, anchor: None }));
        assert_eq!(parse("apply"), Ok(Command::Apply { window: None, publish_damaged: false, layout: None }));
        assert_eq!(parse("apply window=0,0,100,50"), Ok(Command::Apply { window: Some([0.0, 0.0, 100.0, 50.0]), publish_damaged: false, layout: None }));
        assert_eq!(parse("apply damaged=publish view=model"), Ok(Command::Apply { window: None, publish_damaged: true, layout: None }));
        assert_eq!(
            parse("apply damaged=publish view=layout:Sheet A1 – Ground floor"),
            Ok(Command::Apply { window: None, publish_damaged: true, layout: Some("Sheet A1 – Ground floor".into()) })
        );
        assert_eq!(parse("status"), Ok(Command::Status));
        assert_eq!(parse("select all"), Ok(Command::Select(Some(None))));
        assert_eq!(parse("select layer=Walls and doors"), Ok(Command::Select(Some(Some("Walls and doors".into())))));
        assert_eq!(parse("convert route"), Ok(Command::Convert(crate::app::secureplan::convert::Kind::Route)));
        assert_eq!(
            parse("export format=dwg version=AC1018 loss=accept path=/tmp/my export.dwg"),
            Ok(Command::Export {
                path: Some(PathBuf::from("/tmp/my export.dwg")),
                format: Some(crate::app::secureplan::session::Format::Dwg),
                version: Some(acadrust::DxfVersion::AC1018),
                accept_loss: true
            })
        );
        assert_eq!(parse("export cancel"), Ok(Command::Export { path: None, format: None, version: None, accept_loss: false }));
        for bad in [
            "align",
            "align units=furlong",
            "align units=mm turns=4",
            "apply window=1,2,3",
            "apply view=layout:",
            "apply view=paper",
            "apply window=0,0,1,1 view=layout:Sheet A1",
            "import",
            "status now",
            "select some",
            "convert rooms",
            "export",
            "export format=dwg",
            "export version=AC1018 path=/tmp/x.dwg",
            "export format=pdf path=/tmp/x.pdf",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(handle("align units=parsec").starts_with("invalid:"));
    }
}
