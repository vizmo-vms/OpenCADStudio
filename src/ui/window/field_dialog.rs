//! Field dialog (FIELD and the ƒ buttons of the attribute definition
//! dialogs): a category and field list on the left, the chosen field's
//! format on the right, a live preview and the field expression below.

use iced::widget::{button, checkbox, column, container, pick_list, row, scrollable, text, text_input, Space};
use iced::{Element, Fill, Length};

use crate::app::Message;
use crate::t;
use crate::ui::style::common::muted_style;
use crate::ui::style::form::{button_style, dialog_button, field_style};
use crate::ui::window::pdf_dialogs::{accent_text, card_style};

/// Where the chosen field goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldTarget {
    /// FIELD: a new multi-line text placed on screen.
    NewText,
    /// The Default of the Attribute Definition dialog.
    AttdefDefault,
    /// The Default of the Edit Attribute Definition dialog.
    AttdefEdit,
}

/// The categories the dialog offers, with their fields (only the ones the
/// application evaluates).
pub const CATEGORIES: &[(&str, &[&str])] = &[
    ("All", &[]),
    ("Date & Time", &["CreateDate", "Date", "PlotDate", "SaveDate"]),
    (
        "Document",
        &["Author", "Comments", "Filename", "Filesize", "HyperlinkBase", "Keywords", "LastSavedBy", "Subject", "Title"],
    ),
    ("Linked", &["Hyperlink"]),
    ("Objects", &["BlockPlaceholder", "Formula", "NamedObject", "Object"]),
    ("Other", &["DieselExpression", "SystemVariable"]),
    (
        "Plot",
        &["DeviceName", "Login", "PageSetupName", "PaperSize", "PlotDate", "PlotOrientation", "PlotScale", "PlotStyleTable"],
    ),
];

/// The fields of a category; "All" lists every field once, sorted.
pub fn fields_of(category: usize) -> Vec<&'static str> {
    if category == 0 {
        let mut all: Vec<&'static str> = CATEGORIES.iter().flat_map(|(_, f)| f.iter().copied()).collect();
        all.sort_unstable_by_key(|s| s.to_ascii_lowercase());
        all.dedup();
        return all;
    }
    CATEGORIES.get(category).map(|(_, f)| f.to_vec()).unwrap_or_default()
}

/// The date formats the dialog lists, in the reference order.
pub const DATE_FORMATS: &[&str] = &[
    "M/d/yyyy", "dddd, MMMM dd, yyyy", "MMMM d, yyyy", "M/d/yy", "yyyy-MM-dd", "d-MMM-yy",
    "M.d.yyyy", "MMM. d, yy", "d MMMM yyyy", "dd.MM.yyyy", "dd/MM/yyyy", "yyyy/MM/dd",
    "yyyy-M-d", "MMMM yy", "MMM-yy", "M/d/yyyy h:mm tt", "M/d/yyyy h:mm:ss tt", "h:mm tt",
    "h:mm:ss tt", "HH:mm", "HH:mm:ss", "%#x", "%#c", "%x", "%c", "%X",
];

pub const TEXT_CASES: &[&str] = &["(none)", "Uppercase", "Lowercase", "First capital", "Title case"];
pub const SIZE_UNITS: &[&str] = &["Bytes", "Kilobytes", "Megabytes"];
pub const NAMED_TYPES: &[&str] = &["Block", "Dimstyle", "Layer", "Linetype", "Tablestyle", "Textstyle", "View"];

/// How a field is formatted, by its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Date,
    Text,
    Filename,
    Filesize,
    SystemVariable,
    Diesel,
    NamedObject,
    Object,
    Formula,
    Hyperlink,
    PlotScale,
    BlockPlaceholder,
}

/// Formula precisions: the current one, then 0 to 8 decimals.
pub const FORMULA_PRECISIONS: &[&str] =
    &["Current precision", "0", "0.0", "0.00", "0.000", "0.0000", "0.00000", "0.000000", "0.0000000", "0.00000000"];

