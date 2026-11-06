//! Changing, adding and duplicating FTP profiles in an edited copy of a snapshot.
//!
//! A profile is stored as the blob the camera returned. An edit decodes it, changes the fields asked for and writes
//! it back with `ptp_usb::encode_decoded_ftp_profile`, which keeps every field it was not asked to change (the
//! camera address, the option bits it does not know, the timestamps), so a profile read from a camera and edited in
//! one field differs from it in that field only. A new profile starts from a template: the encoder's own default,
//! or, for a duplicate, the profile it copies.
//!
//! Only fields whose meaning is known are offered: the name, the Wi-Fi name, the Wi-Fi key (stored in what the
//! encoder calls the 5 GHz SSID field; **probably** a passphrase, unconfirmed against the camera's menu), the
//! server, the folder, the login, the server port, FTP or SFTP (option bit `0x0008`, confirmed on one profile), and
//! the second port (probably a proxy port). The other option bits are kept as they are.
//!
//! What a camera does with a profile that has a changed port, folder or protocol is **not verified**: reading one
//! back from a camera that took it is the check, and has not been done. The checks here are the encoder's own limits
//! and character rules, and "has a name and a server".
//!
//! **Required fields.** `vendor-write-ftp` refuses a profile that lacks any of six fields
//! ([`ptp_usb::ftp_profile_missing_fields`], the one list). The editor deliberately does not: the cameras make
//! profiles without a login (two of the real blobs in `tests/fixtures/ftp_profile_vectors.json` have an empty
//! username and password), so refusing them here, or when a restore plans them, would refuse profiles a camera made.
//! What the editor does instead is say so: when a profile it adds or edits lacks something `vendor-write-ftp` would
//! require, the edit's result says which ([`write_gate_note`]). Whether a camera accepts such a profile written back
//! is **not verified**.
//!
//! **A new profile's camera address.** [`template`] used to be the encoder's blank profile, which carries the capture
//! rig's camera address (192.168.66.87/24), so every profile added here (and every restore that wrote one) carried
//! another person's camera address. It now has NO stored camera address (0.0.0.0/0) and the 2.4 GHz tag kind byte
//! `0x30`, the form of the one camera-made profile in `tests/fixtures/ftp_profile_vectors.json` that has none
//! (`hotspot_no_address`); a body's own profiles have their own addresses (192.168.66.87/24, 192.168.68.83/22,
//! 10.0.0.251/24 in the fixtures). Option bits stay `0x0005`, the ports 21 and 8080. A duplicate copies its source whole,
//! its camera address included. That a camera accepts, lists and connects with a profile that has no address is
//! **not verified**; a profile added here says so.

use crate::extended::{hex_decode, hex_encode};
use crate::ptp_usb::{DecodedFtpProfile, FtpField, FtpProfile, PtpUsbError, decode_ftp_profile, encode_decoded_ftp_profile, encode_ftp_profile, ftp_profile_missing_fields};
use crate::snapshot::Snapshot;
use crate::snapshot_edit::{Applied, EditRefusal, ensure_editable, finish, ftp_store, ftp_store_mut, renumber};

/// The option bit that makes a profile SFTP (confirmed on one profile whose menu says SFTP).
pub const SFTP_BIT: u16 = 0x0008;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Ftp,
    Sftp,
}

/// The fields of a profile a person edits. `None` leaves a field as it is; for a new profile, as the template has it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct ProfileFields {
    pub name: Option<String>,
    /// The Wi-Fi name (the 2.4 GHz SSID field).
    pub wifi_name: Option<String>,
    /// The Wi-Fi key (the field the encoder calls the 5 GHz SSID; probably a passphrase). A secret.
    pub wifi_key: Option<String>,
    /// The server's host name or address.
    pub host: Option<String>,
    /// The destination folder on the server; empty for none.
    pub folder: Option<String>,
    pub username: Option<String>,
    /// A secret.
    pub password: Option<String>,
    /// The server's port (21 for FTP, 22 for SFTP by convention).
    pub port: Option<u16>,
    pub protocol: Option<Protocol>,
    /// The second port (probably a proxy port; 8080 in nearly every profile seen).
    pub proxy_port: Option<u16>,
}

impl std::fmt::Debug for ProfileFields {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hidden = |s: &Option<String>| s.as_ref().map(|t| format!("<{} byte(s) hidden>", t.len()));
        f.debug_struct("ProfileFields")
            .field("name", &self.name)
            .field("wifi_name", &self.wifi_name)
            .field("wifi_key", &hidden(&self.wifi_key))
            .field("host", &self.host)
            .field("folder", &self.folder)
            .field("username", &self.username)
            .field("password", &hidden(&self.password))
            .field("port", &self.port)
            .field("protocol", &self.protocol)
            .field("proxy_port", &self.proxy_port)
            .finish()
    }
}

/// Every field of a profile as the editor shows it, secrets included (the caller masks them).
#[derive(Clone, PartialEq, Eq)]
pub struct ProfileView {
    pub name: String,
    pub wifi_name: String,
    pub wifi_key: String,
    pub host: String,
    pub folder: String,
    pub username: String,
    pub password: String,
    pub port: u16,
    pub protocol: Protocol,
    pub proxy_port: u16,
}

impl std::fmt::Debug for ProfileView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileView")
            .field("name", &self.name)
            .field("wifi_name", &self.wifi_name)
            .field("wifi_key", &format_args!("<{} byte(s) hidden>", self.wifi_key.len()))
            .field("host", &self.host)
            .field("folder", &self.folder)
            .field("username", &self.username)
            .field("password", &format_args!("<{} byte(s) hidden>", self.password.len()))
            .field("port", &self.port)
            .field("protocol", &self.protocol)
            .field("proxy_port", &self.proxy_port)
            .finish()
    }
}

impl ProfileView {
    pub fn of(d: &DecodedFtpProfile) -> ProfileView {
        ProfileView {
            name: d.profile_name.clone(),
            wifi_name: d.ssid_24ghz.clone(),
            wifi_key: d.ssid_5ghz.clone(),
            host: d.host.clone(),
            folder: d.folder.clone(),
            username: d.username.clone(),
            password: d.password.clone(),
            port: d.ftp_port,
            protocol: if d.is_sftp() { Protocol::Sftp } else { Protocol::Ftp },
            proxy_port: d.second_port,
        }
    }
}

fn bad(why: impl Into<String>) -> EditRefusal {
    EditRefusal::BadValue { item: "FTP profile".to_string(), why: why.into() }
}

