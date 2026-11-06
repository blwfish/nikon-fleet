// ── Editing an edited copy in the snapshot viewer ─────────────────────────
//
// A snapshot read from a camera is only ever viewed. "Edit a copy…" in the snapshot list makes an editable copy
// (`nikon_fleet::snapshot_edit`) and opens it in the same viewer, where the controls appear: a choice becomes a
// drop-down, a position on a scale a slider, a number a text box, a switch a check box; the owner strings, switches,
// IPTC presets and the FTP profile list get their own section. Every change is saved at once, through the
// library's edit functions, which do all the checking (this file decides nothing about what is allowed); the
// viewer then reloads the file, so what is shown is always what is stored. The rules and the file handling are in
// the library and the CLI's `edit` commands use the same ones.

use super::*;
use nikon_fleet::ops::edit_snapshot::{create_edit_copy, edit_in_place};
use nikon_fleet::ops::labels::Lookup;
use nikon_fleet::snapshot::DerivedFrom;
use nikon_fleet::snapshot_edit::{self as edit, Control, OwnerView};

/// What the viewer needs to draw controls for an edited copy. Built where the file is read, from the same snapshot
/// the rows are rendered from.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EditModel {
    pub controls: BTreeMap<String, ControlView>,
    /// `None`: the snapshot has no extended section.
    pub owner: Option<OwnerView>,
    pub derived: DerivedFrom,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ControlView {
    pub control: Control,
    /// For a choice, the text of each of its values, in the order of the control's `values`.
    pub options: Vec<String>,
}

impl EditModel {
    /// `None` for a snapshot read from a camera: it is not edited, so it gets no controls.
    pub(crate) fn build(s: &Snapshot, labels: &Labels) -> Option<EditModel> {
        let derived = s.derived_from.clone()?;
        let controls = s
            .properties
            .iter()
            .map(|(name, entry)| {
                let control = edit::property_control(s, name);
                let options = match &control {
                    Control::Choice { values, .. } => values.iter().map(|v| option_text(labels, entry.code, name, *v)).collect(),
                    _ => Vec::new(),
                };
                (name.clone(), ControlView { control, options })
            })
            .collect();
        Some(EditModel { controls, owner: edit::owner_view(s), derived })
    }

    /// The property was changed in this copy (it is in the edit log).
    pub(crate) fn property_edited(&self, name: &str) -> bool {
        let key = format!("property:{name}");
        self.derived.edits.iter().any(|e| e.item == key)
    }
}

/// A choice's value as the drop-down shows it: its name and number when the SDK's names know it, else the number.
fn option_text(labels: &Labels, code: u32, name: &str, value: i64) -> String {
    match labels.lookup_value(code, Some(name), value) {
        Lookup::Label { text, .. } => format!("{text} ({value})"),
        _ => value.to_string(),
    }
}

/// A change asked for in the viewer. Each is applied by one library function.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ViewEdit {
    /// `name` is the property's full name, as the row has it (the text field keeps its text under it).
    Property { name: String, text: String },
    OwnerString { code: u16, text: String },
    Switch { code: u16, value: u8 },
    Iptc { slot: u32, field: String, text: String },
    FtpDelete(usize),
    FtpMove { from: usize, to: usize },
}

pub(crate) fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
}

/// Apply `edit` to the edited copy `file` in `snapshots/` and keep the result, whole or not at all. The error is for
/// a person and names the reason; nothing was written when there is one.
pub(crate) fn apply_view_edit(data_dir: &Path, file: &str, req: &ViewEdit, now: &str) -> Result<edit::Applied, String> {
    let snaps = data_dir.join("snapshots");
    let done = match req {
        ViewEdit::Property { name, text } => edit_in_place(&snaps, file, |s| edit::set_property_text(s, name, text, now).map(|(_, a)| a)),
        ViewEdit::OwnerString { code, text } => edit_in_place(&snaps, file, |s| edit::set_owner_string(s, *code, text, now)),
        ViewEdit::Switch { code, value } => edit_in_place(&snaps, file, |s| edit::set_switch(s, *code, *value, now)),
        ViewEdit::Iptc { slot, field, text } => edit_in_place(&snaps, file, |s| edit::set_iptc_field(s, *slot, field, text, now)),
        ViewEdit::FtpDelete(index) => edit_in_place(&snaps, file, |s| edit::ftp_delete(s, *index, now)),
        ViewEdit::FtpMove { from, to } => edit_in_place(&snaps, file, |s| edit::ftp_move(s, *from, *to, now)),
    };
    done.map_err(|e| e.to_string())
}

/// Make an editable copy of `snapshots/<file>`; the new file's name. The original is only read.
pub(crate) fn make_edit_copy(data_dir: &Path, file: &str, now: &str) -> Result<String, String> {
    let snaps = data_dir.join("snapshots");
    let source = nikon_fleet::ops::paths::resolve_existing(&snaps, file).map_err(|e| e.to_string())?;
    create_edit_copy(&snaps, &source, None, now).map(|c| c.file).map_err(|e| e.to_string())
}

/// The editing state that lives in a viewer between frames: text being typed, a slider being dragged, a delete that
/// has been asked for once. Cleared whenever the file is reloaded, so it never outlives the stored value it was
/// started from.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct EditUi {
    texts: BTreeMap<String, String>,
    sliders: BTreeMap<String, u64>,
    /// The FTP profile whose delete button was pressed once; a second press deletes it.
    confirm_delete: Option<usize>,
}

impl EditUi {
    pub(crate) fn clear(&mut self) {
        *self = EditUi::default();
    }

    /// Forget the text typed for one field, so it shows the stored value again.
    pub(crate) fn forget(&mut self, key: &str) {
        self.texts.remove(key);
        self.sliders.remove(key);
    }

    #[cfg(test)]
    pub(crate) fn has_text(&self, key: &str) -> bool {
        self.texts.contains_key(key)
    }
}

/// The key under which a request's field keeps its text, so a refused edit can show the stored value again.
pub(crate) fn field_key(req: &ViewEdit) -> Option<String> {
    Some(match req {
        ViewEdit::Property { name, .. } => format!("property:{name}"),
        ViewEdit::OwnerString { code, .. } => format!("string:{code:#06x}"),
        ViewEdit::Iptc { slot, field, .. } => format!("iptc:{slot}:{field}"),
        ViewEdit::Switch { .. } | ViewEdit::FtpDelete(_) | ViewEdit::FtpMove { .. } => return None,
    })
}

