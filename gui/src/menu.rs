//! The menu bar: File, Edit, Camera, Firmware, Help.
//!
//! `menus` says what is in each menu and `MenuAction::label` what each item is called. Both
//! renderers consume them: the native macOS bar (`native`, muda — Apple menu > Settings…) and the
//! in-window egui bar every other platform (and macOS, if the native bar cannot be installed) draws
//! with `FleetApp::ui_menu_bar`. Neither re-encodes the layout.

/// Everything a menu item can do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MenuAction {
    Export,
    Import,
    Quit,
    Preferences,
    Discover,
    Transplant,
    Restore,
    Check,
    VendorOps,
    Firmware,
    About,
}

impl MenuAction {
    #[cfg(test)]
    pub(crate) const ALL: [MenuAction; 11] = [
        MenuAction::Export,
        MenuAction::Import,
        MenuAction::Quit,
        MenuAction::Preferences,
        MenuAction::Discover,
        MenuAction::Transplant,
        MenuAction::Restore,
        MenuAction::Check,
        MenuAction::VendorOps,
        MenuAction::Firmware,
        MenuAction::About,
    ];

    pub(crate) fn label(self, macos: bool) -> &'static str {
        match self {
            MenuAction::Export => "Export All…",
            MenuAction::Import => "Import Archive…",
            MenuAction::Quit => "Quit",
            MenuAction::Preferences if macos => "Settings…",
            MenuAction::Preferences => "Preferences…",
            MenuAction::Discover => "Discover Cameras",
            MenuAction::Transplant => "Transplant…",
            MenuAction::Restore => "Restore…",
            MenuAction::Check => "Check Against Reference…",
            MenuAction::VendorOps => "Vendor Ops…",
            MenuAction::Firmware => "Firmware Archive…",
            MenuAction::About => "About Fleet",
        }
    }
}

pub(crate) enum Entry {
    Item(MenuAction),
    Separator,
}

pub(crate) struct Menu {
    pub(crate) title: &'static str,
    pub(crate) entries: Vec<Entry>,
}

/// What macOS puts in the application menu (the one named after the app) instead of in a menu of
/// ours. `menus(true)` leaves these out; `native` builds them there.
pub(crate) const MAC_APP_MENU: [MenuAction; 3] = [MenuAction::About, MenuAction::Preferences, MenuAction::Quit];

/// The menus, left to right. macOS has no Edit or Help of ours: its Edit would hold nothing (the
/// text fields are egui's, not the system's, so the system's Copy/Paste items would not reach
/// them), and Settings, About and Quit sit in the application menu.
pub(crate) fn menus(macos: bool) -> Vec<Menu> {
    use Entry::{Item, Separator};
    let mut file = vec![Item(MenuAction::Export), Item(MenuAction::Import)];
    if !macos {
        file.push(Separator);
        file.push(Item(MenuAction::Quit));
    }
    let camera = vec![
        Item(MenuAction::Discover),
        Separator,
        Item(MenuAction::Transplant),
        Item(MenuAction::Restore),
        Item(MenuAction::Check),
        Separator,
        Item(MenuAction::VendorOps),
    ];
    let mut out = vec![Menu { title: "File", entries: file }];
    if !macos {
        out.push(Menu { title: "Edit", entries: vec![Item(MenuAction::Preferences)] });
    }
    out.push(Menu { title: "Camera", entries: camera });
    out.push(Menu { title: "Firmware", entries: vec![Item(MenuAction::Firmware)] });
    if !macos {
        out.push(Menu { title: "Help", entries: vec![Item(MenuAction::About)] });
    }
    out
}

/// The native macOS menu bar.
#[cfg(target_os = "macos")]
pub(crate) mod native {
    use std::collections::HashMap;
    use std::sync::mpsc;