/// What changing a field of a profile to something the encoder will not write says, in words for a person.
fn encoder_words(e: &PtpUsbError) -> String {
    match e {
        PtpUsbError::FtpFieldTooLong { field, len, max, unit } => format!("{field} is {len} {unit}; at most {max} fit"),
        PtpUsbError::FtpFieldControlChar { field } => format!("{field} contains a control character (a tab, a line break, NUL ...)"),
        PtpUsbError::FtpFieldNotAscii { field } => format!("{field} may only hold ASCII characters"),
        other => other.to_string(),
    }
}

/// The text fields whose wire form holds ASCII only, by [`FtpField`]. The encoder leaves this rule to the caller (a
/// profile a camera made may hold more than it writes), so this check is the only judge of it; the question itself is
/// the layout's (`FtpEncoding::encoder_ascii_only`), asked here and not re-spelled.
fn ascii_only(field: FtpField) -> bool {
    field.spec().encoding().encoder_ascii_only()
}

/// The names of the required fields of `d` that are empty, by the list `vendor-write-ftp` applies
/// ([`ftp_profile_missing_fields`]); empty when it has them all.
pub fn missing_for_write(d: &DecodedFtpProfile) -> Vec<&'static str> {
    ftp_profile_missing_fields(&d.profile_name, &d.ssid_24ghz, &d.ssid_5ghz, &d.host, &d.username, &d.password)
}

/// Words for an edit's result when it leaves `d` lacking required fields that `before` did not lack (all of them,
/// for a new profile: `before` is empty-handed there). Empty when there is nothing to say. Never holds a value.
fn write_gate_note(before: &[&str], d: &DecodedFtpProfile) -> String {
    let now: Vec<&str> = missing_for_write(d).into_iter().filter(|m| !before.contains(m)).collect();
    if now.is_empty() {
        String::new()
    } else {
        format!(
            "; note: vendor-write-ftp would refuse this profile (no {}); a restore does not check that, because cameras make such profiles",
            now.join(", ")
        )
    }
}

/// Apply `f` to `d`, naming the fields that changed (never their values). Checks the fields that change, and the
/// whole result against the encoder; `d` is left as it was on a refusal.
pub fn apply_fields(d: &mut DecodedFtpProfile, f: &ProfileFields) -> Result<Vec<&'static str>, EditRefusal> {
    let mut next = d.clone();
    let mut changed: Vec<&'static str> = Vec::new();
    let mut text = |label: &'static str, field: Option<FtpField>, slot: &mut String, new: &Option<String>| -> Result<(), EditRefusal> {
        let Some(new) = new else { return Ok(()) };
        if new == slot {
            return Ok(());
        }
        if let Some(field) = field {
            if ascii_only(field) && !new.is_ascii() {
                return Err(bad(format!("{label} may only hold ASCII characters")));
            }
        }
        if new.chars().any(char::is_control) {
            return Err(bad(format!("{label} contains a control character (a tab, a line break, NUL ...)")));
        }
        *slot = new.clone();
        changed.push(label);
        Ok(())
    };
    text("name", Some(FtpField::ProfileName), &mut next.profile_name, &f.name)?;
    text("Wi-Fi name", Some(FtpField::Ssid24Ghz), &mut next.ssid_24ghz, &f.wifi_name)?;
    text("Wi-Fi key", Some(FtpField::Ssid5Ghz), &mut next.ssid_5ghz, &f.wifi_key)?;
    text("host", Some(FtpField::Host), &mut next.host, &f.host)?;
    text("folder", None, &mut next.folder, &f.folder)?;
    text("username", Some(FtpField::Username), &mut next.username, &f.username)?;
    text("password", Some(FtpField::Password), &mut next.password, &f.password)?;
    if let Some(port) = f.port {
        if port == 0 {
            return Err(bad("the port must be 1 to 65535"));
        }
        if port != next.ftp_port {
            next.ftp_port = port;
            changed.push("port");
        }
    }
    if let Some(p) = f.protocol {
        let flags = match p {
            Protocol::Sftp => next.option_flags | SFTP_BIT,
            Protocol::Ftp => next.option_flags & !SFTP_BIT,
        };
        if flags != next.option_flags {
            next.option_flags = flags;
            changed.push("protocol");
        }
    }
    if let Some(p) = f.proxy_port {
        if p != next.second_port {
            next.second_port = p;
            changed.push("proxy port");
        }
    }
    if next.profile_name.trim().is_empty() {
        return Err(bad("a profile needs a name"));
    }
    if next.host.trim().is_empty() {
        return Err(bad("a profile needs a server (host)"));
    }
    // The encoder is the judge of lengths and of what it will write at all.
    encode_decoded_ftp_profile(&next).map_err(|e| bad(encoder_words(&e)))?;
    *d = next;
    Ok(changed)
}

/// The 2.4 GHz SSID tag's kind byte that goes with "no stored camera address" in the one camera-made profile that has
/// none (`hotspot_no_address` in `tests/fixtures/ftp_profile_vectors.json`: tag `03 00 30 00`, where the other five
/// fixtures have `03 00 01 00`). Whether the byte is connected to the missing address or to something else about that
/// profile (it is a hotspot profile) is **unknown**; it travels with the address here because that is how the one
/// example is.
pub const NO_ADDRESS_SSID24_KIND: u8 = 0x30;

/// The camera network a new profile starts with: none stored (0.0.0.0/0), the form a camera itself writes for a
/// profile with no address (`hotspot_no_address`).
pub const NO_CAMERA_NETWORK: crate::ptp_usb::FtpCameraNetwork = crate::ptp_usb::FtpCameraNetwork { ipv4: [0, 0, 0, 0], prefix_len: 0 };

/// The profile a new one starts from: the encoder's blank profile (FTP port 21, second port 8080, option bits `0x0005`,
/// empty folder), with NO stored camera address (0.0.0.0/0) and the 2.4 GHz tag kind byte the camera-made no-address
/// profile carries ([`NO_ADDRESS_SSID24_KIND`]), where it used to carry the capture rig's address (192.168.66.87/24).
///
/// `encode_ftp_profile` writes only the captured form, so this is the encoder's blank profile, decoded, with those two
/// values set, and written back through the byte-exact path (`encode_decoded_ftp_profile`) to prove the strict
/// decoder takes the result. The combination "no address + 5 GHz tag kind 4 + 2.4 GHz kind 0x30" is not itself in a
/// camera-made fixture (`hotspot_no_address` has 5 GHz kind 6, option bits 7 and second port 0), but the decoder
/// accepts any value in those tag bytes, so it is the nearest form the fixtures support while the rest keeps its
/// documented defaults. **Not verified on a camera**: whether a body lists and connects with a profile added this way.
///
/// This is only the starting point of a NEW profile. A duplicate copies its source whole, camera address included.
pub fn template() -> DecodedFtpProfile {
    let blank = FtpProfile {
        profile_name: String::new(),
        ssid_24ghz: String::new(),
        ssid_5ghz: String::new(),
        host: String::new(),
        port: 8080,
        username: String::new(),
        password: String::new(),
    };
    let blob = encode_ftp_profile(&blank).expect("an empty profile always encodes");
    let mut d = decode_ftp_profile(&blob).expect("what the encoder writes always decodes");
    d.camera_network = NO_CAMERA_NETWORK;
    d.ssid24_kind = NO_ADDRESS_SSID24_KIND;
    // The write-back path is the one a snapshot edit and a restore use; the strict decoder must take what it writes.
    let written = encode_decoded_ftp_profile(&d).expect("a blank profile with the no-address form always encodes");
    decode_ftp_profile(&written).expect("what the byte-exact encoder writes always decodes")
}

