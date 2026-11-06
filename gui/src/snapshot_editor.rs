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
use nikon_fleet::ftp_edit::{ProfileFields, ProfileView, Protocol};
use nikon_fleet::ftp_test::{StepOutcome, TestReport};
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
    /// The FTP profiles with their fields (secrets included: the window masks them), or why the list cannot be edited.
    pub ftp: Result<Vec<FtpRow>, String>,
    pub derived: DerivedFrom,
    /// The edit log's `item` keys, as a set: a row asks "was this property edited" every frame, and scanning the whole
    /// log for each of ~200 rows would grow with every edit. Built once, with the model.
    edited_items: std::collections::HashSet<String>,
}

/// One FTP profile of the list as the editor needs it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FtpRow {
    Fields(ProfileView),
    /// The stored profile does not decode, so its fields cannot be shown or edited; it can still be moved or deleted.
    Unreadable { name: String, why: String },
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
        let owner = edit::owner_view(s);
        let ftp = match owner.as_ref().map(|o| &o.ftp) {
            None => Err("the snapshot has no extended section".to_string()),
            Some(Err(why)) => Err(why.clone()),
            Some(Ok(names)) => {
                let mut copy = s.clone();
                Ok((0..names.len())
                    .map(|i| match nikon_fleet::ftp_edit::profile_view(&mut copy, i) {
                        Ok(v) => FtpRow::Fields(v),
                        Err(e) => FtpRow::Unreadable { name: names[i].clone(), why: e.to_string() },
                    })
                    .collect())
            }
        };
        let edited_items = derived.edits.iter().map(|e| e.item.clone()).collect();
        Some(EditModel { controls, owner, ftp, derived, edited_items })
    }

    /// The property was changed in this copy (it is in the edit log).
    pub(crate) fn property_edited(&self, name: &str) -> bool {
        self.edited_items.contains(&format!("property:{name}"))
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
    /// Change fields of one FTP profile.
    FtpFields { index: usize, fields: ProfileFields },
    FtpAdd { fields: ProfileFields },
    FtpDuplicate(usize),
    /// Something typed that is not a value at all (a port that is not a number): shown as a refusal, nothing is written.
    Invalid { key: String, why: String },
    /// Try the stored profile on its server (handled by the app, not by the file).
    TestFtp { index: usize, file_check: bool },
    /// Remove a test result from the window.
    DismissFtpTest(usize),
}

pub(crate) fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc().format(&Rfc3339).unwrap_or_default()
}

/// Apply `edit` to the edited copy `file` in `snapshots/` and keep the result, whole or not at all. The error is for
/// a person and names the reason; nothing was written when there is one.
pub(crate) fn apply_view_edit(data_dir: &Path, file: &str, req: &ViewEdit, now: &str) -> Result<edit::Applied, String> {
    let snaps = crate::snapshots_dir(data_dir);
    let done = match req {
        ViewEdit::Property { name, text } => edit_in_place(&snaps, file, |s| edit::set_property_text(s, name, text, now).map(|(_, a)| a)),
        ViewEdit::OwnerString { code, text } => edit_in_place(&snaps, file, |s| edit::set_owner_string(s, *code, text, now)),
        ViewEdit::Switch { code, value } => edit_in_place(&snaps, file, |s| edit::set_switch(s, *code, *value, now)),
        ViewEdit::Iptc { slot, field, text } => edit_in_place(&snaps, file, |s| edit::set_iptc_field(s, *slot, field, text, now)),
        ViewEdit::FtpDelete(index) => edit_in_place(&snaps, file, |s| edit::ftp_delete(s, *index, now)),
        ViewEdit::FtpMove { from, to } => edit_in_place(&snaps, file, |s| edit::ftp_move(s, *from, *to, now)),
        ViewEdit::FtpFields { index, fields } => edit_in_place(&snaps, file, |s| nikon_fleet::ftp_edit::ftp_set_fields(s, *index, fields, now)),
        ViewEdit::FtpAdd { fields } => edit_in_place(&snaps, file, |s| nikon_fleet::ftp_edit::ftp_add(s, fields, None, now)),
        ViewEdit::FtpDuplicate(index) => edit_in_place(&snaps, file, |s| nikon_fleet::ftp_edit::ftp_duplicate(s, *index, None, &ProfileFields::default(), now)),
        ViewEdit::Invalid { why, .. } => return Err(why.clone()),
        ViewEdit::TestFtp { .. } | ViewEdit::DismissFtpTest(_) => return Err("this is not a change to the file".to_string()),
    };
    done.map_err(|e| e.to_string())
}

/// Make an editable copy of `snapshots/<file>`; the new file's name. The original is only read.
pub(crate) fn make_edit_copy(data_dir: &Path, file: &str, now: &str) -> Result<String, String> {
    let snaps = crate::snapshots_dir(data_dir);
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
    /// FTP tests started or finished in this window, by the profile's position. Cleared on a reload: the list may
    /// have changed, and a result is of the profile as it was.
    pub(crate) ftp_tests: BTreeMap<usize, FtpTestState>,
    /// Leave the test file out of an FTP test (connect, log in and go to the folder only). A choice, so it survives a reload.
    skip_file_check: bool,
}

/// An FTP test of one profile of this window.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FtpTestState {
    Running,
    Done(Box<TestReport>),
    /// The test could not be started or run at all (not a failed step: nothing was tried).
    Failed(String),
}

impl EditUi {
    pub(crate) fn clear(&mut self) {
        let skip_file_check = self.skip_file_check;
        *self = EditUi { skip_file_check, ..EditUi::default() };
    }

    pub(crate) fn skip_file_check(&self) -> bool {
        self.skip_file_check
    }

    /// Whether a test also puts, fetches and deletes a small file.
    pub(crate) fn file_check_wanted(&self) -> bool {
        !self.skip_file_check
    }

    pub(crate) fn want_file_check(&mut self, wanted: bool) {
        self.skip_file_check = !wanted;
    }

    /// The request the Test button of profile `index` makes.
    pub(crate) fn test_request(&self, index: usize) -> ViewEdit {
        ViewEdit::TestFtp { index, file_check: self.file_check_wanted() }
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
        ViewEdit::FtpFields { index, fields } => format!("ftp:{index}:{}", ftp_field_name(fields)?),
        ViewEdit::Invalid { key, .. } => key.clone(),
        ViewEdit::Switch { .. } | ViewEdit::FtpDelete(_) | ViewEdit::FtpMove { .. } | ViewEdit::FtpAdd { .. } | ViewEdit::FtpDuplicate(_) | ViewEdit::TestFtp { .. } | ViewEdit::DismissFtpTest(_) => return None,
    })
}

/// The one field a `ProfileFields` from a single edit sets, by the name its text box keeps its text under.
fn ftp_field_name(f: &ProfileFields) -> Option<&'static str> {
    [
        ("name", f.name.is_some()),
        ("wifi_name", f.wifi_name.is_some()),
        ("wifi_key", f.wifi_key.is_some()),
        ("host", f.host.is_some()),
        ("folder", f.folder.is_some()),
        ("username", f.username.is_some()),
        ("password", f.password.is_some()),
        ("port", f.port.is_some()),
        ("proxy_port", f.proxy_port.is_some()),
    ]
    .into_iter()
    .find_map(|(n, set)| set.then_some(n))
}