pub fn kind_of(name: &str) -> FieldKind {
    match name {
        "CreateDate" | "Date" | "PlotDate" | "SaveDate" => FieldKind::Date,
        "Filename" => FieldKind::Filename,
        "Filesize" => FieldKind::Filesize,
        "SystemVariable" => FieldKind::SystemVariable,
        "DieselExpression" => FieldKind::Diesel,
        "NamedObject" => FieldKind::NamedObject,
        "Object" => FieldKind::Object,
        "Formula" => FieldKind::Formula,
        "Hyperlink" => FieldKind::Hyperlink,
        "PlotScale" => FieldKind::PlotScale,
        "BlockPlaceholder" => FieldKind::BlockPlaceholder,
        _ => FieldKind::Text,
    }
}

/// The Field dialog's working copy.
#[derive(Debug, Clone)]
pub struct FieldDialogState {
    pub target: FieldTarget,
    pub category: usize,
    pub name: &'static str,
    pub date_format: String,
    pub text_case: usize,
    /// 1 = path only, 2 = file name only, 3 = path and file name.
    pub file_parts: u8,
    pub file_extension: bool,
    pub size_unit: usize,
    pub sysvar: String,
    pub diesel: String,
    pub named_type: usize,
    pub named_names: Vec<(String, codec::Handle)>,
    pub named: Option<usize>,
    /// The object picked for an Object field, its type and properties.
    pub object: Option<(codec::Handle, String)>,
    pub object_props: Vec<&'static str>,
    pub object_prop: Option<&'static str>,
    pub formula: String,
    /// Indexes into codec FORMULA_FORMATS and FORMULA_PRECISIONS.
    pub formula_format: usize,
    pub formula_precision: usize,
    pub hyperlink_text: String,
    pub hyperlink_url: String,
    /// Index into codec PLOT_SCALE_FORMATS.
    pub plot_scale: usize,
    /// The block being edited in the block editor; block placeholders are
    /// only offered there.
    pub placeholder_block: Option<String>,
    /// Index into codec BLOCK_PLACEHOLDER_PROPERTIES.
    pub placeholder_property: usize,
    pub preview: String,
    /// Today in each of DATE_FORMATS, for the Examples list.
    pub examples: Vec<String>,
}

impl FieldDialogState {
    pub fn new(target: FieldTarget) -> Self {
        Self {
            target,
            category: 0,
            name: "Date",
            date_format: "%x".into(),
            text_case: 0,
            file_parts: 3,
            file_extension: true,
            size_unit: 0,
            sysvar: crate::entities::field::SYSVARS[0].into(),
            diesel: String::new(),
            named_type: 2,
            named_names: Vec::new(),
            named: None,
            object: None,
            object_props: Vec::new(),
            object_prop: None,
            formula: String::new(),
            formula_format: 0,
            formula_precision: 0,
            hyperlink_text: String::new(),
            hyperlink_url: String::new(),
            plot_scale: 0,
            placeholder_block: None,
            placeholder_property: 7,
            preview: String::new(),
            examples: Vec::new(),
        }
    }