fn decoded_at(snap: &Snapshot, index: usize) -> Result<(DecodedFtpProfile, usize), EditRefusal> {
    let store = ftp_store(snap)?;
    let len = store.profiles.len();
    let p = store.profiles.get(index).ok_or(EditRefusal::FtpIndex { index, len })?;
    let d = hex_decode(&p.hex)
        .map_err(|e| e.to_string())
        .and_then(|b| decode_ftp_profile(&b).map_err(|e| e.to_string()))
        .map_err(|why| EditRefusal::FtpProfileUnreadable { index, why })?;
    Ok((d, len))
}

/// The fields of the profile at `index`, for the editor to show. Reads any snapshot, a capture included.
pub fn profile_view(snap: &Snapshot, index: usize) -> Result<ProfileView, EditRefusal> {
    decoded_at(snap, index).map(|(d, _)| ProfileView::of(&d))
}

/// Change fields of the profile at `index`.
pub fn ftp_set_fields(snap: &mut Snapshot, index: usize, f: &ProfileFields, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let (mut d, _) = decoded_at(snap, index)?;
    // `d` decoded, so its name is known; no need to encode it and decode it again to find it.
    let label = format!("{:?}", d.profile_name);
    let missing_before = missing_for_write(&d);
    let changed = apply_fields(&mut d, f)?;
    let item = format!("ftp:edit:{index}");
    if changed.is_empty() {
        return Ok(Applied { changed: false, item, change: format!("FTP profile {label} already has these values") });
    }
    let blob = encode_decoded_ftp_profile(&d).map_err(|e| bad(encoder_words(&e)))?;
    let store = ftp_store_mut(snap)?;
    store.profiles[index].hex = hex_encode(&blob);
    let renamed = if changed.contains(&"name") { format!(" (now {:?})", d.profile_name) } else { String::new() };
    let change = format!("FTP profile {label}{renamed}: {} changed{}", changed.join(", "), write_gate_note(&missing_before, &d));
    Ok(finish(snap, now, Applied { changed: true, item, change }))
}

fn insert_profile(snap: &mut Snapshot, d: &DecodedFtpProfile, at: usize, what: &str, note: &str, now: &str) -> Result<Applied, EditRefusal> {
    let blob = encode_decoded_ftp_profile(d).map_err(|e| bad(encoder_words(&e)))?;
    let store = ftp_store_mut(snap)?;
    if store.profiles.len() >= store.limit as usize {
        return Err(EditRefusal::FtpListFull { limit: store.limit, len: store.profiles.len() });
    }
    let at = at.min(store.profiles.len());
    let was_empty = store.profiles.is_empty();
    store.profiles.insert(at, crate::extended::FtpProfileEntry { slot: 0, hex: hex_encode(&blob) });
    renumber(store);
    if was_empty {
        // An empty list reports no active profile; one with a profile reports slot 0 (every reply seen).
        store.active_slot = Some(0);
    }
    let change = format!("FTP profile {:?} {what} at position {at}; {} in the list{note}", d.profile_name, store.profiles.len());
    Ok(finish(snap, now, Applied { changed: true, item: "ftp:add".to_string(), change }))
}

/// Add a profile at `position` (the end when `None`), starting from [`template`]. `f` must give a name and a host.
pub fn ftp_add(snap: &mut Snapshot, f: &ProfileFields, position: Option<usize>, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let len = ftp_store_mut(snap)?.profiles.len();
    let mut d = template();
    apply_fields(&mut d, f)?;
    // Nothing of the template is the person's: say that it has no camera address (unverified on a camera).
    let note = format!(
        "{}; it has no stored camera address and the default option bits, which the camera is expected to fill in (unverified on a camera)",
        write_gate_note(&[], &d)
    );
    insert_profile(snap, &d, position.unwrap_or(len), "added", &note, now)
}