// ── drawing ──────────────────────────────────────────────────────────────

/// A text box that commits when it loses focus (Enter, Tab or a click elsewhere) and gives up on Escape. The text
/// typed so far is kept in `ui_state` under `key`; `stored` is what the file holds. Returns the new text when it
/// differs from `stored`.
fn text_field(ui: &mut egui::Ui, state: &mut EditUi, key: &str, stored: &str, enabled: bool, width: f32) -> Option<String> {
    let buf = state.texts.entry(key.to_string()).or_insert_with(|| stored.to_string());
    let resp = ui.add_enabled(enabled, egui::TextEdit::singleline(buf).desired_width(width));
    if resp.lost_focus() {
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.texts.remove(key);
            return None;
        }
        let typed = state.texts.get(key).cloned().unwrap_or_default();
        if typed != stored {
            return Some(typed);
        }
        state.texts.remove(key);
    }
    None
}

/// The control for one property's value cell. `None` when nothing was committed this frame.
pub(crate) fn property_cell(ui: &mut egui::Ui, state: &mut EditUi, name: &str, view: &ControlView, enabled: bool) -> Option<ViewEdit> {
    let key = format!("property:{name}");
    let set = |text: String| Some(ViewEdit::Property { name: name.to_string(), text });
    match &view.control {
        Control::Choice { values, current } => {
            let shown = current.and_then(|c| values.iter().position(|v| *v == c)).map_or("(no current value)".to_string(), |i| view.options[i].clone());
            let mut chosen = None;
            ui.add_enabled_ui(enabled, |ui| {
                egui::ComboBox::from_id_salt(&key).selected_text(shown).show_ui(ui, |ui| {
                    for (v, text) in values.iter().zip(&view.options) {
                        if ui.selectable_label(*current == Some(*v), text).clicked() && *current != Some(*v) {
                            chosen = Some(*v);
                        }
                    }
                });
            });
            chosen.and_then(|v| set(v.to_string()))
        }
        Control::Scale { lower, upper, steps, index } => {
            let mut at = *state.sliders.get(&key).or(index.as_ref()).unwrap_or(&0);
            let (lo, hi, n) = (*lower, *upper, *steps);
            let slider = egui::Slider::new(&mut at, 0..=n - 1)
                .custom_formatter(move |p, _| edit_scale_text(lo, hi, n, p as u64))
                .custom_parser(|s| s.trim().parse::<f64>().ok().map(|_| 0.0))
                .show_value(true);
            let resp = ui.add_enabled(enabled, slider);
            if resp.changed() {
                state.sliders.insert(key.clone(), at);
            }
            // Committed when the drag ends, or on a click that was not a drag; not on every frame of a drag.
            if (resp.drag_stopped() || (resp.changed() && !resp.dragged())) && Some(at) != *index {
                state.sliders.remove(&key);
                return set(edit_scale_text(lo, hi, n, at));
            }
            if !resp.dragged() && !resp.has_focus() && state.sliders.get(&key) == index.as_ref() {
                state.sliders.remove(&key);
            }
            None
        }
        Control::Continuous { current, .. } => text_field(ui, state, &key, &current.map_or(String::new(), |c| c.to_string()), enabled, 90.0).and_then(set),
        Control::Whole { current } => text_field(ui, state, &key, &current.to_string(), enabled, 90.0).and_then(set),
        Control::Real { current } => text_field(ui, state, &key, &current.to_string(), enabled, 90.0).and_then(set),
        Control::Switch { current } => {
            let mut on = *current;
            let resp = ui.add_enabled(enabled, egui::Checkbox::without_text(&mut on));
            if resp.changed() { set(on.to_string()) } else { None }
        }
        Control::Locked { reason } => {
            // No control: the stored value is shown by the caller, and this says why it cannot be changed.
            ui.label(egui::RichText::new("🔒").weak()).on_hover_text(reason);
            None
        }
    }
}