    /// The field code (without the `%<…>%` wrapper) and the objects it refers to.
    pub fn code(&self) -> (String, Vec<codec::Handle>) {
        let case = |base: String| match self.text_case {
            0 => base,
            n => format!("{base} \\f \"%tc{n}\""),
        };
        match kind_of(self.name) {
            FieldKind::Date => (format!("\\AcVar {} \\f \"{}\"", self.name, self.date_format), vec![]),
            FieldKind::Text => (case(format!("\\AcVar {}", self.name)), vec![]),
            FieldKind::Filename => {
                let bits = self.file_parts + if self.file_extension && self.file_parts & 2 != 0 { 4 } else { 0 };
                // The text case goes first in the same format: "%tc1%fn6".
                let case = match self.text_case {
                    0 => String::new(),
                    n => format!("%tc{n}"),
                };
                (format!("\\AcVar Filename \\f \"{case}%fn{bits}\""), vec![])
            }
            // Bytes are whole; kilo- and megabytes keep two decimals.
            FieldKind::Filesize => (
                match self.size_unit {
                    0 => "\\AcVar Filesize \\f \"%ld%by1\"".to_string(),
                    n => format!("\\AcVar Filesize \\f \"%.2f%by{}\"", n + 1),
                },
                vec![],
            ),
            FieldKind::Formula => {
                let format = codec::fields::FORMULA_FORMATS.get(self.formula_format).map_or("", |f| f.1);
                let precision = match self.formula_precision {
                    0 => String::new(),
                    n => format!("%pr{}", n - 1),
                };
                let (formula, objects) = indexed_object_ids(self.formula.trim());
                let body = format!("\\AcExpr ({formula})");
                match format!("{format}{precision}") {
                    f if f.is_empty() => (body, objects),
                    f => (format!("{body} \\f \"{f}\""), objects),
                }
            }
            FieldKind::Hyperlink => (
                format!("\\AcVar \\href \"{}##{}#0\"", self.hyperlink_url.trim(), self.hyperlink_text),
                vec![],
            ),
            FieldKind::PlotScale => (
                codec::fields::PLOT_SCALE_FORMATS
                    .get(self.plot_scale)
                    .map_or_else(|| "\\AcVar PlotScale".to_string(), |f| f.1.to_string()),
                vec![],
            ),
            FieldKind::BlockPlaceholder => {
                let (_, property, format) = codec::fields::BLOCK_PLACEHOLDER_PROPERTIES[self.placeholder_property];
                // Text properties take the chosen case; the others keep their own format.
                let format = match (format, self.text_case) {
                    ("%tc4", 0) => String::new(),
                    ("%tc4", n) => format!("%tc{n}"),
                    (other, _) => other.to_string(),
                };
                let code = format!("\\AcObjProp.16.2 Object(?BlockRefId,1).{property}");
                if format.is_empty() {
                    (code, vec![])
                } else {
                    (format!("{code} \\f \"{format}\""), vec![])
                }
            }
            FieldKind::SystemVariable => (format!("\\AcVar {}", self.sysvar), vec![]),
            FieldKind::Diesel => (format!("\\AcDiesel {}", self.diesel), vec![]),
            FieldKind::NamedObject => match self.named.and_then(|i| self.named_names.get(i)) {
                Some((_, handle)) => (case("\\AcObjProp Object(%<\\_ObjIdx 0>%).Name".into()), vec![*handle]),
                None => ("\\AcObjProp".into(), vec![]),
            },
            FieldKind::Object => match (&self.object, self.object_prop) {
                (Some((handle, _)), Some(prop)) => {
                    (format!("\\AcObjProp Object(%<\\_ObjIdx 0>%).{prop}"), vec![*handle])
                }
                _ => ("\\AcObjProp".into(), vec![]),
            },
        }
    }

    /// The field can be inserted (an object field needs its object and
    /// property, a named object its name, a DIESEL field an expression).
    pub fn complete(&self) -> bool {
        match kind_of(self.name) {
            FieldKind::NamedObject => self.named.is_some(),
            FieldKind::Object => self.object.is_some() && self.object_prop.is_some(),
            FieldKind::Diesel => !self.diesel.trim().is_empty(),
            FieldKind::Formula => !self.formula.trim().is_empty(),
            FieldKind::Hyperlink => !self.hyperlink_url.trim().is_empty(),
            FieldKind::BlockPlaceholder => self.placeholder_block.is_some(),
            _ => true,
        }
    }
}

/// Table references in a formula are typed as `%<\_ObjId HANDLE>%`; the stored
/// code numbers them (`%<\_ObjIdx n>%`) and lists the objects.
fn indexed_object_ids(formula: &str) -> (String, Vec<codec::Handle>) {
    let mut out = String::new();
    let mut objects: Vec<codec::Handle> = Vec::new();
    let mut rest = formula;
    while let Some(start) = rest.find("%<\\_ObjId ") {
        let after = &rest[start + "%<\\_ObjId ".len()..];
        let Some(end) = after.find(">%") else { break };
        let Ok(value) = u64::from_str_radix(after[..end].trim(), 16) else { break };
        let handle = codec::Handle::new(value);
        let index = objects.iter().position(|h| *h == handle).unwrap_or_else(|| {
            objects.push(handle);
            objects.len() - 1
        });
        out.push_str(&rest[..start]);
        out.push_str(&format!("%<\\_ObjIdx {index}>%"));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    (out, objects)
}

/// The geometric properties a field can show for an object of this type.
pub fn object_properties(entity: &codec::EntityType) -> Vec<&'static str> {
    use codec::EntityType as E;
    match entity {
        E::Line(_) => vec!["Center", "EndPoint", "Length", "StartPoint"],
        E::Circle(_) => vec!["Area", "Center", "Circumference", "Diameter", "Radius"],
        E::Arc(_) => vec!["Center", "Length", "Radius"],
        E::Ellipse(_) => vec!["Area", "Center"],
        E::LwPolyline(_) | E::Polyline2D(_) | E::Spline(_) => vec!["Area", "Length"],
        E::Hatch(_) => vec!["Area"],
        _ => vec![],
    }
}

