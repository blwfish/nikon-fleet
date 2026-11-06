//! The menu bar: File, Edit, Camera, Firmware, Help.
//!
//! One layout, declared once (`in_window_layout`), and one list of what the macOS application menu
//! takes out of it (`MAC_APP_MENU`). `menus(Bar::InWindow)` is the layout as it is;
//! `menus(Bar::Native)` is the same layout minus every action `MAC_APP_MENU` holds (and the menus
//! that leaves empty), so the two bars cannot disagree about where an action lives: an action is in
//! the native bar's menus or in the application menu, never in neither. Both renderers consume
//! these: the native macOS bar (`native`, muda — Apple menu > Settings…) builds the application menu
//! from `MAC_APP_MENU` item by item and the rest from `menus(Bar::Native)`; the in-window egui bar
//! (`FleetApp::ui_menu_bar`) draws `menus(Bar::InWindow)` on every other platform and on macOS when
//! the native bar cannot be installed. `MenuAction::label` says what each item is called. Neither
//! renderer re-encodes the layout.

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

/// One line of the macOS application menu (the one named after the app, "Fleet" here): one of our
/// actions or one of the system's own items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppMenuEntry {
    Action(MenuAction),
    Separator,
    /// The system's Services submenu.
    Services,
    Hide,
    HideOthers,
    ShowAll,
}

/// The macOS application menu, top to bottom. The actions in it are the ones macOS puts there
/// instead of in a menu of ours: `menus(Bar::Native)` leaves them out of the layout and `native`
/// builds exactly this list, so the native bar is this plus `menus(Bar::Native)`.
pub(crate) const MAC_APP_MENU: [AppMenuEntry; 11] = [
    AppMenuEntry::Action(MenuAction::About),
    AppMenuEntry::Separator,
    AppMenuEntry::Action(MenuAction::Preferences),
    AppMenuEntry::Separator,
    AppMenuEntry::Services,
    AppMenuEntry::Separator,
    AppMenuEntry::Hide,
    AppMenuEntry::HideOthers,
    AppMenuEntry::ShowAll,
    AppMenuEntry::Separator,
    AppMenuEntry::Action(MenuAction::Quit),
];

fn in_app_menu(action: MenuAction) -> bool {
    MAC_APP_MENU.contains(&AppMenuEntry::Action(action))
}

/// Which bar the layout is for. Not a platform test: macOS draws `InWindow` when the native bar
/// cannot be installed, and then there is no application menu to hold anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bar {
    /// The egui bar in the window: every action is in one of its menus.
    InWindow,
    /// The macOS menu bar, next to `MAC_APP_MENU`.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Native,
}

/// Every menu and what is in it, as the in-window bar shows it.
fn in_window_layout() -> Vec<Menu> {
    use Entry::{Item, Separator};
    vec![
        Menu { title: "File", entries: vec![Item(MenuAction::Export), Item(MenuAction::Import), Separator, Item(MenuAction::Quit)] },
        Menu { title: "Edit", entries: vec![Item(MenuAction::Preferences)] },
        Menu {
            title: "Camera",
            entries: vec![
                Item(MenuAction::Discover),
                Separator,
                Item(MenuAction::Transplant),
                Item(MenuAction::Restore),
                Item(MenuAction::Check),
                Separator,
                Item(MenuAction::VendorOps),
            ],
        },
        Menu { title: "Firmware", entries: vec![Item(MenuAction::Firmware)] },
        Menu { title: "Help", entries: vec![Item(MenuAction::About)] },
    ]
}

/// The menus, left to right. The native bar has what the in-window one has minus the actions the
/// application menu holds (Settings, About, Quit) and the menus that leaves empty: macOS has no Edit
/// or Help of ours, and its Edit would hold nothing anyway (the text fields are egui's, not the
/// system's, so the system's Copy/Paste items would not reach them).
pub(crate) fn menus(bar: Bar) -> Vec<Menu> {
    let layout = in_window_layout();
    if bar == Bar::InWindow {
        return layout;
    }
    layout
        .into_iter()
        .filter_map(|m| {
            let mut entries: Vec<Entry> = Vec::new();
            for e in m.entries {
                match e {
                    Entry::Item(a) if in_app_menu(a) => {}
                    // a separator stays only between two items
                    Entry::Separator if matches!(entries.last(), None | Some(Entry::Separator)) => {}
                    e => entries.push(e),
                }
            }
            if matches!(entries.last(), Some(Entry::Separator)) {
                entries.pop();
            }
            (!entries.is_empty()).then_some(Menu { title: m.title, entries })
        })
        .collect()
}