// ── drawing ──────────────────────────────────────────────────────────────

/// A text box that commits when it loses focus (Enter, Tab or a click elsewhere) and gives up on Escape. The text
/// typed so far is kept in `ui_state` under `key`; `stored` is what the file holds. Returns the new text when it
/// differs from `stored`.
fn text_field(ui: &mut egui::Ui, state: &mut EditUi, key: &str, stored: &str, enabled: bool, width: f32) -> Option<String> {
    text_field_masked(ui, state, key, stored, enabled, width, false)
}

/// [`text_field`], showing dots instead of the text when `masked`.
fn text_field_masked(ui: &mut egui::Ui, state: &mut EditUi, key: &str, stored: &str, enabled: bool, width: f32, masked: bool) -> Option<String> {
    let buf = state.texts.entry(key.to_string()).or_insert_with(|| stored.to_string());
    let resp = ui.add_enabled(enabled, egui::TextEdit::singleline(buf).desired_width(width).password(masked));
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
pub(crate) fn draw_owner_editor(
    ui: &mut egui::Ui,
    file: &str,
    owner: &OwnerView,
    ftp: &Result<Vec<FtpRow>, String>,
    state: &mut EditUi,
    enabled: bool,
    reveal: &mut bool,
) -> Option<ViewEdit> {
    let mut out = None;
    let warn = egui::Color32::from_rgb(200, 120, 0);
    ui.separator();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Owner data (edit)").strong());
        let label = if *reveal { "👁 Hide passwords" } else { "👁 Show passwords" };
        if ui.selectable_label(*reveal, label).clicked() {
            *reveal = !*reveal;
        }
    });
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
    if let Some(req) = draw_ftp_editor(ui, file, ftp, state, enabled, *reveal) {
        out = Some(req);
    }
    out
}

/// A number box for a port. Typing something that is not a whole number from 0 to 65535 is reported as such.
fn port_field(ui: &mut egui::Ui, state: &mut EditUi, key: &str, stored: u16, enabled: bool) -> Option<Result<u16, ViewEdit>> {
    let typed = text_field(ui, state, key, &stored.to_string(), enabled, 70.0)?;
    Some(typed.trim().parse::<u16>().map_err(|_| ViewEdit::Invalid { key: key.to_string(), why: format!("{typed:?} is not a port: a whole number from 0 to 65535") }))
}

/// The list of FTP profiles: for each, its name, the buttons that move, copy, delete and test it, its fields, and the
/// result of the last test; and a form to add one.
fn draw_ftp_editor(ui: &mut egui::Ui, file: &str, ftp: &Result<Vec<FtpRow>, String>, state: &mut EditUi, enabled: bool, reveal: bool) -> Option<ViewEdit> {
    let mut out: Option<ViewEdit> = None;
    ui.add_space(6.0);
    let rows = match ftp {
        Err(why) => {
            ui.label(egui::RichText::new(format!("FTP profiles: {why}")).weak());
            return None;
        }
        Ok(rows) => rows,
    };
    ui.label(format!(
        "FTP profiles ({}): the front of the list is used first. Nothing here touches a camera; restoring the changed list rebuilds it on the camera. \
         A profile you change is saved at once; Test connection tries it from this computer.",
        rows.len()
    ));
    ui.horizontal(|ui| {
        let mut also_file = state.file_check_wanted();
        if ui.checkbox(&mut also_file, "Test also puts, fetches and deletes a small file").changed() {
            state.want_file_check(also_file);
        }
    });
    for (i, row) in rows.iter().enumerate() {
        let title = match row {
            FtpRow::Fields(v) => format!("{i}  {:?}", v.name),
            FtpRow::Unreadable { name, .. } => format!("{i}  {name}"),
        };
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(title).monospace());
            ui.add_enabled_ui(enabled && i > 0, |ui| {
                if ui.small_button("↑").on_hover_text("Move up").clicked() {
                    out = ftp_row_press(state, i, rows.len(), RowPress::Up).or(out.take());
                }
            });
            ui.add_enabled_ui(enabled && i + 1 < rows.len(), |ui| {
                if ui.small_button("↓").on_hover_text("Move down").clicked() {
                    out = ftp_row_press(state, i, rows.len(), RowPress::Down).or(out.take());
                }
            });
            ui.add_enabled_ui(enabled && matches!(row, FtpRow::Fields(_)), |ui| {
                if ui.small_button("Duplicate").on_hover_text("Copy this profile (Wi-Fi name and key and login included) right after it").clicked() {
                    out = Some(ViewEdit::FtpDuplicate(i));
                }
            });
            ui.add_enabled_ui(enabled, |ui| {
                let label = if state.confirm_delete == Some(i) { "Really delete?" } else { "Delete" };
                if ui.small_button(label).on_hover_text("Remove this profile from this copy (the camera is not touched)").clicked() {
                    out = ftp_row_press(state, i, rows.len(), RowPress::Delete).or(out.take());
                }
            });
            let running = matches!(state.ftp_tests.get(&i), Some(FtpTestState::Running));
            ui.add_enabled_ui(enabled && !running && matches!(row, FtpRow::Fields(_)), |ui| {
                if ui.small_button("Test connection").on_hover_text("Try this profile's server from this computer").clicked() {
                    out = Some(state.test_request(i));
                }
            });
        });
        match row {
            FtpRow::Unreadable { why, .. } => {
                ui.label(egui::RichText::new(format!("    cannot be edited: it does not decode ({why})")).small().color(egui::Color32::from_rgb(200, 120, 0)));
            }
            FtpRow::Fields(v) => {
                egui::CollapsingHeader::new("fields").id_salt((file, "ftp_fields_edit", i)).default_open(false).show(ui, |ui| {
                    if let Some(req) = draw_ftp_fields(ui, file, i, v, state, enabled, reveal) {
                        out = Some(req);
                    }
                });
            }
        }
        if let Some(t) = state.ftp_tests.get(&i).cloned() {
            ui.indent(("ftp_test_result", file, i), |ui| {
                if draw_ftp_test(ui, &t) {
                    out = Some(ViewEdit::DismissFtpTest(i));
                }
            });
        }
    }
    egui::CollapsingHeader::new("Add a profile").id_salt((file, "ftp_add")).default_open(false).show(ui, |ui| {
        let mut set = |ui: &mut egui::Ui, label: &str, key: &str, secret: bool| -> Option<String> {
            ui.label(label);
            let buf = state.texts.entry(format!("ftp:new:{key}")).or_default();
            ui.add_enabled(enabled, egui::TextEdit::singleline(buf).desired_width(260.0).password(secret && !reveal));
            ui.end_row();
            let t = state.texts.get(&format!("ftp:new:{key}")).cloned().unwrap_or_default();
            (!t.is_empty()).then_some(t)
        };
        let mut f = ProfileFields::default();
        egui::Grid::new((file, "ftp_add_grid")).num_columns(2).show(ui, |ui| {
            f.name = set(ui, "Profile name", "name", false);
            f.host = set(ui, "Server (host)", "host", false);
            f.folder = set(ui, "Folder", "folder", false);
            f.username = set(ui, "Login", "username", false);
            f.password = set(ui, "Password", "password", true);
            f.wifi_name = set(ui, "Wi-Fi name", "wifi_name", false);
            f.wifi_key = set(ui, "Wi-Fi key", "wifi_key", true);
        });
        ui.label(egui::RichText::new("The port is 21 and the protocol FTP until you change them in the new profile's fields. To reuse a Wi-Fi network, Duplicate a profile instead.").small().weak());
        ui.add_enabled_ui(enabled, |ui| {
            if ui.button("Add this profile").clicked() {
                out = Some(ViewEdit::FtpAdd { fields: f.clone() });
            }
        });
    });
    out
}