    use eframe::egui;
    use muda::accelerator::{Accelerator, Code, Modifiers};
    use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};

    use super::{Entry, MenuAction, MAC_APP_MENU, menus};

    pub(crate) struct NativeMenu {
        _menu: Menu,
        items: Vec<(MenuAction, MenuItem)>,
        clicks: mpsc::Receiver<MenuAction>,
    }

    fn accelerator(action: MenuAction) -> Option<Accelerator> {
        let cmd = |key| Some(Accelerator::new(Modifiers::META, key));
        match action {
            MenuAction::Preferences => cmd(Code::Comma),
            MenuAction::Quit => cmd(Code::KeyQ),
            MenuAction::Discover => cmd(Code::KeyR),
            MenuAction::Export
            | MenuAction::Import
            | MenuAction::Transplant
            | MenuAction::Restore
            | MenuAction::Check
            | MenuAction::VendorOps
            | MenuAction::Firmware
            | MenuAction::About => None,
        }
    }

    impl NativeMenu {
        /// Replace the application's menu bar. Call from the eframe creation closure, once the
        /// window (and so winit's own default menu) exists. On `Err` nothing is installed and the
        /// caller draws the in-window bar instead.
        pub(crate) fn install(ctx: &egui::Context) -> Result<Self, muda::Error> {
            let menu = Menu::new();
            let mut items: Vec<(MenuAction, MenuItem)> = Vec::new();
            let mut item = |action: MenuAction| -> MenuItem {
                let it = MenuItem::new(action.label(true), true, accelerator(action));
                items.push((action, it.clone()));
                it
            };

            let app = Submenu::new("Fleet", true);
            app.append(&item(MenuAction::About))?;
            app.append(&PredefinedMenuItem::separator())?;
            debug_assert!(MAC_APP_MENU.contains(&MenuAction::Preferences) && MAC_APP_MENU.contains(&MenuAction::Quit));
            app.append(&item(MenuAction::Preferences))?;
            app.append(&PredefinedMenuItem::separator())?;
            app.append(&PredefinedMenuItem::services(None))?;
            app.append(&PredefinedMenuItem::separator())?;
            app.append(&PredefinedMenuItem::hide(None))?;
            app.append(&PredefinedMenuItem::hide_others(None))?;
            app.append(&PredefinedMenuItem::show_all(None))?;
            app.append(&PredefinedMenuItem::separator())?;
            app.append(&item(MenuAction::Quit))?;
            menu.append(&app)?;

            for m in menus(true) {
                let sub = Submenu::new(m.title, true);
                for e in m.entries {
                    match e {
                        Entry::Item(a) => sub.append(&item(a))?,
                        Entry::Separator => sub.append(&PredefinedMenuItem::separator())?,
                    }
                }
                menu.append(&sub)?;
            }
            menu.init_for_nsapp();

            let ids: HashMap<MenuId, MenuAction> = items.iter().map(|(a, it)| (it.id().clone(), *a)).collect();
            let (tx, clicks) = mpsc::channel();
            let ctx = ctx.clone();
            MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
                if let Some(action) = ids.get(&ev.id) {
                    // The receiver is gone only while the app is shutting down.
                    let _ = tx.send(*action);
                }
                ctx.request_repaint();
            }));
            Ok(Self { _menu: menu, items, clicks })
        }

        pub(crate) fn set_enabled(&self, enabled: impl Fn(MenuAction) -> bool) {
            for (action, item) in &self.items {
                item.set_enabled(enabled(*action));
            }
        }

        pub(crate) fn clicked(&self) -> Vec<MenuAction> {
            self.clicks.try_iter().collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reachable(macos: bool) -> Vec<MenuAction> {
        let mut v: Vec<MenuAction> = menus(macos)
            .iter()
            .flat_map(|m| m.entries.iter())
            .filter_map(|e| match e {
                Entry::Item(a) => Some(*a),
                Entry::Separator => None,
            })
            .collect();
        if macos {
            v.extend(MAC_APP_MENU);
        }
        v
    }

    /// Adding a variant fails to compile here until it is numbered, and `ALL` must then hold it.
    #[test]
    fn all_lists_every_variant() {
        let n = |a: MenuAction| match a {
            MenuAction::Export => 0,
            MenuAction::Import => 1,
            MenuAction::Quit => 2,
            MenuAction::Preferences => 3,
            MenuAction::Discover => 4,
            MenuAction::Transplant => 5,
            MenuAction::Restore => 6,
            MenuAction::Check => 7,
            MenuAction::VendorOps => 8,
            MenuAction::Firmware => 9,
            MenuAction::About => 10,
        };
        let mut seen: Vec<usize> = MenuAction::ALL.iter().map(|a| n(*a)).collect();
        seen.sort();
        assert_eq!(seen, (0..11).collect::<Vec<_>>());
    }

    #[test]
    fn every_action_is_in_exactly_one_place_on_each_platform() {
        for macos in [false, true] {
            let found = reachable(macos);
            for a in MenuAction::ALL {
                assert_eq!(found.iter().filter(|x| **x == a).count(), 1, "{a:?} (macos={macos})");
            }
            assert_eq!(found.len(), MenuAction::ALL.len(), "macos={macos}");
        }
    }

    #[test]
    fn preferences_is_under_edit_off_macos_and_in_the_application_menu_on_it() {
        let edit = |macos| menus(macos).into_iter().find(|m| m.title == "Edit");
        let e = edit(false).expect("Edit exists off macOS");
        assert!(e.entries.iter().any(|x| matches!(x, Entry::Item(MenuAction::Preferences))));
        assert!(edit(true).is_none());
        assert!(MAC_APP_MENU.contains(&MenuAction::Preferences));
    }

    #[test]
    fn menu_order_is_file_edit_camera_firmware_help() {
        let titles = |macos| menus(macos).iter().map(|m| m.title).collect::<Vec<_>>();
        assert_eq!(titles(false), ["File", "Edit", "Camera", "Firmware", "Help"]);
        assert_eq!(titles(true), ["File", "Camera", "Firmware"]);
    }

    #[test]
    fn the_settings_item_is_called_settings_on_macos_and_preferences_elsewhere() {
        assert_eq!(MenuAction::Preferences.label(true), "Settings…");
        assert_eq!(MenuAction::Preferences.label(false), "Preferences…");
    }

    #[test]
    fn separators_never_lead_trail_or_double_up() {
        for macos in [false, true] {
            for m in menus(macos) {
                assert!(!matches!(m.entries.first(), Some(Entry::Separator)), "{} leads", m.title);
                assert!(!matches!(m.entries.last(), Some(Entry::Separator)), "{} trails", m.title);
                assert!(
                    !m.entries.windows(2).any(|w| matches!(w, [Entry::Separator, Entry::Separator])),
                    "{} doubles",
                    m.title
                );
            }
        }
    }
}