/// The native macOS menu bar.
#[cfg(target_os = "macos")]
pub(crate) mod native {
    use std::collections::HashMap;
    use std::sync::mpsc;

    use eframe::egui;
    use muda::accelerator::{Accelerator, Code, Modifiers};
    use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};

    use super::{AppMenuEntry, Bar, Entry, MenuAction, MAC_APP_MENU, menus};

    pub(crate) struct NativeMenu {
        _menu: Menu,
        items: Vec<(MenuAction, MenuItem)>,
        clicks: mpsc::Receiver<MenuAction>,
    }

    pub(super) fn accelerator(action: MenuAction) -> Option<Accelerator> {
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

            // The application menu is `MAC_APP_MENU` line by line: nothing here decides what is in it.
            let app = Submenu::new("Fleet", true);
            for e in MAC_APP_MENU {
                match e {
                    AppMenuEntry::Action(a) => app.append(&item(a))?,
                    AppMenuEntry::Separator => app.append(&PredefinedMenuItem::separator())?,
                    AppMenuEntry::Services => app.append(&PredefinedMenuItem::services(None))?,
                    AppMenuEntry::Hide => app.append(&PredefinedMenuItem::hide(None))?,
                    AppMenuEntry::HideOthers => app.append(&PredefinedMenuItem::hide_others(None))?,
                    AppMenuEntry::ShowAll => app.append(&PredefinedMenuItem::show_all(None))?,
                }
            }
            menu.append(&app)?;

            for m in menus(Bar::Native) {
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

    /// The actions the menus of `bar` hold (without the application menu).
    fn in_menus(bar: Bar) -> Vec<MenuAction> {
        menus(bar)
            .iter()
            .flat_map(|m| m.entries.iter())
            .filter_map(|e| match e {
                Entry::Item(a) => Some(*a),
                Entry::Separator => None,
            })
            .collect()
    }

    /// The actions in `MAC_APP_MENU`.
    fn in_app_menu_list() -> Vec<MenuAction> {
        MAC_APP_MENU
            .iter()
            .filter_map(|e| match e {
                AppMenuEntry::Action(a) => Some(*a),
                _ => None,
            })
            .collect()
    }

    /// Everything the person can reach on `bar`: its menus, plus the application menu on the native one.
    fn reachable(bar: Bar) -> Vec<MenuAction> {
        let mut v = in_menus(bar);
        if bar == Bar::Native {
            v.extend(in_app_menu_list());
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
    fn every_action_is_in_exactly_one_place_on_each_bar() {
        for bar in [Bar::InWindow, Bar::Native] {
            let found = reachable(bar);
            for a in MenuAction::ALL {
                assert_eq!(found.iter().filter(|x| **x == a).count(), 1, "{a:?} ({bar:?})");
            }
            assert_eq!(found.len(), MenuAction::ALL.len(), "{bar:?}");
        }
    }

    /// The fallback on macOS (the native bar could not be installed): there is no application menu
    /// then, so Settings, About and Quit must be in the window's own menus (nothing else reaches them).
    #[test]
    fn the_in_window_bar_needs_no_application_menu() {
        let found = in_menus(Bar::InWindow);
        for a in MenuAction::ALL {
            assert_eq!(found.iter().filter(|x| **x == a).count(), 1, "{a:?} must be in the in-window menus themselves");
        }
        for a in in_app_menu_list() {
            assert!(found.contains(&a), "{a:?} is only in the macOS application menu, which the fallback bar does not have");
        }
    }

    /// The native bar is the in-window layout minus the application menu's actions, derived: not a
    /// second hand-written copy that could drift.
    #[test]
    fn the_native_bar_is_the_in_window_layout_minus_what_the_application_menu_holds() {
        let mut expected = in_menus(Bar::InWindow);
        expected.retain(|a| !in_app_menu_list().contains(a));
        assert_eq!(in_menus(Bar::Native), expected);
        assert!(in_menus(Bar::Native).iter().all(|a| !in_app_menu_list().contains(a)), "no action in both");
    }

    #[test]
    fn the_application_menu_holds_about_settings_and_quit_once_each_with_quit_last() {
        let list = in_app_menu_list();
        assert_eq!(list, [MenuAction::About, MenuAction::Preferences, MenuAction::Quit]);
        assert_eq!(MAC_APP_MENU.last(), Some(&AppMenuEntry::Action(MenuAction::Quit)));
        assert_eq!(MAC_APP_MENU.first(), Some(&AppMenuEntry::Action(MenuAction::About)));
        // the system's own items are there once each
        for sys in [AppMenuEntry::Services, AppMenuEntry::Hide, AppMenuEntry::HideOthers, AppMenuEntry::ShowAll] {
            assert_eq!(MAC_APP_MENU.iter().filter(|e| **e == sys).count(), 1, "{sys:?}");
        }
    }

    #[test]
    fn the_application_menu_has_no_leading_trailing_or_doubled_separator() {
        assert_ne!(MAC_APP_MENU.first(), Some(&AppMenuEntry::Separator));
        assert_ne!(MAC_APP_MENU.last(), Some(&AppMenuEntry::Separator));
        assert!(!MAC_APP_MENU.windows(2).any(|w| w == [AppMenuEntry::Separator, AppMenuEntry::Separator]));
    }

    #[test]
    fn preferences_is_under_edit_in_the_window_and_in_the_application_menu_on_the_native_bar() {
        let edit = |bar| menus(bar).into_iter().find(|m| m.title == "Edit");
        let e = edit(Bar::InWindow).expect("Edit exists in the window");
        assert!(e.entries.iter().any(|x| matches!(x, Entry::Item(MenuAction::Preferences))));
        assert!(edit(Bar::Native).is_none());
        assert!(in_app_menu_list().contains(&MenuAction::Preferences));
    }

    #[test]
    fn menu_order_is_file_edit_camera_firmware_help() {
        let titles = |bar| menus(bar).iter().map(|m| m.title).collect::<Vec<_>>();
        assert_eq!(titles(Bar::InWindow), ["File", "Edit", "Camera", "Firmware", "Help"]);
        assert_eq!(titles(Bar::Native), ["File", "Camera", "Firmware"]);
    }

    #[test]
    fn the_native_file_menu_loses_quit_and_the_separator_before_it() {
        let file = |bar| {
            menus(bar).into_iter().find(|m| m.title == "File").unwrap().entries.iter().map(|e| match e {
                Entry::Item(a) => format!("{a:?}"),
                Entry::Separator => "-".to_string(),
            }).collect::<Vec<_>>()
        };
        assert_eq!(file(Bar::InWindow), ["Export", "Import", "-", "Quit"]);
        assert_eq!(file(Bar::Native), ["Export", "Import"]);
    }

    /// Every item's name, on both spellings of the settings item: a rename is a visible change.
    #[test]
    fn every_label_is_pinned() {
        let want = [
            (MenuAction::Export, "Export All…", "Export All…"),
            (MenuAction::Import, "Import Archive…", "Import Archive…"),
            (MenuAction::Quit, "Quit", "Quit"),
            (MenuAction::Preferences, "Settings…", "Preferences…"),
            (MenuAction::Discover, "Discover Cameras", "Discover Cameras"),
            (MenuAction::Transplant, "Transplant…", "Transplant…"),
            (MenuAction::Restore, "Restore…", "Restore…"),
            (MenuAction::Check, "Check Against Reference…", "Check Against Reference…"),
            (MenuAction::VendorOps, "Vendor Ops…", "Vendor Ops…"),
            (MenuAction::Firmware, "Firmware Archive…", "Firmware Archive…"),
            (MenuAction::About, "About Fleet", "About Fleet"),
        ];
        assert_eq!(want.len(), MenuAction::ALL.len());
        for (a, macos, other) in want {
            assert_eq!((a.label(true), a.label(false)), (macos, other), "{a:?}");
        }
        for a in MenuAction::ALL {
            assert!(want.iter().any(|(w, _, _)| *w == a), "{a:?} has no pinned label");
        }
    }

    /// The accelerators the native bar installs (Cmd+,  Cmd+Q  Cmd+R and nothing else). Plain data,
    /// so it runs without a menu bar; that the keys fire on a real macOS menu is not tested here.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_native_accelerators_are_cmd_comma_q_r_and_nothing_else() {
        use muda::accelerator::{Accelerator, Code, Modifiers};
        let cmd = |c| Some(Accelerator::new(Modifiers::META, c));
        for a in MenuAction::ALL {
            let want = match a {
                MenuAction::Preferences => cmd(Code::Comma),
                MenuAction::Quit => cmd(Code::KeyQ),
                MenuAction::Discover => cmd(Code::KeyR),
                _ => None,
            };
            assert_eq!(native::accelerator(a), want, "{a:?}");
        }
    }

    #[test]
    fn the_settings_item_is_called_settings_on_macos_and_preferences_elsewhere() {
        assert_eq!(MenuAction::Preferences.label(true), "Settings…");
        assert_eq!(MenuAction::Preferences.label(false), "Preferences…");
    }

    #[test]
    fn separators_never_lead_trail_or_double_up() {
        for bar in [Bar::InWindow, Bar::Native] {
            for m in menus(bar) {
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