/// The fields of one profile as text boxes. Returns the change committed this frame.
fn draw_ftp_fields(ui: &mut egui::Ui, file: &str, i: usize, v: &ProfileView, state: &mut EditUi, enabled: bool, reveal: bool) -> Option<ViewEdit> {
    let mut out = None;
    let one = |f: ProfileFields| Some(ViewEdit::FtpFields { index: i, fields: f });
    egui::Grid::new((file, "ftp_fields_grid", i)).num_columns(2).striped(true).show(ui, |ui| {
        let mut text = |ui: &mut egui::Ui, label: &str, key: &str, stored: &str, secret: bool| -> Option<String> {
            ui.label(label);
            let r = text_field_masked(ui, state, &format!("ftp:{i}:{key}"), stored, enabled, 280.0, secret && !reveal);
            ui.end_row();
            r
        };
        if let Some(t) = text(ui, "Profile name", "name", &v.name, false) {
            out = one(ProfileFields { name: Some(t), ..Default::default() });
        }
        if let Some(t) = text(ui, "Server (host)", "host", &v.host, false) {
            out = one(ProfileFields { host: Some(t), ..Default::default() });
        }
        if let Some(t) = text(ui, "Folder", "folder", &v.folder, false) {
            out = one(ProfileFields { folder: Some(t), ..Default::default() });
        }
        if let Some(t) = text(ui, "Login", "username", &v.username, false) {
            out = one(ProfileFields { username: Some(t), ..Default::default() });
        }
        if let Some(t) = text(ui, "Password", "password", &v.password, true) {
            out = one(ProfileFields { password: Some(t), ..Default::default() });
        }
        if let Some(t) = text(ui, "Wi-Fi name", "wifi_name", &v.wifi_name, false) {
            out = one(ProfileFields { wifi_name: Some(t), ..Default::default() });
        }
        if let Some(t) = text(ui, "Wi-Fi key", "wifi_key", &v.wifi_key, true) {
            out = one(ProfileFields { wifi_key: Some(t), ..Default::default() });
        }
        ui.label("Server port");
        match port_field(ui, state, &format!("ftp:{i}:port"), v.port, enabled) {
            Some(Ok(p)) => out = one(ProfileFields { port: Some(p), ..Default::default() }),
            Some(Err(invalid)) => out = Some(invalid),
            None => {}
        }
        ui.end_row();
        ui.label("Protocol");
        ui.add_enabled_ui(enabled, |ui| {
            let shown = if v.protocol == Protocol::Sftp { "SFTP" } else { "FTP" };
            egui::ComboBox::from_id_salt((file, "ftp_protocol", i)).selected_text(shown).width(80.0).show_ui(ui, |ui| {
                for (p, name) in [(Protocol::Ftp, "FTP"), (Protocol::Sftp, "SFTP")] {
                    if ui.selectable_label(v.protocol == p, name).clicked() && v.protocol != p {
                        out = one(ProfileFields { protocol: Some(p), ..Default::default() });
                    }
                }
            });
        });
        ui.end_row();
        ui.label("Second port");
        match port_field(ui, state, &format!("ftp:{i}:proxy_port"), v.proxy_port, enabled) {
            Some(Ok(p)) => out = one(ProfileFields { proxy_port: Some(p), ..Default::default() }),
            Some(Err(invalid)) => out = Some(invalid),
            None => {}
        }
        ui.end_row();
    });
    ui.label(egui::RichText::new("The Wi-Fi key is stored in the field called the 5 GHz SSID; it is probably the passphrase (unconfirmed). The second port is probably a proxy port.").small().weak());
    out
}

/// A test's state as lines. True when the person asked to remove it.
fn draw_ftp_test(ui: &mut egui::Ui, t: &FtpTestState) -> bool {
    let (ok, bad) = (egui::Color32::from_rgb(60, 160, 80), egui::Color32::from_rgb(200, 60, 60));
    let mut dismiss = false;
    match t {
        FtpTestState::Running => {
            ui.label(egui::RichText::new("Testing…").italics());
        }
        FtpTestState::Failed(why) => {
            ui.colored_label(bad, format!("The test could not be run: {why}"));
            dismiss = ui.small_button("Close").clicked();
        }
        FtpTestState::Done(r) => {
            for s in &r.steps {
                match &s.outcome {
                    StepOutcome::Ok { detail } => {
                        ui.colored_label(ok, format!("✔ {}: {detail}", s.step.words()));
                    }
                    StepOutcome::Failed { message, hint, .. } => {
                        ui.colored_label(bad, format!("✘ {}: {message}", s.step.words()));
                        ui.label(egui::RichText::new(hint).small());
                    }
                    StepOutcome::Skipped { because } => {
                        ui.label(egui::RichText::new(format!("– {}: {because}", s.step.words())).weak());
                    }
                }
            }
            if let Some(k) = &r.host_key {
                ui.label(egui::RichText::new(format!("server host key {k} (reported, not checked)")).small().weak());
            }
            if let Some(f) = &r.file_left_behind {
                ui.colored_label(bad, format!("The test file {f:?} could not be deleted and is still on the server; delete it by hand."));
            }
            ui.horizontal(|ui| {
                // The wording follows what was tried: with the test-file check off, "accepted" would overstate it.
                let verdict = r.verdict();
                let mut chars = verdict.chars();
                let sentence = chars.next().map(|c| c.to_uppercase().chain(chars).collect::<String>()).unwrap_or_default();
                ui.colored_label(if r.ok { ok } else { bad }, format!("{sentence}."));
                dismiss = ui.small_button("Close").clicked();
            });
            ui.label(egui::RichText::new("This tests the server only, not whether the camera can join the Wi-Fi network.").small().weak());
        }
    }
    dismiss
}