/// Copy the profile at `index` to the position after it, under `name` (default: its name and " copy"), optionally
/// with other fields changed too. Everything else, the Wi-Fi name and key included, is the original's.
pub fn ftp_duplicate(snap: &mut Snapshot, index: usize, name: Option<&str>, changes: &ProfileFields, now: &str) -> Result<Applied, EditRefusal> {
    ensure_editable(snap)?;
    let (mut d, _) = decoded_at(snap, index)?;
    let new_name = name.map(str::to_string).or_else(|| changes.name.clone()).unwrap_or_else(|| format!("{} copy", d.profile_name));
    let f = ProfileFields { name: Some(new_name), ..changes.clone() };
    let missing_before = missing_for_write(&d);
    apply_fields(&mut d, &f)?;
    insert_profile(snap, &d, index + 1, "duplicated", &write_gate_note(&missing_before, &d), now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extended::{Extended, FtpProfileEntry, FtpState, FtpStore, IptcState, ReadFrom};
    use crate::snapshot::{Camera, Transport};
    use crate::snapshot_edit::copy_for_edit;

    const NOW: &str = "2026-11-06T12:00:00Z";

    fn profile(name: &str) -> FtpProfileEntry {
        let blob = encode_ftp_profile(&FtpProfile {
            profile_name: name.into(),
            ssid_24ghz: "home-net".into(),
            ssid_5ghz: "wifi-key-12345".into(),
            host: "files.example.org".into(),
            username: "fleet".into(),
            password: "hunter2-secret".into(),
            port: 8080,
        })
        .unwrap();
        FtpProfileEntry { slot: 0, hex: hex_encode(&blob) }
    }

    fn snapshot_with(names: &[&str], limit: u32) -> Snapshot {
        let mut s = Snapshot::new(Camera { model: "Z 9".into(), serial: "S1".into(), firmware: "5.31".into() }, Transport::Usb, "2026-11-05T10:00:00Z".into());
        s.extended = Some(Extended {
            read_from: ReadFrom { model: "Z 9".into(), usb_serial: "S1".into() },
            props: Default::default(),
            ftp: FtpState::Read(FtpStore {
                count: names.len() as u32,
                limit,
                active_slot: (!names.is_empty()).then_some(0),
                device_name: "Z 9_S1".into(),
                truncated: false,
                profiles: names.iter().enumerate().map(|(i, n)| FtpProfileEntry { slot: i as u32, ..profile(n) }).collect(),
            }),
            iptc: IptcState::NotOnModel,
        });
        copy_for_edit(&s, "s.json", None, NOW)
    }

    fn view(s: &mut Snapshot, i: usize) -> ProfileView {
        profile_view(s, i).unwrap()
    }

    fn names(s: &Snapshot) -> Vec<String> {
        let Some(Extended { ftp: FtpState::Read(st), .. }) = &s.extended else { panic!() };
        st.profiles.iter().map(|p| decode_ftp_profile(&hex_decode(&p.hex).unwrap()).unwrap().profile_name).collect()
    }

    fn log(s: &Snapshot) -> String {
        serde_json::to_string(&s.derived_from).unwrap()
    }

    // ── changing fields ──

    #[test]
    fn changing_one_field_changes_that_field_and_the_log_names_it_without_its_value() {
        let mut s = snapshot_with(&["alpha", "beta"], 20);
        let before = view(&mut s, 0);
        let a = ftp_set_fields(&mut s, 0, &ProfileFields { password: Some("a-new-secret".into()), ..Default::default() }, NOW).unwrap();
        assert_eq!(a.change, "FTP profile \"alpha\": password changed");
        let after = view(&mut s, 0);
        assert_eq!(ProfileView { password: before.password.clone(), ..after.clone() }, before, "nothing but the password differs");
        assert_eq!(after.password, "a-new-secret");
        assert!(!log(&s).contains("a-new-secret") && !log(&s).contains("hunter2-secret"), "no secret in the log: {}", log(&s));
        assert_eq!(view(&mut s, 1), view(&mut snapshot_with(&["alpha", "beta"], 20), 1), "the other profile is untouched");
    }

    #[test]
    fn every_field_can_be_changed_and_reads_back_as_set() {
        let mut s = snapshot_with(&["alpha"], 20);
        let f = ProfileFields {
            name: Some("studio".into()),
            wifi_name: Some("studio-wifi".into()),
            wifi_key: Some("studio-key".into()),
            host: Some("ftp.studio.test".into()),
            folder: Some("/incoming/raw".into()),
            username: Some("brian".into()),
            password: Some("s3cret pass".into()),
            port: Some(22),
            protocol: Some(Protocol::Sftp),
            proxy_port: Some(3128),
        };
        let a = ftp_set_fields(&mut s, 0, &f, NOW).unwrap();
        assert!(a.change.contains("name, Wi-Fi name, Wi-Fi key, host, folder, username, password, port, protocol, proxy port"), "{}", a.change);
        let v = view(&mut s, 0);
        assert_eq!(
            v,
            ProfileView {
                name: "studio".into(),
                wifi_name: "studio-wifi".into(),
                wifi_key: "studio-key".into(),
                host: "ftp.studio.test".into(),
                folder: "/incoming/raw".into(),
                username: "brian".into(),
                password: "s3cret pass".into(),
                port: 22,
                protocol: Protocol::Sftp,
                proxy_port: 3128
            }
        );
        // asking for SFTP again on an SFTP profile is not a toggle
        let again = ftp_set_fields(&mut s, 0, &ProfileFields { protocol: Some(Protocol::Sftp), ..Default::default() }, NOW).unwrap();
        assert!(!again.changed && view(&mut s, 0).protocol == Protocol::Sftp, "{again:?}");
        // and back to FTP: the SFTP bit goes and the other option bits stay
        let before_flags = {
            let Some(Extended { ftp: FtpState::Read(st), .. }) = &s.extended else { panic!() };
            decode_ftp_profile(&hex_decode(&st.profiles[0].hex).unwrap()).unwrap().option_flags
        };
        assert_eq!(before_flags & SFTP_BIT, SFTP_BIT);
        ftp_set_fields(&mut s, 0, &ProfileFields { protocol: Some(Protocol::Ftp), ..Default::default() }, NOW).unwrap();
        let Some(Extended { ftp: FtpState::Read(st), .. }) = &s.extended else { panic!() };
        let after = decode_ftp_profile(&hex_decode(&st.profiles[0].hex).unwrap()).unwrap();
        assert_eq!(after.option_flags, before_flags & !SFTP_BIT, "only the SFTP bit moved");
    }

    #[test]
    fn what_the_edit_does_not_touch_is_kept_as_the_camera_wrote_it() {
        let mut s = snapshot_with(&["alpha"], 20);
        let original = {
            let Some(Extended { ftp: FtpState::Read(st), .. }) = &s.extended else { panic!() };
            decode_ftp_profile(&hex_decode(&st.profiles[0].hex).unwrap()).unwrap()
        };
        ftp_set_fields(&mut s, 0, &ProfileFields { host: Some("elsewhere.example".into()), ..Default::default() }, NOW).unwrap();
        let Some(Extended { ftp: FtpState::Read(st), .. }) = &s.extended else { panic!() };
        let now = decode_ftp_profile(&hex_decode(&st.profiles[0].hex).unwrap()).unwrap();
        assert_eq!(DecodedFtpProfile { host: original.host.clone(), ..now }, original, "the camera address, timestamps, tag bytes and option bits are the original's");
    }

    #[test]
    fn setting_what_it_already_holds_is_not_a_change() {
        let mut s = snapshot_with(&["alpha"], 20);
        let v = view(&mut s, 0);
        let same = ProfileFields { name: Some(v.name), host: Some(v.host), port: Some(v.port), protocol: Some(v.protocol), ..Default::default() };
        let bytes = serde_json::to_string(&s).unwrap();
        let a = ftp_set_fields(&mut s, 0, &same, NOW).unwrap();
        assert!(!a.changed);
        assert_eq!(serde_json::to_string(&s).unwrap(), bytes, "nothing recorded, nothing rewritten");
        assert!(!ftp_set_fields(&mut s, 0, &ProfileFields::default(), NOW).unwrap().changed, "an empty request");
    }

    #[test]
    fn a_field_the_encoder_will_not_write_is_refused_with_the_reason_and_the_profile_is_unchanged() {
        let mut s = snapshot_with(&["alpha"], 20);
        let before = serde_json::to_string(&s).unwrap();
        let cases: [(ProfileFields, &str); 9] = [
            (ProfileFields { wifi_name: Some("caf\u{e9}".into()), ..Default::default() }, "ASCII"),
            (ProfileFields { password: Some("pä".into()), ..Default::default() }, "ASCII"),
            (ProfileFields { host: Some("a\nb".into()), ..Default::default() }, "control character"),
            (ProfileFields { folder: Some("/x\ty".into()), ..Default::default() }, "control character"),
            (ProfileFields { wifi_name: Some("x".repeat(33)), ..Default::default() }, "33"),
            (ProfileFields { host: Some("h".repeat(200)), ..Default::default() }, "at most 199"),
            (ProfileFields { name: Some("  ".into()), ..Default::default() }, "needs a name"),
            (ProfileFields { host: Some("".into()), ..Default::default() }, "needs a server"),
            (ProfileFields { port: Some(0), ..Default::default() }, "1 to 65535"),
        ];
        for (f, word) in cases {
            let e = ftp_set_fields(&mut s, 0, &f, NOW).unwrap_err();
            assert_eq!(e.tag(), "bad_value", "{f:?}");
            assert!(e.to_string().contains(word), "{f:?}: {e}");
        }
        assert_eq!(serde_json::to_string(&s).unwrap(), before);
    }

    #[test]
    fn the_limits_are_the_encoders_with_the_edge_allowed_and_one_more_refused() {
        let mut s = snapshot_with(&["alpha"], 20);
        ftp_set_fields(&mut s, 0, &ProfileFields { wifi_name: Some("x".repeat(32)), ..Default::default() }, NOW).unwrap();
        assert!(ftp_set_fields(&mut s, 0, &ProfileFields { wifi_name: Some("x".repeat(33)), ..Default::default() }, NOW).is_err());
        ftp_set_fields(&mut s, 0, &ProfileFields { host: Some("h".repeat(199)), ..Default::default() }, NOW).unwrap();
        assert!(ftp_set_fields(&mut s, 0, &ProfileFields { host: Some("h".repeat(200)), ..Default::default() }, NOW).is_err());
        ftp_set_fields(&mut s, 0, &ProfileFields { port: Some(65535), proxy_port: Some(0), ..Default::default() }, NOW).unwrap();
        ftp_set_fields(&mut s, 0, &ProfileFields { port: Some(1), ..Default::default() }, NOW).unwrap();
    }

    #[test]
    fn a_profile_that_is_not_there_or_does_not_decode_or_a_capture_is_refused() {
        let mut s = snapshot_with(&["alpha"], 20);
        let f = ProfileFields { name: Some("x".into()), ..Default::default() };
        assert_eq!(ftp_set_fields(&mut s, 1, &f, NOW).unwrap_err().tag(), "ftp_index");
        if let Some(Extended { ftp: FtpState::Read(st), .. }) = &mut s.extended {
            st.profiles[0].hex = "deadbeef".into();
        }
        assert_eq!(ftp_set_fields(&mut s, 0, &f, NOW).unwrap_err().tag(), "ftp_profile_unreadable");
        let mut capture = snapshot_with(&["alpha"], 20);
        capture.derived_from = None;
        assert_eq!(ftp_set_fields(&mut capture, 0, &f, NOW).unwrap_err().tag(), "not_a_copy");
        assert_eq!(ftp_add(&mut capture, &f, None, NOW).unwrap_err().tag(), "not_a_copy");
        assert_eq!(ftp_duplicate(&mut capture, 0, None, &ProfileFields::default(), NOW).unwrap_err().tag(), "not_a_copy");
    }

    // ── adding and duplicating ──

    fn new_profile() -> ProfileFields {
        ProfileFields { name: Some("fresh".into()), host: Some("ftp.fresh.test".into()), username: Some("u".into()), password: Some("p".into()), ..Default::default() }
    }

    #[test]
    fn a_new_profile_starts_from_the_encoders_template_and_goes_where_asked() {
        let mut s = snapshot_with(&["alpha", "beta"], 20);
        let a = ftp_add(&mut s, &new_profile(), Some(1), NOW).unwrap();
        assert!(a.change.starts_with("FTP profile \"fresh\" added at position 1; 3 in the list; "), "{}", a.change);
        assert_eq!(names(&s), ["alpha", "fresh", "beta"]);
        let v = view(&mut s, 1);
        assert_eq!((v.host.as_str(), v.port, v.protocol, v.proxy_port, v.folder.as_str()), ("ftp.fresh.test", 21, Protocol::Ftp, 8080, ""), "the template's defaults");
        let Some(Extended { ftp: FtpState::Read(st), .. }) = &s.extended else { panic!() };
        assert_eq!((st.count, st.profiles.iter().map(|p| p.slot).collect::<Vec<_>>()), (3, vec![0, 1, 2]));
        ftp_add(&mut s, &new_profile(), None, NOW).unwrap();
        assert_eq!(names(&s).last().map(String::as_str), Some("fresh"), "the end when no position is given");
        ftp_add(&mut s, &ProfileFields { name: Some("far".into()), ..new_profile() }, Some(99), NOW).unwrap();
        assert_eq!(names(&s).last().map(String::as_str), Some("far"), "a position past the end is the end");
    }

    #[test]
    fn a_new_profile_needs_a_name_and_a_server_and_goes_into_an_empty_list() {
        let mut s = snapshot_with(&["alpha"], 20);
        assert!(ftp_add(&mut s, &ProfileFields { host: Some("h".into()), ..Default::default() }, None, NOW).unwrap_err().to_string().contains("needs a name"));
        assert!(ftp_add(&mut s, &ProfileFields { name: Some("n".into()), ..Default::default() }, None, NOW).unwrap_err().to_string().contains("needs a server"));
        assert_eq!(names(&s), ["alpha"]);
        let mut empty = snapshot_with(&[], 20);
        ftp_add(&mut empty, &new_profile(), None, NOW).unwrap();
        let Some(Extended { ftp: FtpState::Read(st), .. }) = &empty.extended else { panic!() };
        assert_eq!((st.count, st.active_slot), (1, Some(0)), "a list that was empty had no active profile; with one it reports slot 0");
        let mut had_one_but_none_active = snapshot_with(&["a"], 20);
        if let Some(Extended { ftp: FtpState::Read(st), .. }) = &mut had_one_but_none_active.extended {
            st.active_slot = None;
        }
        ftp_add(&mut had_one_but_none_active, &new_profile(), None, NOW).unwrap();
        let Some(Extended { ftp: FtpState::Read(st), .. }) = &had_one_but_none_active.extended else { panic!() };
        assert_eq!(st.active_slot, None, "stored profiles with none active stay that way: only an empty list gets slot 0");
    }

    #[test]
    fn the_camera_holds_a_limited_number_of_profiles() {
        let mut s = snapshot_with(&["a", "b"], 2);
        let e = ftp_add(&mut s, &new_profile(), None, NOW).unwrap_err();
        assert_eq!(e.tag(), "ftp_list_full");
        assert!(e.to_string().contains("at most 2"), "{e}");
        assert_eq!(ftp_duplicate(&mut s, 0, None, &ProfileFields::default(), NOW).unwrap_err().tag(), "ftp_list_full");
        let mut room = snapshot_with(&["a"], 2);
        ftp_add(&mut room, &new_profile(), None, NOW).unwrap();
        assert_eq!(names(&room).len(), 2, "one below the limit is room for one more (the boundary)");
        assert_eq!(ftp_add(&mut room, &new_profile(), None, NOW).unwrap_err().tag(), "ftp_list_full");
    }

    #[test]
    fn a_duplicate_is_the_original_under_a_new_name_right_after_it() {
        let mut s = snapshot_with(&["alpha", "beta"], 20);
        let a = ftp_duplicate(&mut s, 0, None, &ProfileFields::default(), NOW).unwrap();
        assert!(a.change.contains("\"alpha copy\" duplicated at position 1"), "{}", a.change);
        assert_eq!(names(&s), ["alpha", "alpha copy", "beta"]);
        let (orig, copy) = (view(&mut s, 0), view(&mut s, 1));
        assert_eq!(ProfileView { name: orig.name.clone(), ..copy.clone() }, orig, "everything else, the Wi-Fi name and key and the login included, is the original's");
        // with a name, and with other changes
        ftp_duplicate(&mut s, 2, Some("beta at studio"), &ProfileFields { host: Some("studio.test".into()), folder: Some("/in".into()), ..Default::default() }, NOW).unwrap();
        assert_eq!(names(&s), ["alpha", "alpha copy", "beta", "beta at studio"]);
        let v = view(&mut s, 3);
        assert_eq!((v.host.as_str(), v.folder.as_str(), v.wifi_key.as_str()), ("studio.test", "/in", "wifi-key-12345"));
        assert_eq!(ftp_duplicate(&mut s, 9, None, &ProfileFields::default(), NOW).unwrap_err().tag(), "ftp_index");
    }

    #[test]
    fn fields_being_set_hide_their_secrets_from_debug_output_too() {
        let f = ProfileFields { password: Some("hunter2-secret".into()), wifi_key: Some("key-12345".into()), host: Some("h.example".into()), ..Default::default() };
        let dump = format!("{f:?}");
        assert!(!dump.contains("hunter2-secret") && !dump.contains("key-12345") && dump.contains("h.example") && dump.contains("14 byte(s) hidden"), "{dump}");
    }

    #[test]
    fn a_profile_view_hides_its_secrets_from_debug_output() {
        let mut s = snapshot_with(&["alpha"], 20);
        let dump = format!("{:?}", view(&mut s, 0));
        for secret in ["hunter2-secret", "wifi-key-12345"] {
            assert!(!dump.contains(secret), "{dump}");
        }
        assert!(dump.contains("files.example.org"));
    }

    // ── the required-field list, the ASCII rule, the template ──

    fn decoded_of(s: &Snapshot, i: usize) -> DecodedFtpProfile {
        let Some(Extended { ftp: FtpState::Read(st), .. }) = &s.extended else { panic!() };
        decode_ftp_profile(&hex_decode(&st.profiles[i].hex).unwrap()).unwrap()
    }

    #[test]
    fn the_write_gates_required_fields_are_named_for_a_new_profile_that_lacks_them_and_not_for_one_that_has_them() {
        let mut s = snapshot_with(&["alpha"], 20);
        // name and host only: the other four of the six are missing, in the gate's own order and words
        let a = ftp_add(&mut s, &ProfileFields { name: Some("n".into()), host: Some("h".into()), ..Default::default() }, None, NOW).unwrap();
        assert!(a.change.contains("no SSID (2.4GHz), SSID (5GHz), username, password"), "{}", a.change);
        assert!(a.change.contains("a restore does not check that"), "{}", a.change);
        // all six: nothing to say about the gate
        let full = ProfileFields { wifi_name: Some("w".into()), wifi_key: Some("k".into()), ..new_profile() };
        let a = ftp_add(&mut s, &full, None, NOW).unwrap();
        assert!(!a.change.contains("vendor-write-ftp"), "{}", a.change);
        // the note never carries a value
        assert!(!a.change.contains("hunter2") && !log(&s).contains("hunter2"));
    }

    #[test]
    fn an_edit_is_told_about_a_required_field_it_emptied_and_not_about_one_the_profile_already_lacked() {
        let mut s = snapshot_with(&["alpha"], 20);
        // a profile the camera made without a login: edit the host, say nothing about the login it never had
        let anon = ProfileFields { username: Some("".into()), password: Some("".into()), ..Default::default() };
        let a = ftp_set_fields(&mut s, 0, &anon, NOW).unwrap();
        assert!(a.change.contains("no username, password"), "emptying them is the edit's doing: {}", a.change);
        let a = ftp_set_fields(&mut s, 0, &ProfileFields { host: Some("elsewhere.test".into()), ..Default::default() }, NOW).unwrap();
        assert!(!a.change.contains("vendor-write-ftp"), "the login was already absent: {}", a.change);
        // a copy of it lacks the same and is not told again; a copy that blanks one more field is
        let a = ftp_duplicate(&mut s, 0, None, &ProfileFields::default(), NOW).unwrap();
        assert!(!a.change.contains("vendor-write-ftp"), "{}", a.change);
        let a = ftp_duplicate(&mut s, 0, None, &ProfileFields { wifi_name: Some("".into()), ..Default::default() }, NOW).unwrap();
        assert!(a.change.contains("no SSID (2.4GHz)") && !a.change.contains("username"), "{}", a.change);
    }

    #[test]
    fn an_empty_text_is_a_value_and_none_is_no_request() {
        let mut s = snapshot_with(&["alpha"], 20);
        let a = ftp_set_fields(&mut s, 0, &ProfileFields { password: Some(String::new()), ..Default::default() }, NOW).unwrap();
        assert!(a.changed && a.change.contains("password changed"), "{}", a.change);
        assert_eq!(view(&mut s, 0).password, "", "Some(\"\") clears it");
        let none = ftp_set_fields(&mut s, 0, &ProfileFields { username: None, ..Default::default() }, NOW).unwrap();
        assert!(!none.changed && view(&mut s, 0).username == "fleet", "None leaves it");
        let again = ftp_set_fields(&mut s, 0, &ProfileFields { password: Some(String::new()), ..Default::default() }, NOW).unwrap();
        assert!(!again.changed, "clearing what is already empty is not a change");
    }

    /// The ASCII rule is the wire layout's: a text field refuses non-ASCII exactly where the layout says its form holds
    /// ASCII only, and takes it where the form is UTF-16 (the host, the folder). Asked of every text field the editor
    /// offers, against `FtpEncoding::encoder_ascii_only` as the independent judge.
    #[test]
    fn each_text_field_refuses_non_ascii_exactly_where_the_layout_says_its_form_is_ascii_only() {
        type Set = fn(&mut ProfileFields, String);
        let slots: [(&str, Option<FtpField>, Set); 7] = [
            ("name", Some(FtpField::ProfileName), |f, v| f.name = Some(v)),
            ("Wi-Fi name", Some(FtpField::Ssid24Ghz), |f, v| f.wifi_name = Some(v)),
            ("Wi-Fi key", Some(FtpField::Ssid5Ghz), |f, v| f.wifi_key = Some(v)),
            ("host", Some(FtpField::Host), |f, v| f.host = Some(v)),
            ("folder", None, |f, v| f.folder = Some(v)),
            ("username", Some(FtpField::Username), |f, v| f.username = Some(v)),
            ("password", Some(FtpField::Password), |f, v| f.password = Some(v)),
        ];
        for (label, field, set) in slots {
            let mut s = snapshot_with(&["alpha"], 20);
            let mut f = ProfileFields::default();
            set(&mut f, "caf\u{e9}".to_string());
            let r = ftp_set_fields(&mut s, 0, &f, NOW);
            let ascii_only_form = field.is_some_and(|fl| fl.spec().encoding().encoder_ascii_only());
            match r {
                Err(e) => {
                    assert!(ascii_only_form && e.to_string().contains("ASCII"), "{label}: refused but its form is not ASCII-only, or for another reason: {e}");
                }
                Ok(_) => assert!(!ascii_only_form, "{label}: its form is ASCII-only and the editor took non-ASCII"),
            }
        }
        // And the two UTF-16 ones read back what was typed.
        let mut s = snapshot_with(&["alpha"], 20);
        ftp_set_fields(&mut s, 0, &ProfileFields { folder: Some("/caf\u{e9}".into()), host: Some("h\u{e9}.test".into()), ..Default::default() }, NOW).unwrap();
        let v = view(&mut s, 0);
        assert_eq!((v.folder.as_str(), v.host.as_str()), ("/caf\u{e9}", "h\u{e9}.test"));
    }

    #[test]
    fn a_folder_longer_than_its_wire_form_holds_is_refused_by_the_encoder_with_its_name() {
        let mut s = snapshot_with(&["alpha"], 20);
        let before = serde_json::to_string(&s).unwrap();
        ftp_set_fields(&mut s, 0, &ProfileFields { folder: Some("f".repeat(254)), ..Default::default() }, NOW).unwrap();
        let e = ftp_set_fields(&mut s, 0, &ProfileFields { folder: Some("f".repeat(256)), ..Default::default() }, NOW).unwrap_err();
        assert!(e.to_string().contains("folder"), "{e}");
        assert_ne!(serde_json::to_string(&s).unwrap(), before, "the 254-unit folder was taken, the 256-unit one left it as it was");
        assert_eq!(view(&mut s, 0).folder.len(), 254);
    }

    #[test]
    fn the_sftp_bit_and_the_decoders_is_sftp_agree_for_every_option_bit() {
        let mut d = template();
        for bit in 0..16 {
            for set in [false, true] {
                d.option_flags = if set { 1u16 << bit } else { 0xFFFF & !(1u16 << bit) };
                assert_eq!(d.is_sftp(), d.option_flags & SFTP_BIT != 0, "flags {:#06x}", d.option_flags);
            }
        }
    }

    /// A camera-made blob from the shared golden fixtures, by name.
    fn fixture_blob(name: &str) -> Vec<u8> {
        let v: serde_json::Value = serde_json::from_str(include_str!("../tests/fixtures/ftp_profile_vectors.json")).unwrap();
        let p = v["profiles"].as_array().unwrap().iter().find(|p| p["name"] == name).unwrap_or_else(|| panic!("no fixture {name}"));
        hex_decode(p["hex"].as_str().unwrap()).unwrap()
    }

    fn set_profile(s: &mut Snapshot, i: usize, d: &DecodedFtpProfile) {
        if let Some(Extended { ftp: FtpState::Read(st), .. }) = &mut s.extended {
            st.profiles[i].hex = hex_encode(&encode_decoded_ftp_profile(d).unwrap());
        }
    }

    const NO_NETWORK: crate::ptp_usb::FtpCameraNetwork = crate::ptp_usb::FtpCameraNetwork { ipv4: [0, 0, 0, 0], prefix_len: 0 };

    #[test]
    fn a_new_profile_starts_with_no_stored_camera_address_where_it_used_to_carry_the_capture_rigs() {
        // Finding 30: the template carried 192.168.66.87/24 (the capture rig's Z 9) into every added profile and every
        // restore that wrote one. The no-address form is the one the camera itself writes for `hotspot_no_address`.
        let t = template();
        assert_eq!(t.camera_network, NO_NETWORK);
        assert_ne!(t.camera_network, crate::ptp_usb::FtpCameraNetwork::CAPTURED, "not the capture rig's address");
        assert_eq!(t.ssid24_kind, 0x30, "the kind byte the camera-made no-address profile carries");
        assert_eq!(t.ssid24_lead, 0x03);
        // Everything else keeps the documented defaults.
        assert_eq!((t.option_flags, t.ftp_port, t.second_port, t.ssid5_kind, t.folder.as_str()), (0x0005, 21, 8080, 4, ""));
        let mut s = snapshot_with(&["alpha"], 20);
        ftp_add(&mut s, &new_profile(), None, NOW).unwrap();
        assert_eq!(decoded_of(&s, 1).camera_network, NO_NETWORK);
        assert_eq!(decoded_of(&s, 1).ssid24_kind, 0x30);
        assert_eq!(decoded_of(&s, 0).camera_network, crate::ptp_usb::FtpCameraNetwork::CAPTURED, "the existing profile is untouched");
    }

    #[test]
    fn the_no_address_form_matches_the_camera_made_profile_that_has_none() {
        // The independent judge: the real blob. Same network, same 2.4 GHz tag kind, and the layout's strict decoder reads
        // both; the template differs from it only where the template keeps documented defaults (5 GHz kind, option bits, ports).
        let real = decode_ftp_profile(&fixture_blob("hotspot_no_address")).unwrap();
        let t = template();
        assert_eq!((t.camera_network, t.ssid24_kind, t.ssid24_lead), (real.camera_network, real.ssid24_kind, real.ssid24_lead));
        assert_eq!((real.camera_network, real.ssid24_kind), (NO_NETWORK, 0x30), "the fixture is what this test says it is");
        // And the other camera-made profiles all have an address and kind 1: the no-address form is the odd one out, by design.
        for other in ["reference", "z9_317", "z9_331_folder", "sftp_port22", "hotspot_anonymous"] {
            let d = decode_ftp_profile(&fixture_blob(other)).unwrap();
            assert_ne!(d.camera_network, NO_NETWORK, "{other}");
            assert_eq!(d.ssid24_kind, 1, "{other}");
        }
    }

    #[test]
    fn the_template_encodes_decodes_and_round_trips_byte_for_byte() {
        let t = template();
        let blob = encode_decoded_ftp_profile(&t).unwrap();
        let back = decode_ftp_profile(&blob).expect("the strict decoder takes the no-address form");
        assert_eq!(back, t);
        assert_eq!(encode_decoded_ftp_profile(&back).unwrap(), blob, "decode then encode is byte-identical");
        // With the person's fields applied too (the profile a restore would write).
        let mut filled = template();
        apply_fields(&mut filled, &new_profile()).unwrap();
        let blob = encode_decoded_ftp_profile(&filled).unwrap();
        let back = decode_ftp_profile(&blob).unwrap();
        assert_eq!((back.camera_network, back.ssid24_kind), (NO_NETWORK, 0x30));
        assert_eq!(encode_decoded_ftp_profile(&back).unwrap(), blob);
    }

    #[test]
    fn the_vendor_write_ftp_gate_is_unchanged_and_still_wants_the_captured_form_while_a_restore_takes_the_template() {
        // `vendor-write-ftp` builds a profile from scratch with the encoder, which writes the captured network; its gate
        // (`as_encodable`) therefore refuses a profile with any other network and any other tag kind. The restore path
        // writes decoded blobs and asks only that the strict decoder reads them. Both behaviours are pinned here.
        let mut filled = template();
        apply_fields(&mut filled, &new_profile()).unwrap();
        assert!(filled.as_encodable().is_none(), "the vendor-write-ftp gate wants 192.168.66.87/24 and tag kind 1");
        let mut as_captured = filled.clone();
        as_captured.camera_network = crate::ptp_usb::FtpCameraNetwork::CAPTURED;
        as_captured.ssid24_kind = 0x01;
        assert!(as_captured.as_encodable().is_some(), "the gate is unchanged: captured network and kind 1 pass");
        // Each of the three differences alone refuses it.
        let mut only_address = as_captured.clone();
        only_address.camera_network.ipv4 = [0, 0, 0, 0];
        let mut only_prefix = as_captured.clone();
        only_prefix.camera_network.prefix_len = 0;
        let mut only_kind = as_captured.clone();
        only_kind.ssid24_kind = 0x30;
        for (what, d) in [("address", only_address), ("prefix", only_prefix), ("2.4 GHz kind", only_kind)] {
            assert!(d.as_encodable().is_none(), "{what} alone must refuse it");
        }
        let blob = encode_decoded_ftp_profile(&filled).unwrap();
        assert!(decode_ftp_profile(&blob).is_ok(), "the restore path's check (the strict decoder) takes the template");
    }

    #[test]
    fn adding_to_an_empty_list_and_to_a_non_empty_one_both_start_from_no_address() {
        let mut empty = snapshot_with(&[], 20);
        ftp_add(&mut empty, &new_profile(), None, NOW).unwrap();
        assert_eq!(decoded_of(&empty, 0).camera_network, NO_NETWORK);
        let mut s = snapshot_with(&["a", "b"], 20);
        ftp_add(&mut s, &new_profile(), Some(1), NOW).unwrap();
        assert_eq!(names(&s), ["a", "fresh", "b"]);
        assert_eq!(decoded_of(&s, 1).camera_network, NO_NETWORK);
        for i in [0, 2] {
            assert_eq!(decoded_of(&s, i).camera_network, crate::ptp_usb::FtpCameraNetwork::CAPTURED, "profile {i} keeps its own");
        }
    }

    #[test]
    fn the_add_result_says_there_is_no_camera_address_and_that_it_is_unverified() {
        let mut s = snapshot_with(&["a"], 20);
        let a = ftp_add(&mut s, &new_profile(), None, NOW).unwrap();
        assert!(a.change.contains("no stored camera address") && a.change.contains("unverified on a camera"), "{}", a.change);
        assert!(!a.change.contains("template's"), "the old wording is gone: {}", a.change);
    }

    #[test]
    fn a_duplicate_keeps_its_sources_own_camera_address_and_tag_kind_whatever_it_is() {
        // 10.0.0.251/24: one of the camera-made addresses in the fixtures.
        let mut s = snapshot_with(&["alpha"], 20);
        let mut other = decoded_of(&s, 0);
        other.camera_network = crate::ptp_usb::FtpCameraNetwork { ipv4: [10, 0, 0, 251], prefix_len: 24 };
        set_profile(&mut s, 0, &other);
        ftp_duplicate(&mut s, 0, None, &ProfileFields::default(), NOW).unwrap();
        assert_eq!(decoded_of(&s, 1).camera_network, other.camera_network);
        assert_eq!((decoded_of(&s, 1).ssid24_kind, decoded_of(&s, 1).option_flags), (1, decoded_of(&s, 0).option_flags));
        // A duplicate of a no-address profile has none; one of the rig's profile has the rig's: the source decides.
        let mut none = decoded_of(&s, 0);
        none.camera_network = NO_NETWORK;
        none.ssid24_kind = 0x30;
        set_profile(&mut s, 0, &none);
        ftp_duplicate(&mut s, 0, Some("again"), &ProfileFields::default(), NOW).unwrap();
        assert_eq!((decoded_of(&s, 1).camera_network, decoded_of(&s, 1).ssid24_kind), (NO_NETWORK, 0x30));
        // The real fixture with the camera-made address, through the same path.
        let mut real = snapshot_with(&["x"], 20);
        if let Some(Extended { ftp: FtpState::Read(st), .. }) = &mut real.extended {
            st.profiles[0].hex = hex_encode(&fixture_blob("hotspot_anonymous"));
        }
        let want = decode_ftp_profile(&fixture_blob("hotspot_anonymous")).unwrap().camera_network;
        ftp_duplicate(&mut real, 0, None, &ProfileFields::default(), NOW).unwrap();
        assert_eq!(decoded_of(&real, 1).camera_network, want);
        assert_ne!(want, NO_NETWORK);
    }
}