/// The number a position on a scale is, as the text `set_property_text` takes.
fn edit_scale_text(lower: f64, upper: f64, steps: u64, index: u64) -> String {
    let v = edit::scale_value(lower, upper, steps, index);
    let s = format!("{v:.6}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A button on one row of the FTP profile list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowPress {
    Up,
    Down,
    Delete,
}

/// What pressing `press` on row `i` of a list of `len` does. A move is one press. A delete needs two on the same row,
/// the second asking for it: the first only arms it (and arms no other row), and any other press, or a reload, disarms
/// it. A move that goes off either end, or a row that is not there, does nothing.
pub(crate) fn ftp_row_press(state: &mut EditUi, i: usize, len: usize, press: RowPress) -> Option<ViewEdit> {
    if i >= len {
        return None;
    }
    if press != RowPress::Delete {
        state.confirm_delete = None;
    }
    match press {
        RowPress::Up if i > 0 => Some(ViewEdit::FtpMove { from: i, to: i - 1 }),
        RowPress::Down if i + 1 < len => Some(ViewEdit::FtpMove { from: i, to: i + 1 }),
        RowPress::Up | RowPress::Down => None,
        RowPress::Delete if state.confirm_delete == Some(i) => {
            state.confirm_delete = None;
            Some(ViewEdit::FtpDelete(i))
        }
        RowPress::Delete => {
            state.confirm_delete = Some(i);
            None
        }
    }
}

/// The section for the owner data of an edited copy: strings, switches, IPTC presets and the FTP profile list.
pub(crate) fn draw_owner_editor(ui: &mut egui::Ui, file: &str, owner: &OwnerView, state: &mut EditUi, enabled: bool) -> Option<ViewEdit> {
    let mut out = None;
    let warn = egui::Color32::from_rgb(200, 120, 0);
    ui.separator();
    ui.label(egui::RichText::new("Owner data (edit)").strong());
    egui::Grid::new((file, "owner_edit")).num_columns(3).striped(true).show(ui, |ui| {
        for s in &owner.strings {
            ui.label(egui::RichText::new(s.name).monospace());
            match &s.text {
                Some(stored) => {
                    let key = format!("string:{:#06x}", s.code);
                    if let Some(text) = text_field(ui, state, &key, stored, enabled, 260.0) {
                        out = Some(ViewEdit::OwnerString { code: s.code, text });
                    }
                    let typed = state.texts.get(&key).map_or(stored.chars().count(), |t| t.chars().count());
                    let over = typed > s.max_chars;
                    ui.label(egui::RichText::new(format!("{typed}/{} characters", s.max_chars)).small().color(if over { warn } else { egui::Color32::GRAY }));
                }
                None => {
                    ui.label(egui::RichText::new("-").weak());
                    ui.label(egui::RichText::new(s.note.as_deref().unwrap_or("")).small().weak());
                }
            }
            ui.end_row();
            if let (Some(_), Some(note)) = (&s.text, &s.note) {
                ui.label("");
                ui.label(egui::RichText::new(note).small().color(warn));
                ui.label("");
                ui.end_row();
            }
        }
        for w in &owner.switches {
            ui.label(egui::RichText::new(w.name).monospace());
            match w.value {
                Some(current) => {
                    let mut chosen = None;
                    ui.add_enabled_ui(enabled, |ui| {
                        egui::ComboBox::from_id_salt((file, "switch", w.code)).selected_text(current.to_string()).width(60.0).show_ui(ui, |ui| {
                            for v in 0..=w.max {
                                if ui.selectable_label(v == current, v.to_string()).clicked() && v != current {
                                    chosen = Some(v);
                                }
                            }
                        });
                    });
                    if let Some(value) = chosen {
                        out = Some(ViewEdit::Switch { code: w.code, value });
                    }
                    ui.label(egui::RichText::new(format!("0 to {}", w.max)).small().weak());
                }
                None => {
                    ui.label(egui::RichText::new("-").weak());
                    ui.label(egui::RichText::new(w.note.as_deref().unwrap_or("")).small().weak());
                }
            }
            ui.end_row();
        }
    });
    match &owner.iptc {
        Ok(presets) if presets.is_empty() => {
            ui.label(egui::RichText::new("IPTC presets: none to edit").weak());
        }
        Ok(presets) => {
            for p in presets {
                let title = format!("IPTC preset {} {:?}", p.slot, p.name);
                egui::CollapsingHeader::new(title).id_salt((file, "iptc_edit", p.slot)).default_open(false).show(ui, |ui| {
                    egui::Grid::new((file, "iptc_edit_fields", p.slot)).num_columns(2).striped(true).show(ui, |ui| {
                        let mut rows: Vec<(&str, &str)> = vec![("name", p.name.as_str())];
                        rows.extend(nikon_fleet::iptc::IPTC_FIELDS.iter().copied().zip(p.fields.iter().map(String::as_str)));
                        for (field, stored) in rows {
                            ui.label(field);
                            if let Some(text) = text_field(ui, state, &format!("iptc:{}:{field}", p.slot), stored, enabled, 300.0) {
                                out = Some(ViewEdit::Iptc { slot: p.slot, field: field.to_string(), text });
                            }
                            ui.end_row();
                        }
                    });
                });
            }
        }
        Err(why) => {
            ui.label(egui::RichText::new(format!("IPTC presets: {why}")).weak());
        }
    }
    match &owner.ftp {
        Ok(names) if names.is_empty() => {
            ui.label(egui::RichText::new("FTP profiles: the list is empty").weak());
        }
        Ok(names) => {
            ui.label(format!("FTP profiles ({}): the front of the list is used first. Restoring a changed list rebuilds it on the camera.", names.len()));
            for (i, name) in names.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("{i}")).weak());
                    ui.label(egui::RichText::new(name).monospace());
                    ui.add_enabled_ui(enabled && i > 0, |ui| {
                        if ui.small_button("↑").on_hover_text("Move up").clicked() {
                            out = ftp_row_press(state, i, names.len(), RowPress::Up).or(out.take());
                        }
                    });
                    ui.add_enabled_ui(enabled && i + 1 < names.len(), |ui| {
                        if ui.small_button("↓").on_hover_text("Move down").clicked() {
                            out = ftp_row_press(state, i, names.len(), RowPress::Down).or(out.take());
                        }
                    });
                    ui.add_enabled_ui(enabled, |ui| {
                        let label = if state.confirm_delete == Some(i) { "Really delete?" } else { "Delete" };
                        if ui.small_button(label).on_hover_text("Remove this profile from this copy (the camera is not touched)").clicked() {
                            out = ftp_row_press(state, i, names.len(), RowPress::Delete).or(out.take());
                        }
                    });
                });
            }
        }
        Err(why) => {
            ui.label(egui::RichText::new(format!("FTP profiles: {why}")).weak());
        }
    }
    out
}

