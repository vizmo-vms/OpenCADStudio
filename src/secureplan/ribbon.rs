//! The SecurePlan ribbon tab (DSK-06).
//!
//! Every button runs a command, so each is also reachable from the keyboard
//! by typing it at the command line:
//! `SECUREPLANIMPORT`, `SECUREPLANALIGN`, `SECUREPLANAPPLY`,
//! `SECUREPLANCONVERT`, `SECUREPLANOVERLAY`, `SECUREPLANTRUST`, `SECUREPLANREVOKE`,
//! `SECUREPLANDEVORIGINS`, `SECUREPLANUPDATE` and `SECUREPLANAUTOUPDATE`. The command `SECUREPLAN` opens the same actions as
//! a keyboard menu with visible focus (the upstream ribbon has no keyboard
//! focus of its own). Labels are text, never colour alone.

use std::sync::OnceLock;

use crate::modules::{CadModule, IconKind, ModuleEvent, RibbonGroup, RibbonItem, ToolDef};

pub struct SecurePlanModule;

/// The SecurePlan commands, in ribbon order: (command, label, glyph).
pub const COMMANDS: [(&str, &str, &str); 10] = [
    ("SECUREPLANIMPORT", "Import drawing", "⤓"),
    ("SECUREPLANALIGN", "Align", "⌖"),
    ("SECUREPLANAPPLY", "Apply", "✓"),
    ("SECUREPLANCONVERT", "Convert selection", "⇄"),
    ("SECUREPLANOVERLAY", "Design overlay", "◫"),
    ("SECUREPLANTRUST", "Trusted websites", "☰"),
    ("SECUREPLANREVOKE", "Revoke trust", "✕"),
    ("SECUREPLANDEVORIGINS", "Developer origins", "⚙"),
    ("SECUREPLANUPDATE", "Check for updates", "↻"),
    ("SECUREPLANAUTOUPDATE", "Automatic update checks", "⏲"),
];

fn tool(index: usize) -> ToolDef {
    let (id, label, glyph) = COMMANDS[index];
    ToolDef { id, label, icon: IconKind::Glyph(glyph), event: ModuleEvent::Command(id.to_string()) }
}

fn groups() -> &'static [RibbonGroup] {
    static GROUPS: OnceLock<Vec<RibbonGroup>> = OnceLock::new();
    GROUPS.get_or_init(|| {
        vec![
            RibbonGroup {
                title: "Survey drawing",
                tools: vec![RibbonItem::LargeTool(tool(0)), RibbonItem::LargeTool(tool(1)), RibbonItem::LargeTool(tool(2))],
            },
            RibbonGroup { title: "SecurePlan design", tools: vec![RibbonItem::LabeledTool(tool(3)), RibbonItem::LabeledTool(tool(4))] },
            RibbonGroup {
                title: "Trust",
                tools: vec![RibbonItem::LabeledTool(tool(5)), RibbonItem::LabeledTool(tool(6)), RibbonItem::LabeledTool(tool(7))],
            },
            // The About/Help place for SecurePlan CAD's own updates (DSK-07).
            RibbonGroup { title: "SecurePlan CAD", tools: vec![RibbonItem::LabeledTool(tool(8)), RibbonItem::LabeledTool(tool(9))] },
        ]
    })
}

impl CadModule for SecurePlanModule {
    fn id(&self) -> &'static str {
        "secureplan"
    }

    fn title(&self) -> &'static str {
        "SecurePlan"
    }

    fn ribbon_groups(&self) -> &[RibbonGroup] {
        groups()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_secureplan_command_has_a_labelled_button() {
        let mut commands = Vec::new();
        for group in SecurePlanModule.ribbon_groups() {
            for item in &group.tools {
                let (RibbonItem::LargeTool(tool) | RibbonItem::LabeledTool(tool)) = item else { panic!("unexpected ribbon item") };
                assert!(!tool.label.is_empty());
                let ModuleEvent::Command(command) = &tool.event else { panic!("not a command") };
                commands.push(command.clone());
            }
        }
        let expected: Vec<String> = COMMANDS.iter().map(|(command, ..)| command.to_string()).collect();
        assert_eq!(commands, expected);
        // The trust commands are on the tab (user decision, F4a).
        for trust in ["SECUREPLANTRUST", "SECUREPLANREVOKE", "SECUREPLANDEVORIGINS"] {
            assert!(commands.iter().any(|c| c == trust), "{trust}");
        }
        // The manual update check and the automatic-check switch (DSK-07).
        for update in ["SECUREPLANUPDATE", "SECUREPLANAUTOUPDATE"] {
            assert!(commands.iter().any(|c| c == update), "{update}");
        }
    }
}