/// The strip at the top of an edited copy's window: where it came from, and the history.
pub(crate) fn draw_edit_header(ui: &mut egui::Ui, file: &str, model: &EditModel, notice: Option<&(Level, String)>, saving: bool) {
    let d = &model.derived;
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("✎ Edited copy").strong());
        ui.label(format!(
            "of {} (captured {}). Changes are saved as you make them; a snapshot read from a camera is never changed.",
            nikon_fleet::ops::render::printable(&d.source_file),
            d.source_captured_at.get(..19).unwrap_or(&d.source_captured_at)
        ));
        if saving {
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
            let mut reveal = false;
            assert_eq!(draw_owner_editor(ui, "f.json", owner, &m.ftp, &mut state, true, &mut reveal), None);
            assert_eq!(draw_owner_editor(ui, "f.json", owner, &m.ftp, &mut state, false, &mut reveal), None);
            reveal = true;
            assert_eq!(draw_owner_editor(ui, "f.json", owner, &m.ftp, &mut state, true, &mut reveal), None);
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

    /// Pump the app's events until the window for `file` has nothing in flight: neither a change being written
    /// nor a file being read.
    fn wait_for_reload(app: &mut FleetApp, file: &str) {
        let ctx = egui::Context::default();
        for _ in 0..400 {
            app.poll_views(&ctx);
            if app.viewers.find_mut(file).is_some_and(|v| !v.reloading && !v.loading && v.saving.is_none()) {
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
        wait_for_reload(&mut app, &copy);
        let v = app.viewers.find_mut(&copy).unwrap();
        assert_eq!(v.edit_notice.as_ref().map(|(l, t)| (*l, t.as_str())), Some((Level::Info, "VibrationReduction: 2 -> 0")));
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
        wait_for_reload(&mut app, &copy);
        let v = app.viewers.find_mut(&copy).unwrap();
        let (level, text) = v.edit_notice.clone().unwrap();
        assert_eq!(level, Level::Error);
        assert!(text.contains("whole number"), "{text}");
        assert!(!v.reloading, "nothing changed, so nothing is read again");
        assert!(!v.edit_ui.has_text("property:kNkMAIDCapability_ElectronicVR"), "the field shows the stored value again");
        assert_eq!(std::fs::read(dir.path().join("snapshots").join(&copy)).unwrap(), bytes);
        // a change to what it already holds says so and does not reload
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: VR.into(), text: "2".into() });
        wait_for_reload(&mut app, &copy);
        let v = app.viewers.find_mut(&copy).unwrap();
        assert!(!v.reloading && v.edit_notice.as_ref().unwrap().1.starts_with("No change"), "{:?}", v.edit_notice);
    }

    // ── a change is written off the UI thread ──

    /// Stand in for the writer: each call reports the thread it runs on and the request, then waits to be released
    /// before it writes (with the real function), so a test sees the app while a change is in flight.
    struct Gate {
        release: std::sync::mpsc::Sender<()>,
        started: std::sync::mpsc::Receiver<(std::thread::ThreadId, ViewEdit)>,
    }

    impl Gate {
        fn install(app: &mut FleetApp) -> Gate {
            let (release, wait) = std::sync::mpsc::channel::<()>();
            let (started_tx, started) = std::sync::mpsc::channel();
            let wait = std::sync::Mutex::new(wait);
            let started_tx = std::sync::Mutex::new(started_tx);
            app.edit_runner = std::sync::Arc::new(move |dir: &Path, file: &str, req: &ViewEdit, now: &str| {
                started_tx.lock().unwrap().send((std::thread::current().id(), req.clone())).unwrap();
                wait.lock().unwrap().recv_timeout(std::time::Duration::from_secs(5)).map_err(|_| "the test never released the writer".to_string())?;
                apply_view_edit(dir, file, req, now)
            });
            Gate { release, started }
        }

        /// The writer has been called, and is waiting.
        fn started(&self) -> (std::thread::ThreadId, ViewEdit) {
            self.started.recv_timeout(std::time::Duration::from_secs(5)).expect("the writer was called")
        }

        fn release(&self) {
            self.release.send(()).unwrap();
        }
    }

    /// An edited copy of the stored snapshot, open and loaded, with the writer gated.
    fn gated_copy() -> (FleetApp, Gate, tempfile::TempDir, String, egui::Context) {
        let (mut app, _cmd_rx, dir, file) = app_with_snapshot();
        let ctx = egui::Context::default();
        app.start_edit_copy(&ctx, &file);
        let copy = app.viewers.open[0].file.clone();
        wait_for_reload(&mut app, &copy);
        let gate = Gate::install(&mut app);
        (app, gate, dir, copy, ctx)
    }

    fn stored(dir: &tempfile::TempDir, file: &str) -> Vec<u8> {
        std::fs::read(dir.path().join("snapshots").join(file)).unwrap()
    }

    #[test]
    fn a_change_is_written_off_the_ui_thread_and_is_not_claimed_until_the_thread_reports() {
        let (mut app, gate, dir, copy, ctx) = gated_copy();
        let before = stored(&dir, &copy);
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: VR.into(), text: "0".into() });
        let (thread, asked) = gate.started();
        assert_ne!(thread, std::thread::current().id(), "the writer runs on a thread of its own");
        assert_eq!(asked, ViewEdit::Property { name: VR.into(), text: "0".into() });
        // the call has returned and the writer is parked before it writes: the file is as it was, and the window says nothing was done
        assert_eq!(stored(&dir, &copy), before, "the calling thread did not write");
        let v = app.viewers.find_mut(&copy).unwrap();
        assert!(v.saving.is_some() && !v.controls_enabled(), "in flight, and the controls wait");
        assert!(v.edit_notice.is_none() && !v.reloading, "no claim that the change happened: {:?}", v.edit_notice);
        app.poll_views(&ctx);
        let v = app.viewers.find_mut(&copy).unwrap();
        assert!(v.saving.is_some() && v.edit_notice.is_none(), "pumping the events before the answer changes nothing");
        gate.release();
        wait_for_reload(&mut app, &copy);
        assert_ne!(stored(&dir, &copy), before, "now it is written");
        let v = app.viewers.find_mut(&copy).unwrap();
        assert!(v.controls_enabled());
        assert_eq!(v.edit_notice.as_ref().map(|(l, t)| (*l, t.as_str())), Some((Level::Info, "VibrationReduction: 2 -> 0")));
    }

    #[test]
    fn changes_are_applied_in_the_order_made_with_at_most_one_in_flight_per_window() {
        let (mut app, gate, dir, copy, ctx) = gated_copy();
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: VR.into(), text: "0".into() });
        gate.started();
        // a second change made while the first is in flight is refused, not run beside it and not queued
        let mid = stored(&dir, &copy);
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: VR.into(), text: "1".into() });
        assert!(gate.started.try_recv().is_err(), "the writer was not called for the second change");
        let v = app.viewers.find_mut(&copy).unwrap();
        let (level, text) = v.edit_notice.clone().expect("it says so");
        assert_eq!(level, Level::Warn);
        assert!(text.contains("still being saved"), "{text}");
        assert_eq!(stored(&dir, &copy), mid);
        gate.release();
        wait_for_reload(&mut app, &copy);
        // and once the first is done the second goes through, after it
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: VR.into(), text: "1".into() });
        gate.started();
        gate.release();
        wait_for_reload(&mut app, &copy);
        let m = app.viewers.find_mut(&copy).unwrap().edit.clone().unwrap();
        assert_eq!(m.controls[VR].control, Control::Choice { values: vec![0, 1, 2], current: Some(1) });
        let log: Vec<&str> = m.derived.edits.iter().map(|e| e.change.as_str()).collect();
        assert_eq!(log, ["VibrationReduction: 2 -> 0", "VibrationReduction: 0 -> 1"], "in the order they were made");
    }

    #[test]
    fn the_controls_are_disabled_while_the_file_is_read_and_while_a_change_is_written() {
        let (mut app, gate, _dir, copy, ctx) = gated_copy();
        let v = app.viewers.find_mut(&copy).unwrap();
        assert!(v.controls_enabled(), "idle");
        v.reloading = true;
        assert!(!v.controls_enabled(), "being read again");
        v.reloading = false;
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: VR.into(), text: "0".into() });
        gate.started();
        assert!(!app.viewers.find_mut(&copy).unwrap().controls_enabled(), "being written");
        // a window drawn in that state asks for nothing; the wiring of `controls_enabled` into each widget is driven by input in
        // `the_window_hands_its_enabled_state_to_every_editor_so_a_saving_window_cannot_be_typed_into`
        let mut asked = Vec::new();
        run_ui(|ui| asked.push(crate::draw_snapshot_viewer(ui, app.viewers.find_mut(&copy).unwrap())));
        assert!(asked.iter().all(Option::is_none), "{asked:?}");
        gate.release();
        wait_for_reload(&mut app, &copy);
        assert!(app.viewers.find_mut(&copy).unwrap().controls_enabled(), "idle again");
    }

    /// What the whole window asks for when focus is moved with Tab to its `tabs`-th focusable widget, a character is
    /// typed and Enter pressed. Disabled widgets take no focus, so a widget that was handed `enabled = false` can
    /// never answer here. (A click needs the widget's place on screen; the Tab walk needs nothing but the window.)
    fn edits_asked_after_tabs(viewer: &crate::SnapshotViewer, tabs: usize) -> Vec<ViewEdit> {
        let mut v = viewer.clone();
        let mut frames: Vec<Vec<egui::Event>> = vec![vec![], vec![]];
        for _ in 0..tabs {
            frames.push(key(egui::Key::Tab));
        }
        frames.push(vec![egui::Event::Text("7".to_string())]);
        frames.push(key(egui::Key::Enter));
        frames.push(vec![]);
        drive(frames, |ui, _| crate::draw_snapshot_viewer(ui, &mut v)).into_iter().flatten().collect()
    }

    #[test]
    fn the_window_hands_its_enabled_state_to_every_editor_so_a_saving_window_cannot_be_typed_into() {
        let (mut app, _gate, _dir, copy, _ctx) = gated_copy();
        let idle = app.viewers.find_mut(&copy).unwrap().clone();
        assert!(idle.controls_enabled() && idle.edit.as_ref().is_some_and(|m| m.owner.is_some()), "an edited copy with an owner section");

        // The positive control: walk the idle window until the keyboard has reached each of the three kinds of editor the
        // window draws (a property cell, an owner/IPTC/FTP text field, an FTP row button). The silence below is then the
        // window's doing and not a harness that cannot type.
        let (mut property, mut owner, mut button) = (None, None, None);
        for tabs in 0..60 {
            for e in edits_asked_after_tabs(&idle, tabs) {
                match e {
                    ViewEdit::Property { .. } => property = property.or(Some(tabs)),
                    ViewEdit::OwnerString { .. } | ViewEdit::Iptc { .. } | ViewEdit::FtpFields { .. } => owner = owner.or(Some(tabs)),
                    ViewEdit::FtpMove { .. } | ViewEdit::FtpDuplicate(_) | ViewEdit::TestFtp { .. } | ViewEdit::FtpDelete(_) | ViewEdit::FtpAdd { .. } => button = button.or(Some(tabs)),
                    _ => {}
                }
            }
            if property.is_some() && owner.is_some() && button.is_some() {
                break;
            }
        }
        assert!(property.is_some(), "a property cell answers the keyboard in an idle window");
        assert!(owner.is_some(), "a text field of the owner editor answers the keyboard in an idle window");
        assert!(button.is_some(), "an FTP row button answers the keyboard in an idle window");

        // A change in flight, and a file being read again. Disabled widgets take no focus, so the stops are not the
        // idle window's (the same count lands on another widget): walk EVERY stop of the much shorter list. Focus
        // wraps around, so 20 is more than enough to visit whatever a mis-wired section would have left focusable
        // (a window of this size has three FTP rows of four buttons and a handful of fields).
        let mut saving = idle.clone();
        saving.saving = Some(1);
        assert!(!saving.controls_enabled());
        let mut reading = idle;
        reading.reloading = true;
        assert!(!reading.controls_enabled());
        for tabs in 0..20 {
            assert_eq!(edits_asked_after_tabs(&saving, tabs), vec![], "after {tabs} tabs: a window with a change in flight asks for a change");
            assert_eq!(edits_asked_after_tabs(&reading, tabs), vec![], "after {tabs} tabs: a window being read again asks for a change");
        }
    }

    #[test]
    fn an_answer_for_a_window_that_was_closed_or_closed_and_opened_again_is_dropped() {
        let (mut app, gate, _dir, copy, ctx) = gated_copy();
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: VR.into(), text: "0".into() });
        gate.started();
        // closed while the change is being written
        assert!(app.viewers.close(&copy));
        gate.release();
        for _ in 0..40 {
            app.poll_views(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(app.viewers.find_mut(&copy).is_none(), "nothing reopened it");
        assert!(app.status.level != Level::Error, "{}", app.status.text);
        // opened again, then the old change's answer turns up: it is not this window's
        app.open_snapshot(&ctx, &copy, false, false);
        wait_for_reload(&mut app, &copy);
        let fresh = app.viewers.find_mut(&copy).unwrap().clone();
        app.edit_tx
            .send(crate::ViewEditDone { file: copy.clone(), ticket: 1, req: ViewEdit::Property { name: VR.into(), text: "0".into() }, result: Ok(edit::Applied { changed: true, item: "i".into(), change: "stray".into() }) })
            .unwrap();
        app.poll_views(&ctx);
        let v = app.viewers.find_mut(&copy).unwrap();
        assert!(v.edit_notice.is_none() && !v.reloading && v.saving.is_none(), "{:?}", v.edit_notice);
        assert_eq!(*v, fresh, "the stray answer changed nothing");
        // and while a newer change is in flight, an older ticket's answer is not taken for it
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: VR.into(), text: "1".into() });
        gate.started();
        let waiting = app.viewers.find_mut(&copy).unwrap().saving.unwrap();
        app.edit_tx
            .send(crate::ViewEditDone { file: copy.clone(), ticket: waiting - 1, req: ViewEdit::Property { name: VR.into(), text: "0".into() }, result: Ok(edit::Applied { changed: true, item: "i".into(), change: "stray".into() }) })
            .unwrap();
        app.poll_views(&ctx);
        let v = app.viewers.find_mut(&copy).unwrap();
        assert_eq!(v.saving, Some(waiting), "still waiting for its own answer");
        assert!(v.edit_notice.is_none());
        gate.release();
        wait_for_reload(&mut app, &copy);
    }

    #[test]
    fn a_change_the_writer_refuses_or_cannot_save_is_a_notice_and_nothing_is_read_again() {
        let (mut app, _cmd_rx, dir, file) = app_with_snapshot();
        let ctx = egui::Context::default();
        app.start_edit_copy(&ctx, &file);
        let copy = app.viewers.open[0].file.clone();
        wait_for_reload(&mut app, &copy);
        app.edit_runner = std::sync::Arc::new(|_: &Path, _: &str, _: &ViewEdit, _: &str| Err("the disk is full".to_string()));
        let before = stored(&dir, &copy);
        app.viewers.find_mut(&copy).unwrap().edit_ui.texts.insert(format!("property:{VR}"), "typed".into());
        app.apply_viewer_edit(&ctx, &copy, ViewEdit::Property { name: VR.into(), text: "0".into() });
        wait_for_reload(&mut app, &copy);
        let v = app.viewers.find_mut(&copy).unwrap();
        assert_eq!(v.edit_notice, Some((Level::Error, "the disk is full".to_string())));
        assert!(v.controls_enabled() && !v.reloading, "usable again, and nothing to read");
        assert!(!v.edit_ui.has_text(&format!("property:{VR}")), "the field shows the stored value again");
        assert_eq!(stored(&dir, &copy), before);
    }

    #[test]
    fn a_change_to_a_window_whose_file_is_a_capture_is_refused_by_the_library() {
        let (mut app, _cmd_rx, dir, file) = app_with_snapshot();
        let ctx = egui::Context::default();
        app.open_snapshot(&ctx, &file, false, false);
        wait_for_reload(&mut app, &file);
        let before = std::fs::read(dir.path().join("snapshots").join(&file)).unwrap();
        app.apply_viewer_edit(&ctx, &file, ViewEdit::Property { name: VR.into(), text: "0".into() });
        wait_for_reload(&mut app, &file);
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

    // ── FTP profile fields ──

    fn copy_of_captured() -> Snapshot {
        nikon_fleet::snapshot_edit::copy_for_edit(&captured(), "captured.json", None, NOW)
    }

    #[test]
    fn the_model_carries_each_profiles_fields_and_says_why_a_list_or_a_profile_cannot_be_edited() {
        let m = EditModel::build(&copy_of_captured(), &no_labels()).unwrap();
        let rows = m.ftp.as_ref().unwrap();
        let FtpRow::Fields(first) = &rows[0] else { panic!("{rows:?}") };
        assert_eq!((first.name.as_str(), first.host.as_str(), first.username.as_str(), first.port, first.protocol), ("alpha", "h", "u", 21, Protocol::Ftp));
        assert_eq!(rows.len(), 3);
        assert!(!format!("{rows:?}").contains("\"pw\""), "secrets are not in Debug output");
        // a profile that does not decode can be moved and deleted but not edited
        let mut broken = copy_of_captured();
        if let Some(Extended { ftp: FtpState::Read(st), .. }) = &mut broken.extended {
            st.profiles[1].hex = "deadbeef".into();
        }
        let m = EditModel::build(&broken, &no_labels()).unwrap();
        let rows = m.ftp.as_ref().unwrap();
        assert!(matches!(&rows[0], FtpRow::Fields(_)) && matches!(&rows[1], FtpRow::Unreadable { why, .. } if why.contains("decode")) && matches!(&rows[2], FtpRow::Fields(_)));
        // lists that are not there
        let mut none = copy_of_captured();
        none.extended.as_mut().unwrap().ftp = FtpState::NotOnModel;
        assert!(EditModel::build(&none, &no_labels()).unwrap().ftp.unwrap_err().contains("no FTP"));
        let mut bare = copy_of_captured();
        bare.extended = None;
        assert!(EditModel::build(&bare, &no_labels()).unwrap().ftp.is_err());
    }

    #[test]
    fn profile_changes_are_applied_through_the_library_and_the_request_that_names_a_field_names_its_text_box() {
        let (d, file) = data_dir_with(&captured());
        let copy = make_edit_copy(d.path(), &file, NOW).unwrap();
        let one = |f: ProfileFields| ViewEdit::FtpFields { index: 0, fields: f };
        for (req, expect) in [
            (one(ProfileFields { host: Some("files.example.org".into()), ..Default::default() }), "host changed"),
            (one(ProfileFields { port: Some(22), protocol: Some(Protocol::Sftp), ..Default::default() }), "port, protocol changed"),
            (ViewEdit::FtpAdd { fields: ProfileFields { name: Some("fresh".into()), host: Some("h.example".into()), ..Default::default() } }, "\"fresh\" added"),
            (ViewEdit::FtpDuplicate(0), "\"alpha copy\" duplicated"),
            (ViewEdit::FtpDuplicate(3), "\"gamma copy\" duplicated"),
        ] {
            let a = apply_view_edit(d.path(), &copy, &req, NOW).unwrap_or_else(|e| panic!("{req:?}: {e}"));
            assert!(a.change.contains(expect), "{req:?}: {}", a.change);
        }
        let s = Snapshot::load_from_file(d.path().join("snapshots").join(&copy)).unwrap();
        let FtpState::Read(st) = &s.extended.unwrap().ftp else { panic!() };
        assert_eq!(st.profiles.len(), 6);
        // refusals and invalid typing are errors with the reason, writing nothing
        let bytes = std::fs::read(d.path().join("snapshots").join(&copy)).unwrap();
        let e = apply_view_edit(d.path(), &copy, &one(ProfileFields { wifi_name: Some("café".into()), ..Default::default() }), NOW).unwrap_err();
        assert!(e.contains("ASCII"), "{e}");
        let e = apply_view_edit(d.path(), &copy, &ViewEdit::Invalid { key: "ftp:0:port".into(), why: "\"x\" is not a port".into() }, NOW).unwrap_err();
        assert!(e.contains("not a port"));
        assert!(apply_view_edit(d.path(), &copy, &ViewEdit::TestFtp { index: 0, file_check: true }, NOW).is_err(), "a test is not a change to the file");
        assert_eq!(std::fs::read(d.path().join("snapshots").join(&copy)).unwrap(), bytes);
        // the text box a field was typed in
        assert_eq!(field_key(&one(ProfileFields { password: Some("x".into()), ..Default::default() })).as_deref(), Some("ftp:0:password"));
        assert_eq!(field_key(&ViewEdit::FtpFields { index: 3, fields: ProfileFields { proxy_port: Some(1), ..Default::default() } }).as_deref(), Some("ftp:3:proxy_port"));
        assert_eq!(field_key(&ViewEdit::Invalid { key: "ftp:2:port".into(), why: String::new() }).as_deref(), Some("ftp:2:port"));
        assert_eq!(field_key(&ViewEdit::FtpAdd { fields: ProfileFields::default() }), None, "a refused add keeps what was typed");
        assert_eq!(field_key(&ViewEdit::TestFtp { index: 0, file_check: true }), None);
        assert_eq!(ftp_field_name(&ProfileFields::default()), None);
    }

    #[test]
    fn a_secret_being_set_is_not_in_the_requests_debug_output() {
        let dump = format!("{:?}", ViewEdit::FtpFields { index: 0, fields: ProfileFields { password: Some("hunter2-secret".into()), host: Some("h.example".into()), ..Default::default() } });
        assert!(!dump.contains("hunter2-secret") && dump.contains("h.example"), "{dump}");
    }

    fn ftp_row(host: &str) -> ProfileView {
        ProfileView { name: "alpha".into(), wifi_name: "w".into(), wifi_key: "k".into(), host: host.into(), folder: String::new(), username: "u".into(), password: "pw".into(), port: 21, protocol: Protocol::Ftp, proxy_port: 8080 }
    }

    /// Focus the first text box with Tab, type `typed`, press Enter, and return what was asked for on each frame.
    fn edit_first_box(stored: &ProfileView, typed: &str) -> Vec<Option<ViewEdit>> {
        let mut frames: Vec<Vec<egui::Event>> = vec![vec![], key(egui::Key::Tab)];
        frames.push(vec![egui::Event::Text(typed.to_string())]);
        frames.push(key(egui::Key::Enter));
        frames.push(vec![]);
        let mut state = EditUi::default();
        // the fields are inside a collapsing header in the full editor; draw them directly here
        drive(frames, |ui, _| draw_ftp_fields(ui, "f.json", 0, stored, &mut state, true, false))
    }

    #[test]
    fn typing_in_a_profile_field_and_pressing_enter_asks_for_that_field_only() {
        let asked = edit_first_box(&ftp_row("h"), "Z");
        let requests: Vec<&ViewEdit> = asked.iter().flatten().collect();
        assert_eq!(requests.len(), 1, "one request, on Enter: {asked:?}");
        let ViewEdit::FtpFields { index: 0, fields } = requests[0] else { panic!("{requests:?}") };
        assert_eq!(ftp_field_name(fields), Some("name"), "the first box is the profile name");
        assert!(fields.name.as_deref().is_some_and(|n| n.contains('Z') && n != "alpha"), "{fields:?}");
        assert!(fields.host.is_none() && fields.password.is_none(), "nothing but the field that was typed in");
        // pressing Enter without typing asks for nothing
        let asked = edit_first_box(&ftp_row("h"), "");
        assert!(asked.iter().all(Option::is_none), "{asked:?}");
    }

    #[test]
    fn a_port_that_is_not_a_number_is_reported_a_good_one_is_asked_for_and_an_unchanged_one_is_not() {
        let commit = |typed: &str, stored: u16| -> Vec<Result<u16, ViewEdit>> {
            let mut state = EditUi::default();
            state.texts.insert("ftp:0:port".into(), typed.to_string());
            let frames = vec![vec![], key(egui::Key::Tab), key(egui::Key::Enter), vec![]];
            drive(frames, |ui, _| port_field(ui, &mut state, "ftp:0:port", stored, true)).into_iter().flatten().collect()
        };
        let bad = commit("twenty", 21);
        assert!(matches!(bad.as_slice(), [Err(ViewEdit::Invalid { key, why })] if key == "ftp:0:port" && why.contains("not a port")), "{bad:?}");
        for text in ["65536", "-1", "", "2e1", "21.5"] {
            assert!(matches!(commit(text, 21).as_slice(), [Err(ViewEdit::Invalid { .. })]), "{text:?}");
        }
        for (text, port) in [("2222", 2222u16), (" 22 ", 22), ("0", 0), ("65535", 65535)] {
            assert!(matches!(commit(text, 21).as_slice(), [Ok(p)] if *p == port), "{text:?}");
        }
        assert!(commit("21", 21).is_empty(), "what is already stored is not a request");
    }

    /// Tab to the `n`th box (1 is the first), type `typed`, press Enter: what was asked for.
    fn edit_nth_box(stored: &ProfileView, n: usize, typed: &str) -> Vec<ViewEdit> {
        let mut frames: Vec<Vec<egui::Event>> = vec![vec![]];
        frames.extend((0..n).map(|_| key(egui::Key::Tab)));
        frames.push(vec![egui::Event::Text(typed.to_string())]);
        frames.push(key(egui::Key::Enter));
        frames.push(vec![]);
        let mut state = EditUi::default();
        drive(frames, |ui, _| draw_ftp_fields(ui, "f.json", 0, stored, &mut state, true, false)).into_iter().flatten().collect()
    }

    #[test]
    fn each_box_of_a_profile_asks_for_its_own_field() {
        let v = ftp_row("h");
        let want = ["name", "host", "folder", "username", "password", "wifi_name", "wifi_key", "port"];
        for (i, field) in want.iter().enumerate() {
            let typed = if *field == "port" { "9" } else { "Q" };
            let got = edit_nth_box(&v, i + 1, typed);
            let [ViewEdit::FtpFields { index: 0, fields }] = got.as_slice() else { panic!("box {} ({field}): {got:?}", i + 1) };
            assert_eq!(ftp_field_name(fields), Some(*field), "box {}: {fields:?}", i + 1);
        }
    }

    // ── testing a profile through the app ──

    fn copy_with_profile_at(dir: &Path, port: u16, protocol: Protocol) -> String {
        use nikon_fleet::ftp_edit::ftp_add;
        let mut s = nikon_fleet::snapshot_edit::copy_for_edit(&captured(), "captured.json", None, NOW);
        if let Some(Extended { ftp: FtpState::Read(st), .. }) = &mut s.extended {
            st.profiles.clear();
            st.count = 0;
            st.active_slot = None;
        }
        let f = ProfileFields {
            name: Some("local".into()),
            host: Some("127.0.0.1".into()),
            username: Some("fleet".into()),
            password: Some("s3cret!".into()),
            folder: Some("incoming".into()),
            port: Some(port),
            protocol: Some(protocol),
            ..Default::default()
        };
        ftp_add(&mut s, &f, None, NOW).unwrap();
        std::fs::create_dir_all(dir.join("snapshots")).unwrap();
        let name = "copy.json".to_string();
        s.save_to_file(dir.join("snapshots").join(&name)).unwrap();
        name
    }

    /// An FTP server that knows login, PWD and CWD.
    fn mini_ftp(pass: &'static str) -> u16 {
        use std::io::{BufRead, BufReader, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for conn in l.incoming().flatten() {
                std::thread::spawn(move || {
                    let mut w = conn.try_clone().unwrap();
                    let mut r = BufReader::new(conn);
                    let say = |w: &mut std::net::TcpStream, t: &str| {
                        let _ = w.write_all(format!("{t}\r\n").as_bytes());
                    };
                    say(&mut w, "220 mini");
                    let mut line = String::new();
                    let mut good = false;
                    loop {
                        line.clear();
                        if r.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let c = line.trim_end();
                        let (v, a) = c.split_once(' ').unwrap_or((c, ""));
                        match v.to_uppercase().as_str() {
                            "USER" => {
                                good = a == "fleet";
                                say(&mut w, "331 password");
                            }
                            "PASS" => say(&mut w, if good && a == pass { "230 ok" } else { "530 Login incorrect." }),
                            "TYPE" => say(&mut w, "200 ok"),
                            "PWD" => say(&mut w, "257 \"/\" current"),
                            "CWD" => say(&mut w, if a == "incoming" { "250 ok" } else { "550 No such file or directory." }),
                            "QUIT" => {
                                say(&mut w, "221 bye");
                                return;
                            }
                            _ => say(&mut w, "502 no"),
                        }
                    }
                });
            }
        });
        port
    }

    fn wait_for_test(app: &mut FleetApp, file: &str, index: usize) -> FtpTestState {
        for _ in 0..400 {
            app.poll_views(&egui::Context::default());
            let st = app.viewers.find_mut(file).and_then(|v| v.edit_ui.ftp_tests.get(&index).cloned());
            match st {
                Some(FtpTestState::Running) | None => std::thread::sleep(std::time::Duration::from_millis(25)),
                Some(done) => return done,
            }
        }
        panic!("the test of profile {index} never finished");
    }

    #[test]
    fn the_test_button_runs_the_stored_profile_against_its_server_and_shows_the_result() {
        let port = mini_ftp("s3cret!");
        let (mut app, _cmd_rx, _evt_tx, dir) = crate::tests::test_app();
        let name = copy_with_profile_at(dir.path(), port, Protocol::Ftp);
        let ctx = egui::Context::default();
        app.open_snapshot(&ctx, &name, false, false);
        wait_for_reload(&mut app, &name);
        app.apply_viewer_edit(&ctx, &name, ViewEdit::TestFtp { index: 0, file_check: false });
        assert!(matches!(app.viewers.find_mut(&name).unwrap().edit_ui.ftp_tests.get(&0), Some(FtpTestState::Running)), "it shows as running at once");
        let FtpTestState::Done(report) = wait_for_test(&mut app, &name, 0) else { panic!("a finished test") };
        assert!(report.ok, "{}", report.to_text());
        assert_eq!((report.host.as_str(), report.port), ("127.0.0.1", port));
        assert!(!format!("{report:?}").contains("s3cret!"));
        // dismissing it removes it
        app.apply_viewer_edit(&ctx, &name, ViewEdit::DismissFtpTest(0));
        assert!(app.viewers.find_mut(&name).unwrap().edit_ui.ftp_tests.is_empty());
    }

    #[test]
    fn a_test_of_a_server_that_is_not_there_shows_the_failed_step_and_the_wrong_password_shows_the_servers_words() {
        let (mut app, _cmd_rx, _evt_tx, dir) = crate::tests::test_app();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let name = copy_with_profile_at(dir.path(), closed, Protocol::Sftp);
        let ctx = egui::Context::default();
        app.open_snapshot(&ctx, &name, false, false);
        wait_for_reload(&mut app, &name);
        app.apply_viewer_edit(&ctx, &name, ViewEdit::TestFtp { index: 0, file_check: true });
        let FtpTestState::Done(r) = wait_for_test(&mut app, &name, 0) else { panic!() };
        assert!(!r.ok && matches!(r.failure().map(|f| f.step), Some(nikon_fleet::ftp_test::Step::Connect)), "{}", r.to_text());
        // a profile that does not exist
        app.apply_viewer_edit(&ctx, &name, ViewEdit::TestFtp { index: 7, file_check: true });
        let FtpTestState::Failed(why) = wait_for_test(&mut app, &name, 7) else { panic!("could not be run") };
        assert!(why.contains("does not exist"), "{why}");
    }

    #[test]
    fn the_file_check_choice_is_what_the_button_asks_for() {
        let mut st = EditUi::default();
        assert!(st.file_check_wanted(), "on unless turned off");
        assert_eq!(st.test_request(3), ViewEdit::TestFtp { index: 3, file_check: true });
        st.want_file_check(false);
        assert!(!st.file_check_wanted() && st.skip_file_check());
        assert_eq!(st.test_request(0), ViewEdit::TestFtp { index: 0, file_check: false });
        st.want_file_check(true);
        assert_eq!(st.test_request(1), ViewEdit::TestFtp { index: 1, file_check: true });
    }

    #[test]
    fn a_result_for_a_test_the_window_is_not_waiting_for_is_dropped() {
        let (mut app, _cmd_rx, _evt_tx, dir) = crate::tests::test_app();
        let name = copy_with_profile_at(dir.path(), 1, Protocol::Ftp);
        let ctx = egui::Context::default();
        app.open_snapshot(&ctx, &name, false, false);
        wait_for_reload(&mut app, &name);
        let report = nikon_fleet::ftp_test::run_test(&nikon_fleet::ftp_test::TestRequest::from_profile(&ftp_row("no-such-host.invalid"), false, std::time::Duration::from_millis(300)));
        // nothing was started: a result that turns up anyway (from before a reload, say) must not appear
        app.ftp_test_tx.send(crate::FtpTestDone { file: name.clone(), index: 0, result: Ok(report.clone()) }).unwrap();
        app.poll_views(&egui::Context::default());
        assert!(app.viewers.find_mut(&name).unwrap().edit_ui.ftp_tests.is_empty());
        // one that is waited for is shown
        app.viewers.find_mut(&name).unwrap().edit_ui.ftp_tests.insert(0, FtpTestState::Running);
        app.ftp_test_tx.send(crate::FtpTestDone { file: name.clone(), index: 0, result: Ok(report) }).unwrap();
        app.poll_views(&egui::Context::default());
        assert!(matches!(app.viewers.find_mut(&name).unwrap().edit_ui.ftp_tests.get(&0), Some(FtpTestState::Done(_))));
        // a result for a window that was closed is ignored
        app.ftp_test_tx.send(crate::FtpTestDone { file: "closed.json".into(), index: 0, result: Err("x".into()) }).unwrap();
        app.poll_views(&egui::Context::default());
    }

    #[test]
    fn a_reload_forgets_tests_and_a_result_that_arrives_for_a_forgotten_one_is_dropped() {
        let port = mini_ftp("s3cret!");
        let (mut app, _cmd_rx, _evt_tx, dir) = crate::tests::test_app();
        let name = copy_with_profile_at(dir.path(), port, Protocol::Ftp);
        let ctx = egui::Context::default();
        app.open_snapshot(&ctx, &name, false, false);
        wait_for_reload(&mut app, &name);
        app.apply_viewer_edit(&ctx, &name, ViewEdit::TestFtp { index: 0, file_check: false });
        // the file is changed (and reloaded) while the test runs: the list may be different now
        app.apply_viewer_edit(&ctx, &name, ViewEdit::FtpFields { index: 0, fields: ProfileFields { folder: Some("incoming".into()), host: Some("localhost".into()), ..Default::default() } });
        wait_for_reload(&mut app, &name);
        std::thread::sleep(std::time::Duration::from_millis(600));
        app.poll_views(&egui::Context::default());
        assert!(app.viewers.find_mut(&name).unwrap().edit_ui.ftp_tests.is_empty(), "the reload cleared it and the late result did not bring it back");
        // the choice about the test file survives a reload
        let v = app.viewers.find_mut(&name).unwrap();
        v.edit_ui.skip_file_check = true;
        v.edit_ui.clear();
        assert!(v.edit_ui.skip_file_check());
    }

    #[test]
    fn every_state_of_a_test_draws() {
        let mut report = nikon_fleet::ftp_test::run_test(&nikon_fleet::ftp_test::TestRequest::from_profile(&ftp_row("no-such-host.invalid"), true, std::time::Duration::from_millis(500)));
        report.file_left_behind = Some("nikon-fleet-test-1.txt".into());
        report.host_key = Some("SHA256:abc".into());
        let states = [FtpTestState::Running, FtpTestState::Failed("nothing".into()), FtpTestState::Done(Box::new(report))];
        run_ui(|ui| {
            for st in &states {
                let _ = draw_ftp_test(ui, st);
            }
            let m = EditModel::build(&copy_of_captured(), &no_labels()).unwrap();
            let mut state = EditUi::default();
            for (i, st) in states.iter().enumerate() {
                state.ftp_tests.insert(i, st.clone());
            }
            let mut reveal = true;
            assert_eq!(draw_owner_editor(ui, "f.json", m.owner.as_ref().unwrap(), &m.ftp, &mut state, true, &mut reveal), None);
        });
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