/// The strip at the top of an edited copy's window: where it came from, and the history.
pub(crate) fn draw_edit_header(ui: &mut egui::Ui, file: &str, model: &EditModel, notice: Option<&(Level, String)>, reloading: bool) {
    let d = &model.derived;
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("✎ Edited copy").strong());
        ui.label(format!(
            "of {} (captured {}). Changes are saved as you make them; a snapshot read from a camera is never changed.",
            nikon_fleet::ops::render::printable(&d.source_file),
            d.source_captured_at.get(..19).unwrap_or(&d.source_captured_at)
        ));
        if reloading {
            ui.label(egui::RichText::new("saving…").italics().weak());
        }
    });
    if let Some((level, text)) = notice {
        colored(ui, *level, text.clone());
    }
    egui::CollapsingHeader::new(format!("History ({} change{})", d.edits.len(), if d.edits.len() == 1 { "" } else { "s" }))
        .id_salt((file, "edit_history"))
        .default_open(false)
        .show(ui, |ui| {
            if d.edits.is_empty() {
                ui.label(egui::RichText::new("No changes yet.").weak());
            }
            for e in d.edits.iter().rev() {
                ui.label(format!("{}  {}", e.at.get(..19).unwrap_or(&e.at), nikon_fleet::ops::render::printable(&e.change)));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use nikon_fleet::extended::{Extended, ExtendedProp, FtpProfileEntry, FtpState, FtpStore, IptcSlotEntry, IptcState, IptcStore, PropState, ReadFrom, hex_encode, prop_key};
    use nikon_fleet::iptc::{IptcPreset, encode_iptc_preset};
    use nikon_fleet::ptp_usb::{FtpProfile, encode_ftp_profile};
    use nikon_fleet::snapshot::{Camera, Transport};
    use serde_json::json;

    const NOW: &str = "2026-11-06T12:00:00Z";
    const VR: &str = "kNkMAIDCapability_VibrationReduction";

    fn captured() -> Snapshot {
        let mut s = Snapshot::new(Camera { model: "Z 9".into(), serial: "S1".into(), firmware: "5.31".into() }, Transport::Usb, "2026-11-05T10:00:00Z".into());
        s.label = Some("base".into());
        s.insert(VR, 34053, json!({"default_index": 2, "elem_bytes": 4, "elem_count": 3, "elem_type": 2, "value_index": 2, "values": [0, 1, 2]}));
        s.insert("kNkMAIDCapability_ElectronicVR", 33944, json!(1));
        s.insert("kNkMAIDCapability_Flag", 70000, json!(true));
        s.insert("kNkMAIDCapability_ExposureComp", 33000, json!({"default": 0.0, "default_index": 15, "lower": -5.0, "steps": 31, "upper": 5.0, "value": 0.0, "value_index": 15}));
        s.insert("kNkMAIDCapability_Continuous", 33001, json!({"lower": 0.0, "upper": 10.0, "steps": 0, "value": 2.5, "value_index": 0, "default": 0.0, "default_index": 0}));
        s.insert("kNkMAIDCapability_Level", 33005, json!(0.25));
        s.insert("kNkMAIDCapability_ModelName", 33002, json!("Z 9"));
        let mut props = BTreeMap::new();
        for (code, name, state) in [
            (0xD072u16, "Artist", PropState::Text { text: "Brian".into() }),
            (0xD073, "Copyright", PropState::Text { text: "(c)".into() }),
            (0xD090, "Image comment", PropState::NotSupported),
            (0xD053, "EnableCopyright", PropState::Bytes { hex: "01".into() }),
            (0xD060, "IPTCPresetSelect", PropState::Bytes { hex: "00".into() }),
        ] {
            props.insert(prop_key(code), ExtendedProp { code: u32::from(code), name: name.into(), state });
        }
        let profile = |n: &str| {
            hex_encode(&encode_ftp_profile(&FtpProfile { profile_name: n.into(), ssid_24ghz: "net".into(), ssid_5ghz: "k".into(), host: "h".into(), username: "u".into(), password: "pw".into(), port: 8080 }).unwrap())
        };
        let mut fields = vec![String::new(); 14];
        fields[2] = "Hello".into();
        s.extended = Some(Extended {
            read_from: ReadFrom { model: "Z 9".into(), usb_serial: "S1".into() },
            props,
            ftp: FtpState::Read(FtpStore {
                count: 3,
                limit: 20,
                active_slot: Some(0),
                device_name: "Z 9_S1".into(),
                truncated: false,
                profiles: ["alpha", "beta", "gamma"].iter().enumerate().map(|(i, n)| FtpProfileEntry { slot: i as u32, hex: profile(n) }).collect(),
            }),
            iptc: IptcState::Read(IptcStore { count: 1, truncated: false, slots: vec![IptcSlotEntry { slot: 1, hex: hex_encode(&encode_iptc_preset(&IptcPreset { name: "Race".into(), fields }).unwrap()) }] }),
        });
        s
    }

    fn data_dir_with(s: &Snapshot) -> (tempfile::TempDir, String) {
        let d = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(d.path().join("snapshots")).unwrap();
        s.save_to_file(d.path().join("snapshots").join("captured.json")).unwrap();
        (d, "captured.json".into())
    }

    // ── copying and applying ──

    #[test]
    fn making_a_copy_makes_a_new_marked_file_and_leaves_the_original() {
        let (d, file) = data_dir_with(&captured());
        let before = std::fs::read(d.path().join("snapshots").join(&file)).unwrap();
        let copy = make_edit_copy(d.path(), &file, NOW).unwrap();
        assert_ne!(copy, file);
        assert_eq!(std::fs::read(d.path().join("snapshots").join(&file)).unwrap(), before);
        assert!(Snapshot::load_from_file(d.path().join("snapshots").join(&copy)).unwrap().is_edited());
        for hostile in ["../captured.json", "/etc/passwd", "", "nope.json"] {
            assert!(make_edit_copy(d.path(), hostile, NOW).is_err(), "{hostile:?}");
        }
    }

    #[test]
    fn every_kind_of_change_the_viewer_can_ask_for_is_applied_through_the_library_and_kept() {
        let (d, file) = data_dir_with(&captured());
        let copy = make_edit_copy(d.path(), &file, NOW).unwrap();
        let later = "2026-11-06T12:05:00Z";
        for req in [
            ViewEdit::Property { name: VR.into(), text: "0".into() },
            ViewEdit::Property { name: "ExposureComp".into(), text: "-0.3333".into() },
            ViewEdit::Property { name: "ElectronicVR".into(), text: "0".into() },
            ViewEdit::Property { name: "Flag".into(), text: "false".into() },
            ViewEdit::OwnerString { code: 0xD072, text: "Zoë".into() },
            ViewEdit::Switch { code: 0xD053, value: 0 },
            ViewEdit::Iptc { slot: 1, field: "Headline".into(), text: "Race day".into() },
            ViewEdit::FtpMove { from: 2, to: 0 },
            ViewEdit::FtpDelete(1),
        ] {
            let a = apply_view_edit(d.path(), &copy, &req, later).unwrap_or_else(|e| panic!("{req:?}: {e}"));
            assert!(a.changed, "{req:?}");
        }
        let s = Snapshot::load_from_file(d.path().join("snapshots").join(&copy)).unwrap();
        assert_eq!(s.properties[VR].value["value_index"], 0);
        assert_eq!(s.derived_from.unwrap().edits.len(), 9);
    }

    #[test]
    fn a_refused_change_is_an_error_with_the_reason_and_the_file_is_untouched() {
        let (d, file) = data_dir_with(&captured());
        let copy = make_edit_copy(d.path(), &file, NOW).unwrap();
        let bytes = std::fs::read(d.path().join("snapshots").join(&copy)).unwrap();
        let e = apply_view_edit(d.path(), &copy, &ViewEdit::Property { name: VR.into(), text: "9".into() }, NOW).unwrap_err();
        assert!(e.contains("0, 1, 2"), "{e}");
        assert!(apply_view_edit(d.path(), &copy, &ViewEdit::OwnerString { code: 0xD072, text: "日本".into() }, NOW).is_err());
        assert!(apply_view_edit(d.path(), &copy, &ViewEdit::FtpDelete(7), NOW).is_err());
        assert_eq!(std::fs::read(d.path().join("snapshots").join(&copy)).unwrap(), bytes);
        // and the original, which is not an edited copy, is refused outright
        let e = apply_view_edit(d.path(), &file, &ViewEdit::Property { name: VR.into(), text: "0".into() }, NOW).unwrap_err();
        assert!(e.contains("read from a camera"), "{e}");
    }

    // ── the model ──

    #[test]
    fn only_an_edited_copy_has_a_model_and_it_offers_what_the_library_allows() {
        let cap = captured();
        assert!(EditModel::build(&cap, &no_labels()).is_none(), "a snapshot read from a camera gets no controls");
        let copy = nikon_fleet::snapshot_edit::copy_for_edit(&cap, "captured.json", None, NOW);
        let m = EditModel::build(&copy, &no_labels()).unwrap();
        assert_eq!(m.controls[VR].control, Control::Choice { values: vec![0, 1, 2], current: Some(2) });
        assert_eq!(m.controls[VR].options, ["0", "1", "2"], "with no names loaded the options are their numbers");
        assert!(matches!(m.controls["kNkMAIDCapability_ModelName"].control, Control::Locked { .. }));
        assert_eq!(m.controls.len(), copy.properties.len(), "every property has a control or a lock");
        assert!(m.owner.is_some());
    }

    fn no_labels() -> Labels {
        Labels::load_from(Path::new("/nonexistent/MaidLayer.config"), Path::new("/nonexistent/maid-labels.json"), "Z 9", None)
    }

    #[test]
    fn a_property_changed_in_the_copy_is_marked_and_the_others_are_not() {
        let cap = captured();
        let mut copy = nikon_fleet::snapshot_edit::copy_for_edit(&cap, "captured.json", None, NOW);
        edit::set_enum_value(&mut copy, VR, 0, NOW).unwrap();
        let m = EditModel::build(&copy, &no_labels()).unwrap();
        assert!(m.property_edited(VR));
        assert!(!m.property_edited("kNkMAIDCapability_ElectronicVR"));
        assert!(!m.property_edited("kNkMAIDCapability_Flag"), "an edit to the value it already held is not logged, so not marked");
    }

    // ── the controls ──

    fn run_ui(f: impl FnMut(&mut egui::Ui)) {
        let ctx = egui::Context::default();
        let mut f = f;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| f(ui));
        });
    }

    #[test]
    fn every_kind_of_control_draws_and_asks_for_nothing_until_the_person_does_something() {
        let copy = nikon_fleet::snapshot_edit::copy_for_edit(&captured(), "captured.json", None, NOW);
        let m = EditModel::build(&copy, &no_labels()).unwrap();
        let mut state = EditUi::default();
        run_ui(|ui| {
            for (name, view) in &m.controls {
                assert_eq!(property_cell(ui, &mut state, name, view, true), None, "{name} asks for nothing on its own");
                assert_eq!(property_cell(ui, &mut state, name, view, false), None, "{name} disabled");
            }
            let owner = m.owner.as_ref().unwrap();
            assert_eq!(draw_owner_editor(ui, "f.json", owner, &mut state, true), None);
            assert_eq!(draw_owner_editor(ui, "f.json", owner, &mut state, false), None);
            draw_edit_header(ui, "f.json", &m, Some(&(Level::Error, "a refusal".into())), true);
        });
    }

    // ── driving the controls with input ──

    /// Run `frames` of input through `draw` (called once per frame, with the rectangle the last widget it reports),
    /// returning what `draw` returned on each frame.
    fn drive<R>(frames: Vec<Vec<egui::Event>>, mut draw: impl FnMut(&mut egui::Ui, &mut Option<egui::Rect>) -> R) -> Vec<R> {
        let ctx = egui::Context::default();
        let mut rect = None;
        let mut out = Vec::new();
        for (n, events) in frames.into_iter().enumerate() {
            let mut raw = egui::RawInput::default();
            raw.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0)));
            raw.time = Some(n as f64 * 0.1);
            raw.events = events;
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| out.push(draw(ui, &mut rect)));
            });
        }
        out
    }

    fn click(at: egui::Pos2) -> Vec<Vec<egui::Event>> {
        let button = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        vec![vec![egui::Event::PointerMoved(at)], vec![button(true)], vec![button(false)]]
    }

    fn key(k: egui::Key) -> Vec<egui::Event> {
        vec![egui::Event::Key { key: k, physical_key: Some(k), pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }]
    }

    /// The text field of a whole-number property: click into it, type, then finish with `finish`.
    fn type_into_a_number(typed: &str, finish: egui::Key, stored: i64) -> Vec<Option<ViewEdit>> {
        type_into_a_number_with(typed, finish, stored, false)
    }

    fn type_into_a_number_with(typed: &str, finish: egui::Key, stored: i64, select_all_first: bool) -> Vec<Option<ViewEdit>> {
        let view = ControlView { control: Control::Whole { current: stored }, options: vec![] };
        let name = "kNkMAIDCapability_ElectronicVR";
        let mut state = EditUi::default();
        // frame 0 finds where the box is
        let probe = drive(vec![vec![]], |ui, rect| {
            let before = ui.cursor().min;
            let r = property_cell(ui, &mut state, name, &view, true);
            *rect = Some(egui::Rect::from_min_max(before, ui.min_rect().max));
            r
        });
        assert_eq!(probe, vec![None]);
        let center = egui::Pos2::new(12.0, 14.0);
        let mut frames = click(center);
        if select_all_first {
            frames.push(vec![egui::Event::Key { key: egui::Key::A, physical_key: Some(egui::Key::A), pressed: true, repeat: false, modifiers: egui::Modifiers::COMMAND }]);
        }
        frames.push(vec![egui::Event::Text(typed.to_string())]);
        frames.push(key(finish));
        frames.push(vec![]);
        let mut state = EditUi::default();
        drive(frames, move |ui, _| property_cell(ui, &mut state, name, &view, true))
    }

    #[test]
    fn typing_a_number_and_pressing_enter_asks_for_that_number_and_nothing_before() {
        let name = "kNkMAIDCapability_ElectronicVR";
        // after Select All the typed number replaces the stored one
        let asked = type_into_a_number_with("7", egui::Key::Enter, 1, true);
        let requests: Vec<&ViewEdit> = asked.iter().flatten().collect();
        assert_eq!(requests, [&ViewEdit::Property { name: name.into(), text: "7".into() }], "{asked:?}");
        let first = asked.iter().position(Option::is_some).unwrap();
        assert!(first >= 5, "not while clicking or typing: it commits on Enter (frame {first})");
        // a click puts the cursor where it landed (the left edge), so typing inserts there: the box is not cleared for you
        let inserted = type_into_a_number("7", egui::Key::Enter, 1);
        assert_eq!(inserted.iter().flatten().collect::<Vec<_>>(), [&ViewEdit::Property { name: name.into(), text: "71".into() }]);
    }

    #[test]
    fn escape_gives_up_the_typed_text_and_asks_for_nothing() {
        let asked = type_into_a_number("7", egui::Key::Escape, 1);
        assert!(asked.iter().all(Option::is_none), "{asked:?}");
    }

    #[test]
    fn leaving_a_box_with_the_text_it_started_with_asks_for_nothing() {
        // type nothing, press Enter: the box still holds the stored value
        let asked = type_into_a_number("", egui::Key::Enter, 1);
        assert!(asked.iter().all(Option::is_none), "{asked:?}");
    }

    #[test]
    fn clicking_a_switch_asks_for_the_opposite_and_a_disabled_one_cannot_be_clicked() {
        let view = ControlView { control: Control::Switch { current: true }, options: vec![] };
        let name = "kNkMAIDCapability_Flag";
        for (enabled, want) in [(true, Some(ViewEdit::Property { name: name.into(), text: "false".into() })), (false, None)] {
            let mut state = EditUi::default();
            let mut frames = click(egui::Pos2::new(12.0, 14.0));
            frames.push(vec![]);
            let asked = drive(frames, |ui, _| property_cell(ui, &mut state, name, &view, enabled));
            let got: Vec<&ViewEdit> = asked.iter().flatten().collect();
            assert_eq!(got.len(), usize::from(want.is_some()), "enabled={enabled}: {asked:?}");
            if let Some(w) = &want {
                assert_eq!(got[0], w);
            }
        }
    }

    #[test]
    fn deleting_an_ftp_profile_takes_two_presses_on_the_same_row_and_a_move_takes_one() {
        let mut st = EditUi::default();
        assert_eq!(ftp_row_press(&mut st, 1, 3, RowPress::Delete), None, "the first press only arms it");
        assert_eq!(st.confirm_delete, Some(1));
        assert_eq!(ftp_row_press(&mut st, 1, 3, RowPress::Delete), Some(ViewEdit::FtpDelete(1)));
        assert_eq!(st.confirm_delete, None, "and it is spent");
        assert_eq!(ftp_row_press(&mut st, 1, 3, RowPress::Delete), None, "a third press arms it again, it does not delete");
        // arming one row and pressing another's delete does not delete either
        let mut st = EditUi::default();
        ftp_row_press(&mut st, 0, 3, RowPress::Delete);
        assert_eq!(ftp_row_press(&mut st, 2, 3, RowPress::Delete), None);
        assert_eq!(st.confirm_delete, Some(2), "it re-arms on the row pressed");
        // a move disarms
        let mut st = EditUi::default();
        ftp_row_press(&mut st, 1, 3, RowPress::Delete);
        assert_eq!(ftp_row_press(&mut st, 1, 3, RowPress::Up), Some(ViewEdit::FtpMove { from: 1, to: 0 }));
        assert_eq!(st.confirm_delete, None);
        assert_eq!(ftp_row_press(&mut st, 1, 3, RowPress::Delete), None, "so the next delete press arms again");
        // a reload disarms
        ftp_row_press(&mut st, 1, 3, RowPress::Delete);
        st.clear();
        assert_eq!(ftp_row_press(&mut st, 1, 3, RowPress::Delete), None);
    }

    #[test]
    fn a_move_off_either_end_or_from_a_row_that_is_not_there_does_nothing() {
        let mut st = EditUi::default();
        assert_eq!(ftp_row_press(&mut st, 0, 3, RowPress::Up), None, "the first row cannot go up");
        assert_eq!(ftp_row_press(&mut st, 2, 3, RowPress::Down), None, "the last row cannot go down (len 3, row 2)");
        assert_eq!(ftp_row_press(&mut st, 1, 3, RowPress::Down), Some(ViewEdit::FtpMove { from: 1, to: 2 }));
        assert_eq!(ftp_row_press(&mut st, 3, 3, RowPress::Delete), None, "one past the end");
        assert_eq!(st.confirm_delete, None, "and it arms nothing");
        assert_eq!(ftp_row_press(&mut st, 0, 0, RowPress::Delete), None, "an empty list");
        assert_eq!(ftp_row_press(&mut st, 0, 1, RowPress::Down), None, "a list of one has nowhere to go");
    }

    /// Where a click lands on a control's rail, as a fraction of the width its ui uses.
    fn scale_view(index: Option<u64>) -> ControlView {
        ControlView { control: Control::Scale { lower: -5.0, upper: 5.0, steps: 31, index }, options: vec![] }
    }

    fn slider_extent(view: &ControlView) -> egui::Rect {
        let ctx = egui::Context::default();
        let mut used = egui::Rect::NOTHING;
        let mut state = EditUi::default();
        let mut raw = egui::RawInput::default();
        raw.screen_rect = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0)));
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                property_cell(ui, &mut state, "kNkMAIDCapability_ExposureComp", view, true);
                used = ui.min_rect();
            });
        });
        used
    }

    #[test]
    fn clicking_a_slider_asks_for_the_position_clicked_once_and_only_when_it_differs() {
        let view = scale_view(Some(15));
        let used = slider_extent(&view);
        assert!(used.width() > 40.0, "the slider has a rail to click: {used:?}");
        // the rail is the left part of what the slider uses (the value box is to its right)
        let y = used.top() + 12.0;
        let far_left = egui::Pos2::new(used.left() + 8.0, y);
        let mut state = EditUi::default();
        let mut frames = click(far_left);
        frames.push(vec![]);
        frames.push(vec![]);
        let asked = drive(frames, |ui, _| property_cell(ui, &mut state, "kNkMAIDCapability_ExposureComp", &view, true));
        let got: Vec<&ViewEdit> = asked.iter().flatten().collect();
        assert_eq!(got.len(), 1, "one request for one click, not one per frame: {asked:?}");
        let ViewEdit::Property { name, text } = got[0] else { panic!("{got:?}") };
        assert_eq!(name, "kNkMAIDCapability_ExposureComp");
        let asked_value: f64 = text.parse().unwrap();
        assert!(asked_value < -3.0, "a click near the left end of -5..5 is near -5, not the current 0: {text}");
        // a click on the very end of the rail when the scale is already at that end is not a request
        let at_left = scale_view(Some(0));
        let edge = egui::Pos2::new(used.left() + 1.0, y);
        let mut state = EditUi::default();
        let mut frames = click(edge);
        frames.push(vec![]);
        frames.push(vec![]);
        let asked = drive(frames, |ui, _| property_cell(ui, &mut state, "kNkMAIDCapability_ExposureComp", &at_left, true));
        assert!(asked.iter().all(Option::is_none), "asking for -5 when it is -5 is not an edit: {asked:?}");
        // and the same click when it is somewhere else is
        let mut state = EditUi::default();
        let mut frames = click(edge);
        frames.push(vec![]);
        frames.push(vec![]);
        let asked = drive(frames, |ui, _| property_cell(ui, &mut state, "kNkMAIDCapability_ExposureComp", &view, true));
        let got: Vec<&ViewEdit> = asked.iter().flatten().collect();
        assert!(matches!(got.as_slice(), [ViewEdit::Property { text, .. }] if text == "-5"), "{asked:?}");
    }

    #[test]
    fn the_scale_text_is_the_number_the_position_stands_for() {
        // 31 steps over -5..5: position 15 is 0, 16 is a third of an EV
        assert_eq!(edit_scale_text(-5.0, 5.0, 31, 15), "0");
        assert_eq!(edit_scale_text(-5.0, 5.0, 31, 16), "0.333333");
        assert_eq!(edit_scale_text(-5.0, 5.0, 31, 0), "-5");
        assert_eq!(edit_scale_text(-5.0, 5.0, 31, 30), "5");
        // and the text goes back to the same position through the library
        let mut copy = nikon_fleet::snapshot_edit::copy_for_edit(&captured(), "captured.json", None, NOW);
        for index in [0u64, 1, 15, 16, 29, 30] {
            let text = edit_scale_text(-5.0, 5.0, 31, index);
            edit::set_property_text(&mut copy, "ExposureComp", &text, NOW).unwrap();
            assert_eq!(copy.properties["kNkMAIDCapability_ExposureComp"].value["value_index"], index, "{text}");
        }
    }

    // ── through the app ──

    /// The app with a stored snapshot in its data directory; the file name.
    fn app_with_snapshot() -> (FleetApp, std::sync::mpsc::Receiver<Cmd>, tempfile::TempDir, String) {
        let (app, cmd_rx, _evt_tx, dir) = crate::tests::test_app();
        std::fs::create_dir_all(dir.path().join("snapshots")).unwrap();
        captured().save_to_file(dir.path().join("snapshots").join("captured.json")).unwrap();
        (app, cmd_rx, dir, "captured.json".into())
    }

    fn wait_for_reload(app: &mut FleetApp, file: &str) {
        for _ in 0..400 {
            app.poll_views();
            if app.viewers.find_mut(file).is_some_and(|v| !v.reloading && !v.loading) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        panic!("the viewer for {file} never finished loading");
    }

    #[test]
    fn edit_a_copy_makes_the_copy_opens_it_with_controls_and_leaves_the_original_unopened_and_unchanged() {
        let (mut app, cmd_rx, dir, file) = app_with_snapshot();
        let ctx = egui::Context::default();
        let before = std::fs::read(dir.path().join("snapshots").join(&file)).unwrap();
        app.start_edit_copy(&ctx, &file);
        assert!(app.status.text.contains("editable copy") && app.status.text.contains("untouched"), "{}", app.status.text);
        assert!(matches!(cmd_rx.try_recv(), Ok(Cmd::RefreshListing)), "the listing is refreshed so the copy shows in it");
        let copy = app.viewers.open.iter().map(|v| v.file.clone()).find(|f| f != &file).expect("the copy is open");
        assert!(app.viewers.open.iter().all(|v| v.file != file), "the original was not opened");
        wait_for_reload(&mut app, &copy);
        let v = app.viewers.find_mut(&copy).unwrap();
        assert!(v.edit.is_some(), "an edited copy gets controls");
        assert!(v.title().contains("edited copy"), "{}", v.title());
        assert_eq!(std::fs::read(dir.path().join("snapshots").join(&file)).unwrap(), before);
        // a captured snapshot opened in the viewer gets none
        app.open_snapshot(&ctx, &file, false, false);
        wait_for_reload(&mut app, &file);
        assert!(app.viewers.find_mut(&file).unwrap().edit.is_none());
        assert!(!app.viewers.find_mut(&file).unwrap().title().contains("edited"));
    }

    #[test]
    fn a_change_is_saved_the_window_reloads_and_shows_what_is_stored() {
        let (mut app, _cmd_rx, dir, file) = app_with_snapshot();
        let ctx = egui::Context::default();
        app.start_edit_copy(&ctx, &file);
        let copy = app.viewers.open[0].file.clone();
        wait_for_reload(&mut app, &copy);
        app.viewers.find_mut(&copy).unwrap().edit_ui.texts.insert("string:0xd073".into(), "half typed".into());
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: VR.into(), text: "0".into() });
        let v = app.viewers.find_mut(&copy).unwrap();
        assert!(v.reloading, "the controls wait for the file to be read again");
        assert_eq!(v.edit_notice.as_ref().map(|(l, t)| (*l, t.as_str())), Some((Level::Info, "VibrationReduction: 2 -> 0")));
        wait_for_reload(&mut app, &copy);
        let v = app.viewers.find_mut(&copy).unwrap();
        let m = v.edit.as_ref().unwrap();
        assert_eq!(m.controls[VR].control, Control::Choice { values: vec![0, 1, 2], current: Some(0) }, "what is shown is what is stored");
        assert!(m.property_edited(VR));
        assert_eq!(m.derived.edits.len(), 1);
        assert!(v.edit_notice.is_some(), "the notice survives the reload");
        assert!(!v.edit_ui.has_text("string:0xd073"), "text typed before the reload does not outlive the value it started from");
        assert_eq!(Snapshot::load_from_file(dir.path().join("snapshots").join(&copy)).unwrap().properties[VR].value["value_index"], 0);
    }

    #[test]
    fn a_refused_change_shows_the_reason_does_not_reload_and_resets_the_field() {
        let (mut app, _cmd_rx, dir, file) = app_with_snapshot();
        let ctx = egui::Context::default();
        app.start_edit_copy(&ctx, &file);
        let copy = app.viewers.open[0].file.clone();
        wait_for_reload(&mut app, &copy);
        let bytes = std::fs::read(dir.path().join("snapshots").join(&copy)).unwrap();
        app.viewers.find_mut(&copy).unwrap().edit_ui.texts.insert("property:kNkMAIDCapability_ElectronicVR".into(), "typed".into());
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: "kNkMAIDCapability_ElectronicVR".into(), text: "x".into() });
        let v = app.viewers.find_mut(&copy).unwrap();
        let (level, text) = v.edit_notice.clone().unwrap();
        assert_eq!(level, Level::Error);
        assert!(text.contains("whole number"), "{text}");
        assert!(!v.reloading, "nothing changed, so nothing is read again");
        assert!(!v.edit_ui.has_text("property:kNkMAIDCapability_ElectronicVR"), "the field shows the stored value again");
        assert_eq!(std::fs::read(dir.path().join("snapshots").join(&copy)).unwrap(), bytes);
        // a change to what it already holds says so and does not reload
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: VR.into(), text: "2".into() });
        let v = app.viewers.find_mut(&copy).unwrap();
        assert!(!v.reloading && v.edit_notice.as_ref().unwrap().1.starts_with("No change"), "{:?}", v.edit_notice);
    }

    #[test]
    fn a_change_to_a_window_whose_file_is_a_capture_is_refused_by_the_library() {
        let (mut app, _cmd_rx, dir, file) = app_with_snapshot();
        let ctx = egui::Context::default();
        app.open_snapshot(&ctx, &file, false, false);
        wait_for_reload(&mut app, &file);
        let before = std::fs::read(dir.path().join("snapshots").join(&file)).unwrap();
        app.apply_viewer_edit(&ctx, &file, ViewEdit::Property { name: VR.into(), text: "0".into() });
        let v = app.viewers.find_mut(&file).unwrap();
        assert!(v.edit_notice.as_ref().unwrap().1.contains("read from a camera"), "{:?}", v.edit_notice);
        assert_eq!(std::fs::read(dir.path().join("snapshots").join(&file)).unwrap(), before);
    }

    #[test]
    fn the_snapshot_list_marks_an_edited_copy_and_its_label_says_so() {
        let (mut app, _cmd_rx, dir, file) = app_with_snapshot();
        let ctx = egui::Context::default();
        app.start_edit_copy(&ctx, &file);
        let listing = list_all(dir.path());
        let edited: Vec<(&str, bool)> = listing.rows.iter().map(|r| (r.file.as_str(), r.edited)).collect();
        assert_eq!(edited.len(), 2);
        assert!(edited.contains(&(file.as_str(), false)) && edited.iter().any(|(f, e)| *f != file && *e), "{edited:?}");
        let row = listing.rows.iter().find(|r| r.edited).unwrap();
        assert!(snapshot_label(row).ends_with("✎ edited"), "{}", snapshot_label(row));
        assert!(!snapshot_label(listing.rows.iter().find(|r| !r.edited).unwrap()).contains("edited"));
    }

    #[test]
    fn an_edited_copy_window_draws_with_controls_and_a_capture_window_draws_without() {
        let (mut app, _cmd_rx, _dir, file) = app_with_snapshot();
        let ctx = egui::Context::default();
        app.start_edit_copy(&ctx, &file);
        app.open_snapshot(&ctx, &file, false, false);
        for v in app.viewers.open.clone() {
            wait_for_reload(&mut app, &v.file);
        }
        let ctx = egui::Context::default();
        let mut asked = Vec::new();
        for _ in 0..2 {
            let _ = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    for v in &mut app.viewers.open {
                        asked.push(draw_snapshot_viewer(ui, v));
                    }
                });
            });
        }
        assert!(asked.iter().all(Option::is_none), "drawing asks for nothing by itself: {asked:?}");
        let copy_has_controls = app.viewers.open.iter().filter(|v| v.edit.is_some()).count();
        assert_eq!(copy_has_controls, 1);
    }

    #[test]
    fn a_refused_field_goes_back_to_the_stored_value_and_the_other_fields_keep_what_was_typed() {
        let mut ui_state = EditUi::default();
        ui_state.texts.insert("string:0xd072".into(), "typed but refused".into());
        ui_state.texts.insert("string:0xd073".into(), "still being typed".into());
        let req = ViewEdit::OwnerString { code: 0xD072, text: "x".into() };
        ui_state.forget(&field_key(&req).unwrap());
        assert!(!ui_state.has_text("string:0xd072") && ui_state.has_text("string:0xd073"));
        ui_state.confirm_delete = Some(1);
        ui_state.clear();
        assert_eq!(ui_state, EditUi::default(), "a reload forgets everything, so no field outlives the value it started from");
        assert_eq!(field_key(&ViewEdit::FtpDelete(0)), None);
        assert_eq!(field_key(&ViewEdit::Property { name: "N".into(), text: String::new() }).as_deref(), Some("property:N"));
        assert_eq!(field_key(&ViewEdit::Iptc { slot: 2, field: "Headline".into(), text: String::new() }).as_deref(), Some("iptc:2:Headline"));
    }
}