#[derive(Debug, Clone)]
pub enum FieldDialogMsg {
    Category(usize),
    Name(&'static str),
    DateFormat(String),
    DateExample(usize),
    TextCase(usize),
    FileParts(u8),
    FileExtension(bool),
    SizeUnit(usize),
    SysVar(String),
    Diesel(String),
    NamedType(usize),
    Named(usize),
    SelectObject,
    ObjectProp(&'static str),
    Formula(String),
    FormulaFormat(usize),
    FormulaPrecision(usize),
    Evaluate,
    /// Sum / Average / Count of a cell range, or one Cell, picked in a table.
    TableFunction(&'static str),
    PlaceholderProperty(usize),
    HyperlinkText(String),
    HyperlinkUrl(String),
    BrowseHyperlink,
    PlotScale(usize),
    Help,
    Ok,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pick(usize, String);

impl std::fmt::Display for Pick {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

fn msg(m: FieldDialogMsg) -> Message {
    Message::FieldDialog(m)
}

fn card<'a>(title: String, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(column![text(title.to_uppercase()).size(10).style(accent_text), content.into()].spacing(8))
        .padding([10, 12])
        .width(Fill)
        .style(card_style)
        .into()
}

/// A selectable list: one row per item, the chosen one highlighted.
fn list<'a>(rows: Vec<(String, bool, FieldDialogMsg)>, height: f32) -> Element<'a, Message> {
    let rows = rows.into_iter().map(|(label, on, m)| {
        button(text(label).size(12))
            .on_press(msg(m))
            .style(button_style(on))
            .padding([3, 8])
            .width(Fill)
            .into()
    });
    container(scrollable(iced::widget::Column::with_children(rows.collect::<Vec<Element<'a, Message>>>()).spacing(1)))
        .height(Length::Fixed(height))
        .padding(2)
        .style(|theme: &iced::Theme| field_style(theme, text_input::Status::Active).into_container())
        .into()
}

trait IntoContainer {
    fn into_container(self) -> container::Style;
}

impl IntoContainer for text_input::Style {
    fn into_container(self) -> container::Style {
        container::Style {
            background: Some(self.background),
            border: self.border,
            ..Default::default()
        }
    }
}

fn table_button<'a>(label: String, function: &'static str) -> Element<'a, Message> {
    button(text(label).size(12))
        .on_press(msg(FieldDialogMsg::TableFunction(function)))
        .style(button_style(false))
        .padding([5, 10])
        .into()
}

fn case_list<'a>(state: &FieldDialogState) -> Element<'a, Message> {
    list(
        TEXT_CASES
            .iter()
            .enumerate()
            .map(|(i, c)| (t!(*c).into_owned(), state.text_case == i, FieldDialogMsg::TextCase(i)))
            .collect(),
        130.0,
    )
}

fn format_panel<'a>(state: &'a FieldDialogState) -> Element<'a, Message> {
    let examples = &state.examples;
    match kind_of(state.name) {
        FieldKind::Date => column![
            row![
                text(t!("Date format")).size(12).width(Length::Fixed(96.0)),
                text_input("", &state.date_format)
                    .size(12)
                    .padding([5, 8])
                    .style(field_style)
                    .on_input(|v| msg(FieldDialogMsg::DateFormat(v))),
            ]
            .spacing(8)
            .align_y(iced::Center),
            text(t!("Examples")).size(12),
            list(
                examples
                    .iter()
                    .enumerate()
                    .map(|(i, e)| (e.clone(), DATE_FORMATS.get(i) == Some(&state.date_format.as_str()), FieldDialogMsg::DateExample(i)))
                    .collect(),
                220.0,
            ),
        ]
        .spacing(8)
        .into(),
        FieldKind::Text => column![text(t!("Format")).size(12), case_list(state)].spacing(8).into(),
        FieldKind::Filename => {
            let part = |label: &str, bits: u8| {
                checkbox(state.file_parts == bits)
                    .label(t!(label).into_owned())
                    .text_size(12)
                    .size(14)
                    .on_toggle(move |_| msg(FieldDialogMsg::FileParts(bits)))
            };
            let mut ext = checkbox(state.file_extension)
                .label(t!("Display file extension").into_owned())
                .text_size(12)
                .size(14);
            if state.file_parts & 2 != 0 {
                ext = ext.on_toggle(|v| msg(FieldDialogMsg::FileExtension(v)));
            }
            column![
                part("Filename only", 2),
                part("Path only", 1),
                part("Path and filename", 3),
                ext,
                text(t!("Format")).size(12),
                case_list(state),
            ]
            .spacing(8)
            .into()
        }
        FieldKind::Filesize => column![
            text(t!("Format")).size(12),
            list(
                SIZE_UNITS
                    .iter()
                    .enumerate()
                    .map(|(i, u)| (t!(*u).into_owned(), state.size_unit == i, FieldDialogMsg::SizeUnit(i)))
                    .collect(),
                90.0,
            ),
        ]
        .spacing(8)
        .into(),
        FieldKind::SystemVariable => column![
            text(t!("System variable")).size(12),
            list(
                crate::entities::field::SYSVARS
                    .iter()
                    .map(|v| (v.to_string(), state.sysvar == *v, FieldDialogMsg::SysVar(v.to_string())))
                    .collect(),
                250.0,
            ),
        ]
        .spacing(8)
        .into(),
        FieldKind::Diesel => column![
            text(t!("Diesel expression")).size(12),
            text_input("$(getvar,dwgname)", &state.diesel)
                .size(12)
                .padding([5, 8])
                .style(field_style)
                .on_input(|v| msg(FieldDialogMsg::Diesel(v))),
        ]
        .spacing(8)
        .into(),
        FieldKind::NamedObject => column![
            row![
                text(t!("Named object type")).size(12).width(Length::Fixed(120.0)),
                pick_list(
                    Some(Pick(state.named_type, t!(NAMED_TYPES[state.named_type]).into_owned())),
                    NAMED_TYPES.iter().enumerate().map(|(i, n)| Pick(i, t!(*n).into_owned())).collect::<Vec<_>>(),
                    |p: &Pick| p.1.clone(),
                )
                .on_select(|p| msg(FieldDialogMsg::NamedType(p.0)))
                .text_size(12)
                .padding([5, 8])
                .width(Fill),
            ]
            .spacing(8)
            .align_y(iced::Center),
            text(t!("Name")).size(12),
            list(
                state
                    .named_names
                    .iter()
                    .enumerate()
                    .map(|(i, (n, _))| (n.clone(), state.named == Some(i), FieldDialogMsg::Named(i)))
                    .collect(),
                150.0,
            ),
            text(t!("Format")).size(12),
            case_list(state),
        ]
        .spacing(8)
        .into(),
        FieldKind::Formula => {
            let formats: Vec<Pick> = codec::fields::FORMULA_FORMATS
                .iter()
                .enumerate()
                .map(|(i, (n, _))| Pick(i, t!(*n).into_owned()))
                .collect();
            let precisions: Vec<Pick> =
                FORMULA_PRECISIONS.iter().enumerate().map(|(i, n)| Pick(i, t!(*n).into_owned())).collect();
            column![
                row![
                    text(t!("Format")).size(12).width(Length::Fixed(80.0)),
                    pick_list(Some(formats[state.formula_format].clone()), formats.clone(), |p: &Pick| p.1.clone())
                        .on_select(|p| msg(FieldDialogMsg::FormulaFormat(p.0)))
                        .text_size(12)
                        .padding([5, 8])
                        .width(Fill),
                ]
                .spacing(8)
                .align_y(iced::Center),
                row![
                    text(t!("Precision")).size(12).width(Length::Fixed(80.0)),
                    pick_list(
                        Some(precisions[state.formula_precision].clone()),
                        precisions.clone(),
                        |p: &Pick| p.1.clone(),
                    )
                    .on_select(|p| msg(FieldDialogMsg::FormulaPrecision(p.0)))
                    .text_size(12)
                    .padding([5, 8])
                    .width(Fill),
                ]
                .spacing(8)
                .align_y(iced::Center),
            ]
            .spacing(8)
            .into()
        }
        FieldKind::Hyperlink => column![
            row![
                text(t!("Text to display")).size(12).width(Length::Fixed(120.0)),
                text_input("", &state.hyperlink_text)
                    .size(12)
                    .padding([5, 8])
                    .style(field_style)
                    .on_input(|v| msg(FieldDialogMsg::HyperlinkText(v))),
            ]
            .spacing(8)
            .align_y(iced::Center),
            row![
                text(t!("Address")).size(12).width(Length::Fixed(120.0)),
                text_input("https://", &state.hyperlink_url)
                    .size(12)
                    .padding([5, 8])
                    .font(iced::Font::MONOSPACE)
                    .style(field_style)
                    .on_input(|v| msg(FieldDialogMsg::HyperlinkUrl(v))),
                button(text("...").size(12))
                    .on_press(msg(FieldDialogMsg::BrowseHyperlink))
                    .style(button_style(false))
                    .padding([5, 10]),
            ]
            .spacing(8)
            .align_y(iced::Center),
        ]
        .spacing(8)
        .into(),
        FieldKind::BlockPlaceholder => match &state.placeholder_block {
            None => column![text(t!("Only accessible in the block editor")).size(12).style(muted_style)].into(),
            Some(block) => {
                let (_, _, format) = codec::fields::BLOCK_PLACEHOLDER_PROPERTIES[state.placeholder_property];
                let mut panel = column![
                    row![
                        text(t!("Block name")).size(12).width(Length::Fixed(110.0)),
                        container(text(block.clone()).size(12).style(muted_style)).padding([5, 8]).width(Fill),
                    ]
                    .spacing(8)
                    .align_y(iced::Center),
                    text(t!("Block reference property")).size(12),
                    list(
                        codec::fields::BLOCK_PLACEHOLDER_PROPERTIES
                            .iter()
                            .enumerate()
                            .map(|(i, (label, _, _))| {
                                (t!(*label).into_owned(), state.placeholder_property == i, FieldDialogMsg::PlaceholderProperty(i))
                            })
                            .collect(),
                        150.0,
                    ),
                ]
                .spacing(8);
                if format == "%tc4" {
                    panel = panel.push(text(t!("Format")).size(12)).push(case_list(state));
                }
                panel.into()
            }
        },
        FieldKind::PlotScale => column![
            text(t!("Format")).size(12),
            list(
                codec::fields::PLOT_SCALE_FORMATS
                    .iter()
                    .enumerate()
                    .map(|(i, (n, _))| (t!(*n).into_owned(), state.plot_scale == i, FieldDialogMsg::PlotScale(i)))
                    .collect(),
                170.0,
            ),
        ]
        .spacing(8)
        .into(),
        FieldKind::Object => column![
            row![
                text(t!("Object type")).size(12).width(Length::Fixed(96.0)),
                container(
                    text(state.object.as_ref().map(|(_, t)| t.clone()).unwrap_or_default()).size(12).style(muted_style)
                )
                .padding([5, 8])
                .width(Fill),
                button(text(t!("Select object")).size(12))
                    .on_press(msg(FieldDialogMsg::SelectObject))
                    .style(button_style(false))
                    .padding([5, 10]),
            ]
            .spacing(8)
            .align_y(iced::Center),
            text(t!("Property")).size(12),
            list(
                state
                    .object_props
                    .iter()
                    .map(|p| (t!(*p).into_owned(), state.object_prop == Some(*p), FieldDialogMsg::ObjectProp(p)))
                    .collect(),
                180.0,
            ),
        ]
        .spacing(8)
        .into(),
    }
}

pub fn view<'a>(
    state: &'a FieldDialogState,
    sizing: crate::ui::modal::ModalSizing,
) -> Element<'a, Message> {
    let categories: Vec<Pick> = CATEGORIES.iter().enumerate().map(|(i, (c, _))| Pick(i, t!(*c).into_owned())).collect();
    let left = card(
        t!("Field").into_owned(),
        column![
            row![
                text(t!("Category")).size(12).width(Length::Fixed(70.0)),
                pick_list(Some(categories[state.category].clone()), categories.clone(), |p: &Pick| p.1.clone())
                    .on_select(|p| msg(FieldDialogMsg::Category(p.0)))
                    .text_size(12)
                    .padding([5, 8])
                    .width(Fill),
            ]
            .spacing(8)
            .align_y(iced::Center),
            list(
                fields_of(state.category)
                    .into_iter()
                    .map(|n| (n.to_string(), state.name == n, FieldDialogMsg::Name(n)))
                    .collect(),
                330.0,
            ),
        ]
        .spacing(8),
    );
    let preview = card(
        t!("Preview").into_owned(),
        container(text(if state.preview.is_empty() { "----".to_string() } else { state.preview.clone() }).size(15))
            .padding([6, 8])
            .width(Fill),
    );
    let right = match kind_of(state.name) {
        FieldKind::Formula => column![
            card(
                t!("Formula").into_owned(),
                column![
                    text_input("(12+3)*2", &state.formula)
                        .size(12)
                        .padding([8, 8])
                        .font(iced::Font::MONOSPACE)
                        .style(field_style)
                        .on_input(|v| msg(FieldDialogMsg::Formula(v))),
                    row![
                        table_button(t!("Average").into_owned(), "Average"),
                        table_button(t!("Sum").into_owned(), "Sum"),
                        table_button(t!("Count").into_owned(), "Count"),
                        table_button(t!("Cell").into_owned(), "Cell"),
                        Space::new().width(Fill),
                        button(text(t!("Evaluate")).size(12))
                            .on_press(msg(FieldDialogMsg::Evaluate))
                            .style(button_style(false))
                            .padding([5, 14]),
                    ]
                    .spacing(6),
                ]
                .spacing(8),
            ),
            card(t!("Format").into_owned(), format_panel(state)),
            preview,
        ]
        .spacing(10),
        FieldKind::Hyperlink => column![card(t!("Hyperlink").into_owned(), format_panel(state)), preview].spacing(10),
        FieldKind::BlockPlaceholder => column![card(t!("Block placeholder").into_owned(), format_panel(state)), preview].spacing(10),
        _ => column![card(t!("Format").into_owned(), format_panel(state)), preview].spacing(10),
    };
    let (code, _) = state.code();
    let expression = column![
        text(t!("Field expression")).size(12),
        container(text(format!("%<{code}>%")).size(12).font(iced::Font::MONOSPACE).style(muted_style))
            .padding([5, 8])
            .width(Fill)
            .style(|theme: &iced::Theme| field_style(theme, text_input::Status::Disabled).into_container()),
    ]
    .spacing(4);
    let mut insert = button(text(t!("Insert")).size(12))
    .style(button_style(true))
    .padding([6, 16]);
    if state.complete() {
        insert = insert.on_press(msg(FieldDialogMsg::Ok));
    }
    let footer = row![
        button(text("?").size(12)).on_press(msg(FieldDialogMsg::Help)).style(button_style(false)).padding([5, 11]),
        Space::new().width(Fill),
        dialog_button(t!("Cancel"), Message::CloseModal, false),
        insert,
    ]
    .spacing(6)
    .align_y(iced::Center);
    column![
        row![container(left).width(Length::Fixed(240.0)), right].spacing(10),
        expression,
        footer,
    ]
    .spacing(10)
    .padding([10, 12])
    .width(sizing.width)
    .into()
}
