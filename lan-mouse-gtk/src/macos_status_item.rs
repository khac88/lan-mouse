#![allow(clashing_extern_declarations)]

use std::{
    cell::RefCell,
    ffi::{CStr, CString, c_char, c_double, c_uint, c_void},
    sync::OnceLock,
};

use adw::prelude::*;
use gtk::{gio, glib};
use lan_mouse_ipc::{FrontendRequest, Position};

use crate::{client_object::ClientObject, window::Window};

type Id = *mut c_void;
type Class = *mut c_void;
type Sel = *mut c_void;
type Bool = i8;

struct StatusItem {
    app: glib::WeakRef<adw::Application>,
    window: glib::WeakRef<Window>,
    _hold: gio::ApplicationHoldGuard,
    _delegate: Id,
    _status_item: Id,
    _menu: Id,
}

thread_local! {
    static STATUS_ITEM: RefCell<Option<StatusItem>> = const { RefCell::new(None) };
    static PENDING_NEW_CLIENT_HOSTNAME: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn take_pending_client_hostname() -> Option<String> {
    PENDING_NEW_CLIENT_HOSTNAME.with(|p| p.borrow_mut().take())
}

pub fn setup(app: &adw::Application, window: &Window) {
    log::debug!("macos_status_item::setup entered");
    STATUS_ITEM.with(|item| {
        let already_initialized = item.borrow().is_some();
        if already_initialized {
            let mut cell = item.borrow_mut();
            if let Some(existing) = cell.as_mut() {
                existing.app.set(Some(app));
                existing.window.set(Some(window));
            }
            return;
        }

        unsafe {
            let hold = app.hold();

            let ns_app = msg_send_id(class(c"NSApplication"), sel(c"sharedApplication"));
            assert!(
                !ns_app.is_null(),
                "NSApplication sharedApplication returned null"
            );
            msg_send_bool_usize(ns_app, sel(c"setActivationPolicy:"), 1);

            let delegate = new_delegate();
            let menu = msg_send_id(msg_send_id(class(c"NSMenu"), sel(c"alloc")), sel(c"init"));
            msg_send_void_id(menu, sel(c"setDelegate:"), delegate);
            rebuild_menu(menu, window, delegate);

            let status_bar = msg_send_id(class(c"NSStatusBar"), sel(c"systemStatusBar"));
            assert!(
                !status_bar.is_null(),
                "NSStatusBar systemStatusBar returned null"
            );
            let status_item = msg_send_id_f64(status_bar, sel(c"statusItemWithLength:"), -1.0);
            assert!(!status_item.is_null(), "statusItemWithLength returned null");
            // Retain so the status item survives autorelease pool drain.
            let status_item = msg_send_id(status_item, sel(c"retain"));

            let button = msg_send_id(status_item, sel(c"button"));
            assert!(!button.is_null(), "NSStatusItem.button was null");
            set_button_image(button);
            msg_send_void_id(button, sel(c"setToolTip:"), nsstring(c"Lan Mouse"));
            msg_send_void_id(status_item, sel(c"setMenu:"), menu);

            install_reopen_handler(delegate);

            log::debug!("macos_status_item ready at {status_item:p}");

            item.replace(Some(StatusItem {
                app: app.downgrade(),
                window: window.downgrade(),
                _hold: hold,
                _delegate: delegate,
                _status_item: status_item,
                _menu: menu,
            }));
        }
    });
}

unsafe fn rebuild_menu(menu: Id, window: &Window, delegate: Id) {
    msg_send_void(menu, sel(c"removeAllItems"));
    msg_send_void_bool(menu, sel(c"setAutoenablesItems:"), 0);

    // 1. Accessibility Warning if missing
    if !crate::macos_privacy::accessibility_granted() {
        let warn_item = menu_item(
            c"⚠️ Accessibility Permission Required...",
            c"openPrivacySettings:",
        );
        msg_send_void_id(warn_item, sel(c"setTarget:"), delegate);
        msg_send_void_id(menu, sel(c"addItem:"), warn_item);
        msg_send_void_id(menu, sel(c"addItem:"), separator_item());
    }

    // 2. Header with local hostname & port
    let local_host = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "localhost".to_string());
    let local_port = window.local_port();
    let header_str = CString::new(format!("Lan Mouse ({local_host}:{local_port})")).unwrap();
    let header_item = menu_item(&header_str, c"");
    msg_send_void_bool(header_item, sel(c"setEnabled:"), 0);
    msg_send_void_id(menu, sel(c"addItem:"), header_item);
    msg_send_void_id(menu, sel(c"addItem:"), separator_item());

    // 3. Client Connections
    let clients: Vec<ClientObject> = window
        .client_list_store()
        .map(|store| {
            (0..store.n_items())
                .filter_map(|i| store.item(i).and_downcast::<ClientObject>())
                .collect()
        })
        .unwrap_or_default();

    if clients.is_empty() {
        let no_conn = menu_item(c"No connections configured", c"");
        msg_send_void_bool(no_conn, sel(c"setEnabled:"), 0);
        msg_send_void_id(menu, sel(c"addItem:"), no_conn);
    } else if clients.len() == 1 {
        let client = &clients[0];
        let data = client.get_data();
        let name = data.hostname.clone().unwrap_or_else(|| {
            data.ips
                .first()
                .cloned()
                .unwrap_or_else(|| "Client".to_string())
        });
        let status = if data.active {
            if data.peer_commit.is_some() {
                "Connected"
            } else {
                "Active"
            }
        } else {
            "Inactive"
        };
        let bullet = if data.peer_commit.is_some() {
            "●"
        } else {
            "○"
        };
        let current_pos = data.position.to_lowercase();
        let display_pos = match current_pos.as_str() {
            "top" => "Top",
            "right" => "Right",
            "bottom" => "Bottom",
            "left" => "Left",
            _ => &data.position,
        };
        let title = CString::new(format!("{bullet} {name} ({display_pos} · {status})")).unwrap();
        let client_header = menu_item(&title, c"");
        msg_send_void_bool(client_header, sel(c"setEnabled:"), 0);
        msg_send_void_id(menu, sel(c"addItem:"), client_header);

        let items = build_client_items(client, delegate, false);
        for item in items {
            msg_send_void_id(menu, sel(c"addItem:"), item);
        }
    } else {
        // Multiple clients: each in its own submenu
        for client in &clients {
            let data = client.get_data();
            let name = data.hostname.clone().unwrap_or_else(|| {
                data.ips
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "Client".to_string())
            });
            let status = if data.active {
                if data.peer_commit.is_some() {
                    "Connected"
                } else {
                    "Active"
                }
            } else {
                "Inactive"
            };
            let bullet = if data.peer_commit.is_some() {
                "●"
            } else {
                "○"
            };
            let current_pos = data.position.to_lowercase();
            let display_pos = match current_pos.as_str() {
                "top" => "Top",
                "right" => "Right",
                "bottom" => "Bottom",
                "left" => "Left",
                _ => &data.position,
            };
            let title =
                CString::new(format!("{bullet} {name} ({display_pos} · {status})")).unwrap();
            let sub_parent = menu_item(&title, c"");
            let items = build_client_items(client, delegate, true);
            let sub = create_menu(&items);
            msg_send_void_bool(sub, sel(c"setAutoenablesItems:"), 0);
            msg_send_void_id(sub_parent, sel(c"setSubmenu:"), sub);
            msg_send_void_id(menu, sel(c"addItem:"), sub_parent);
        }
    }

    // 4. Add Connection
    let add_conn_item = menu_item(c"+ Add Connection...", c"addClient:");
    msg_send_void_id(add_conn_item, sel(c"setTarget:"), delegate);
    msg_send_void_id(menu, sel(c"addItem:"), add_conn_item);

    // 5. Authorized Keys
    let auth_keys: Vec<crate::key_object::KeyObject> = window
        .authorized_list_store()
        .map(|store| {
            (0..store.n_items())
                .filter_map(|i| store.item(i).and_downcast::<crate::key_object::KeyObject>())
                .collect()
        })
        .unwrap_or_default();

    if !auth_keys.is_empty() {
        msg_send_void_id(menu, sel(c"addItem:"), separator_item());
        let in_header = menu_item(c"Incoming Connections:", c"");
        msg_send_void_bool(in_header, sel(c"setEnabled:"), 0);
        msg_send_void_id(menu, sel(c"addItem:"), in_header);

        for key in auth_keys {
            let desc = key.get_description();
            let title = CString::new(format!("  {desc} (Authorized)")).unwrap();
            let item = menu_item(&title, c"");
            msg_send_void_bool(item, sel(c"setEnabled:"), 0);
            msg_send_void_id(menu, sel(c"addItem:"), item);
        }
    }

    msg_send_void_id(menu, sel(c"addItem:"), separator_item());

    // 6. Copy Hostname & Fingerprint
    let copy_host_title =
        CString::new(format!("Copy Hostname ({local_host}:{local_port})")).unwrap();
    let copy_host_item = menu_item(&copy_host_title, c"copyHostname:");
    msg_send_void_id(copy_host_item, sel(c"setTarget:"), delegate);
    msg_send_void_id(menu, sel(c"addItem:"), copy_host_item);

    let copy_fp_item = menu_item(c"Copy Certificate Fingerprint", c"copyFingerprint:");
    msg_send_void_id(copy_fp_item, sel(c"setTarget:"), delegate);
    msg_send_void_id(menu, sel(c"addItem:"), copy_fp_item);

    msg_send_void_id(menu, sel(c"addItem:"), separator_item());

    // 7. Quit
    let quit_item = menu_item(c"Quit Lan Mouse", c"quitLanMouse:");
    msg_send_void_id(quit_item, sel(c"setTarget:"), delegate);
    msg_send_void_id(menu, sel(c"addItem:"), quit_item);
}

unsafe fn build_client_items(client: &ClientObject, delegate: Id, is_submenu: bool) -> Vec<Id> {
    let data = client.get_data();
    let handle = data.handle;
    let handle_isize = handle as isize;
    let mut items = Vec::new();

    // 1. Active checkbox
    let active_item = menu_item(c"Active", c"toggleClientActive:");
    msg_send_void_isize(active_item, sel(c"setTag:"), handle_isize);
    msg_send_void_id(active_item, sel(c"setTarget:"), delegate);
    msg_send_void_isize(
        active_item,
        sel(c"setState:"),
        if data.active { 1 } else { 0 },
    );
    if !is_submenu {
        msg_send_void_isize(active_item, sel(c"setIndentationLevel:"), 1);
    }
    items.push(active_item);

    // 2. Position submenu
    let current_pos = data.position.to_lowercase();
    let display_pos = match current_pos.as_str() {
        "top" => "Top",
        "right" => "Right",
        "bottom" => "Bottom",
        "left" => "Left",
        _ => &data.position,
    };
    let pos_title = CString::new(format!("Position: {display_pos}")).unwrap();
    let pos_item = menu_item(&pos_title, c"");
    if !is_submenu {
        msg_send_void_isize(pos_item, sel(c"setIndentationLevel:"), 1);
    }

    let top_item = menu_item(c"Top", c"setPosTop:");
    let right_item = menu_item(c"Right", c"setPosRight:");
    let bottom_item = menu_item(c"Bottom", c"setPosBottom:");
    let left_item = menu_item(c"Left", c"setPosLeft:");

    for (item, pos_name) in [
        (top_item, "top"),
        (right_item, "right"),
        (bottom_item, "bottom"),
        (left_item, "left"),
    ] {
        msg_send_void_isize(item, sel(c"setTag:"), handle_isize);
        msg_send_void_id(item, sel(c"setTarget:"), delegate);
        let checked = if current_pos == pos_name { 1 } else { 0 };
        msg_send_void_isize(item, sel(c"setState:"), checked);
    }

    let pos_menu = create_menu(&[top_item, right_item, bottom_item, left_item]);
    msg_send_void_bool(pos_menu, sel(c"setAutoenablesItems:"), 0);
    msg_send_void_id(pos_item, sel(c"setSubmenu:"), pos_menu);
    items.push(pos_item);

    // 3. Sync Sound checkbox
    let sound_item = menu_item(c"Sync Sound", c"toggleSyncSound:");
    msg_send_void_isize(sound_item, sel(c"setTag:"), handle_isize);
    msg_send_void_id(sound_item, sel(c"setTarget:"), delegate);
    msg_send_void_isize(
        sound_item,
        sel(c"setState:"),
        if data.sync_sound { 1 } else { 0 },
    );
    if !is_submenu {
        msg_send_void_isize(sound_item, sel(c"setIndentationLevel:"), 1);
    }
    items.push(sound_item);

    // 4. Sync Clipboard checkbox
    let clip_item = menu_item(c"Sync Clipboard", c"toggleSyncClipboard:");
    msg_send_void_isize(clip_item, sel(c"setTag:"), handle_isize);
    msg_send_void_id(clip_item, sel(c"setTarget:"), delegate);
    msg_send_void_isize(
        clip_item,
        sel(c"setState:"),
        if data.sync_clipboard { 1 } else { 0 },
    );
    if !is_submenu {
        msg_send_void_isize(clip_item, sel(c"setIndentationLevel:"), 1);
    }
    items.push(clip_item);

    // 5. Delete connection
    let client_name = data.hostname.unwrap_or_else(|| {
        data.ips
            .first()
            .cloned()
            .unwrap_or_else(|| format!("{handle}"))
    });
    let del_title = CString::new(format!("Delete {client_name}...")).unwrap();
    let del_item = menu_item(&del_title, c"deleteClient:");
    msg_send_void_isize(del_item, sel(c"setTag:"), handle_isize);
    msg_send_void_id(del_item, sel(c"setTarget:"), delegate);
    if !is_submenu {
        msg_send_void_isize(del_item, sel(c"setIndentationLevel:"), 1);
    }
    items.push(del_item);

    items
}

// Prefer a pre-rendered template PNG (black silhouette with alpha) so macOS
// auto-tints the glyph to match the menu bar in light and dark modes.
// Falls back to the full-color icns, then to "LM" text.
unsafe fn set_button_image(button: Id) {
    if let Some(image) = load_menubar_template() {
        msg_send_void_bool(image, sel(c"setTemplate:"), 1);
        msg_send_void_id(button, sel(c"setImage:"), image);
        return;
    }
    if let Some(image) = load_app_icon() {
        msg_send_void_id(button, sel(c"setImage:"), image);
        return;
    }
    log::warn!("no menu bar image available; falling back to text title");
    msg_send_void_id(button, sel(c"setTitle:"), nsstring(c"LM"));
}

unsafe fn load_menubar_template() -> Option<Id> {
    load_resource_image(c"menubar-template", c"png", MENUBAR_ICON_SIZE)
}

unsafe fn load_app_icon() -> Option<Id> {
    load_resource_image(c"icon", c"icns", MENUBAR_ICON_SIZE)
}

unsafe fn load_resource_image(name: &CStr, ext: &CStr, size_pt: c_double) -> Option<Id> {
    let bundle = msg_send_id(class(c"NSBundle"), sel(c"mainBundle"));
    if bundle.is_null() {
        return None;
    }
    let path = msg_send_id_id_id(
        bundle,
        sel(c"pathForResource:ofType:"),
        nsstring(name),
        nsstring(ext),
    );
    if path.is_null() {
        return None;
    }
    let image = msg_send_id_id(
        msg_send_id(class(c"NSImage"), sel(c"alloc")),
        sel(c"initWithContentsOfFile:"),
        path,
    );
    if image.is_null() {
        return None;
    }
    // Render at menu bar height; 22pt is the full status bar icon height.
    msg_send_void_size(image, sel(c"setSize:"), size_pt, size_pt);
    Some(image)
}

const MENUBAR_ICON_SIZE: c_double = 22.0;

unsafe fn create_menu(items: &[Id]) -> Id {
    let m = msg_send_id(msg_send_id(class(c"NSMenu"), sel(c"alloc")), sel(c"init"));
    for item in items {
        msg_send_void_id(m, sel(c"addItem:"), *item);
    }
    msg_send_id(m, sel(c"autorelease"))
}

unsafe fn menu_item(title: &CStr, action: &CStr) -> Id {
    let action_sel = if action.to_bytes().is_empty() {
        std::ptr::null_mut()
    } else {
        sel(action)
    };
    let item = msg_send_id_id_sel_id(
        msg_send_id(class(c"NSMenuItem"), sel(c"alloc")),
        sel(c"initWithTitle:action:keyEquivalent:"),
        nsstring(title),
        action_sel,
        nsstring(c""),
    );
    msg_send_id(item, sel(c"autorelease"))
}

unsafe fn separator_item() -> Id {
    msg_send_id(class(c"NSMenuItem"), sel(c"separatorItem"))
}

unsafe fn new_delegate() -> Id {
    let class = delegate_class();
    msg_send_id(msg_send_id(class, sel(c"alloc")), sel(c"init"))
}

fn delegate_class() -> Class {
    static CLASS: OnceLock<usize> = OnceLock::new();

    *CLASS.get_or_init(|| unsafe {
        let superclass = class(c"NSObject");
        let class_name = CString::new("LanMouseStatusItemDelegate").unwrap();
        let class = objc_allocateClassPair(superclass, class_name.as_ptr(), 0);
        assert!(!class.is_null(), "failed to allocate status item delegate");

        class_addMethod(
            class,
            sel(c"quitLanMouse:"),
            quit_lan_mouse as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"handleReopenEvent:withReplyEvent:"),
            handle_reopen_event as *const c_void,
            c"v@:@@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"menuNeedsUpdate:"),
            menu_needs_update as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"toggleClientActive:"),
            toggle_client_active as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"setPosTop:"),
            set_pos_top as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"setPosRight:"),
            set_pos_right as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"setPosBottom:"),
            set_pos_bottom as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"setPosLeft:"),
            set_pos_left as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"toggleSyncSound:"),
            toggle_sync_sound as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"toggleSyncClipboard:"),
            toggle_sync_clipboard as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"deleteClient:"),
            delete_client as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"addClient:"),
            add_client as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"copyHostname:"),
            copy_hostname as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"copyFingerprint:"),
            copy_fingerprint as *const c_void,
            c"v@:@".as_ptr(),
        );
        class_addMethod(
            class,
            sel(c"openPrivacySettings:"),
            open_privacy_settings as *const c_void,
            c"v@:@".as_ptr(),
        );

        objc_registerClassPair(class);
        class as usize
    }) as Class
}

extern "C" fn menu_needs_update(_this: Id, _cmd: Sel, menu: Id) {
    STATUS_ITEM.with(|item| {
        if let Some(window) = item.borrow().as_ref().and_then(|i| i.window.upgrade()) {
            unsafe {
                rebuild_menu(menu, &window, _this);
            }
        }
    });
}

extern "C" fn toggle_client_active(_this: Id, _cmd: Sel, sender: Id) {
    let handle = unsafe { msg_send_isize(sender, sel(c"tag")) } as u64;
    STATUS_ITEM.with(|item| {
        if let Some(window) = item.borrow().as_ref().and_then(|i| i.window.upgrade()) {
            if let Some(client) = window.client_by_handle(handle) {
                let new_val = !client.active();
                client.set_active(new_val);
                window.send_frontend_request(FrontendRequest::Activate(handle, new_val));
            }
        }
    });
}

extern "C" fn set_pos_top(_this: Id, _cmd: Sel, sender: Id) {
    set_client_position(sender, Position::Top);
}

extern "C" fn set_pos_right(_this: Id, _cmd: Sel, sender: Id) {
    set_client_position(sender, Position::Right);
}

extern "C" fn set_pos_bottom(_this: Id, _cmd: Sel, sender: Id) {
    set_client_position(sender, Position::Bottom);
}

extern "C" fn set_pos_left(_this: Id, _cmd: Sel, sender: Id) {
    set_client_position(sender, Position::Left);
}

fn set_client_position(sender: Id, pos: Position) {
    let handle = unsafe { msg_send_isize(sender, sel(c"tag")) } as u64;
    STATUS_ITEM.with(|item| {
        if let Some(window) = item.borrow().as_ref().and_then(|i| i.window.upgrade()) {
            if let Some(client) = window.client_by_handle(handle) {
                client.set_position(pos.to_string());
                window.send_frontend_request(FrontendRequest::UpdatePosition(handle, pos));
            }
        }
    });
}

extern "C" fn toggle_sync_sound(_this: Id, _cmd: Sel, sender: Id) {
    let handle = unsafe { msg_send_isize(sender, sel(c"tag")) } as u64;
    STATUS_ITEM.with(|item| {
        if let Some(window) = item.borrow().as_ref().and_then(|i| i.window.upgrade()) {
            if let Some(client) = window.client_by_handle(handle) {
                let new_val = !client.sync_sound();
                client.set_sync_sound(new_val);
                window.send_frontend_request(FrontendRequest::UpdateSyncSound(handle, new_val));
            }
        }
    });
}

extern "C" fn toggle_sync_clipboard(_this: Id, _cmd: Sel, sender: Id) {
    let handle = unsafe { msg_send_isize(sender, sel(c"tag")) } as u64;
    STATUS_ITEM.with(|item| {
        if let Some(window) = item.borrow().as_ref().and_then(|i| i.window.upgrade()) {
            if let Some(client) = window.client_by_handle(handle) {
                let new_val = !client.sync_clipboard();
                client.set_sync_clipboard(new_val);
                window.send_frontend_request(FrontendRequest::UpdateSyncClipboard(handle, new_val));
            }
        }
    });
}

extern "C" fn delete_client(_this: Id, _cmd: Sel, sender: Id) {
    let handle = unsafe { msg_send_isize(sender, sel(c"tag")) } as u64;
    STATUS_ITEM.with(|item| {
        if let Some(window) = item.borrow().as_ref().and_then(|i| i.window.upgrade()) {
            let client_name = window
                .client_by_handle(handle)
                .and_then(|c| c.hostname())
                .unwrap_or_else(|| format!("Client {handle}"));

            unsafe {
                let alert =
                    msg_send_id(msg_send_id(class(c"NSAlert"), sel(c"alloc")), sel(c"init"));
                let title = CString::new("Delete Connection?").unwrap();
                let msg = CString::new(format!(
                    "Are you sure you want to remove connection '{client_name}'?"
                ))
                .unwrap();
                msg_send_void_id(alert, sel(c"setMessageText:"), nsstring(&title));
                msg_send_void_id(alert, sel(c"setInformativeText:"), nsstring(&msg));
                msg_send_void_id(alert, sel(c"addButtonWithTitle:"), nsstring(c"Delete"));
                msg_send_void_id(alert, sel(c"addButtonWithTitle:"), nsstring(c"Cancel"));

                let ns_app = msg_send_id(class(c"NSApplication"), sel(c"sharedApplication"));
                msg_send_void_bool(ns_app, sel(c"activateIgnoringOtherApps:"), 1);

                let res = msg_send_isize(alert, sel(c"runModal"));
                if res == 1000 {
                    window.send_frontend_request(FrontendRequest::Delete(handle));
                }
            }
        }
    });
}

extern "C" fn add_client(_this: Id, _cmd: Sel, _sender: Id) {
    STATUS_ITEM.with(|item| {
        if let Some(window) = item.borrow().as_ref().and_then(|i| i.window.upgrade()) {
            unsafe {
                let alert =
                    msg_send_id(msg_send_id(class(c"NSAlert"), sel(c"alloc")), sel(c"init"));
                msg_send_void_id(alert, sel(c"setMessageText:"), nsstring(c"Add Connection"));
                msg_send_void_id(
                    alert,
                    sel(c"setInformativeText:"),
                    nsstring(c"Enter client hostname or IP address:"),
                );
                msg_send_void_id(alert, sel(c"addButtonWithTitle:"), nsstring(c"Add"));
                msg_send_void_id(alert, sel(c"addButtonWithTitle:"), nsstring(c"Cancel"));

                let field = msg_send_id(
                    msg_send_id(class(c"NSTextField"), sel(c"alloc")),
                    sel(c"init"),
                );
                msg_send_void_size(field, sel(c"setFrameSize:"), 260.0, 24.0);
                msg_send_void_id(
                    field,
                    sel(c"setPlaceholderString:"),
                    nsstring(c"192.168.1.100"),
                );
                msg_send_void_id(alert, sel(c"setAccessoryView:"), field);

                let alert_win = msg_send_id(alert, sel(c"window"));
                msg_send_void_id(alert_win, sel(c"makeFirstResponder:"), field);

                let ns_app = msg_send_id(class(c"NSApplication"), sel(c"sharedApplication"));
                msg_send_void_bool(ns_app, sel(c"activateIgnoringOtherApps:"), 1);

                let res = msg_send_isize(alert, sel(c"runModal"));
                if res == 1000 {
                    let str_id = msg_send_id(field, sel(c"stringValue"));
                    let utf8 = msg_send_ptr(str_id, sel(c"UTF8String"));
                    if !utf8.is_null() {
                        let text = CStr::from_ptr(utf8).to_string_lossy().trim().to_string();
                        if !text.is_empty() {
                            PENDING_NEW_CLIENT_HOSTNAME.with(|p| p.replace(Some(text)));
                            window.send_frontend_request(FrontendRequest::Create);
                        }
                    }
                }
            }
        }
    });
}

extern "C" fn copy_hostname(_this: Id, _cmd: Sel, _sender: Id) {
    if let Ok(host) = hostname::get() {
        STATUS_ITEM.with(|item| {
            if let Some(window) = item.borrow().as_ref().and_then(|i| i.window.upgrade()) {
                let text = format!("{}:{}", host.to_string_lossy(), window.local_port());
                let display = gtk::gdk::Display::default().unwrap();
                display.clipboard().set_text(&text);
            }
        });
    }
}

extern "C" fn copy_fingerprint(_this: Id, _cmd: Sel, _sender: Id) {
    STATUS_ITEM.with(|item| {
        if let Some(window) = item.borrow().as_ref().and_then(|i| i.window.upgrade()) {
            if let Some(fp) = window.public_key_fingerprint() {
                let display = gtk::gdk::Display::default().unwrap();
                display.clipboard().set_text(&fp);
            }
        }
    });
}

extern "C" fn open_privacy_settings(_this: Id, _cmd: Sel, _sender: Id) {
    crate::macos_privacy::open_accessibility_settings();
}

extern "C" fn handle_reopen_event(_this: Id, _cmd: Sel, _event: Id, _reply: Id) {
    log::debug!("kAEReopenApplication received");
}

// Register the status-item delegate as the handler for the
// kAEReopenApplication Apple Event ('aevt'/'rapp'). NSApplication
// installs a default handler at -finishLaunching that just delegates to
// applicationShouldHandleReopen:hasVisibleWindows: — which is a no-op
// here because GApplication owns NSApp's delegate. Replacing it lets us
// re-present the window when the user double-clicks the .app while
// we're already running.
unsafe fn install_reopen_handler(delegate: Id) {
    const K_CORE_EVENT_CLASS: c_uint = 0x6165_7674; // 'aevt'
    const K_AE_REOPEN_APPLICATION: c_uint = 0x7261_7070; // 'rapp'

    let manager = msg_send_id(
        class(c"NSAppleEventManager"),
        sel(c"sharedAppleEventManager"),
    );
    if manager.is_null() {
        log::warn!("NSAppleEventManager unavailable; re-launch will not re-open window");
        return;
    }
    msg_send_void_id_sel_u32_u32(
        manager,
        sel(c"setEventHandler:andSelector:forEventClass:andEventID:"),
        delegate,
        sel(c"handleReopenEvent:withReplyEvent:"),
        K_CORE_EVENT_CLASS,
        K_AE_REOPEN_APPLICATION,
    );
}

extern "C" fn quit_lan_mouse(_this: Id, _cmd: Sel, _sender: Id) {
    STATUS_ITEM.with(|item| {
        if let Some(app) = item.borrow().as_ref().and_then(|item| item.app.upgrade()) {
            app.quit();
        }
    });
}

unsafe fn class(name: &CStr) -> Class {
    let class = objc_getClass(name.as_ptr());
    assert!(!class.is_null(), "missing Objective-C class {name:?}");
    class
}

unsafe fn sel(name: &CStr) -> Sel {
    sel_registerName(name.as_ptr())
}

unsafe fn nsstring(value: &CStr) -> Id {
    msg_send_id_ptr(
        class(c"NSString"),
        sel(c"stringWithUTF8String:"),
        value.as_ptr(),
    )
}

#[link(name = "objc")]
extern "C" {
    fn objc_allocateClassPair(superclass: Class, name: *const c_char, extra_bytes: usize) -> Class;
    fn objc_getClass(name: *const c_char) -> Class;
    fn objc_registerClassPair(class: Class);
    fn sel_registerName(name: *const c_char) -> Sel;
    fn class_addMethod(class: Class, name: Sel, imp: *const c_void, types: *const c_char) -> Bool;
}

#[link(name = "AppKit", kind = "framework")]
extern "C" {}

#[link(name = "objc")]
extern "C" {
    #[link_name = "objc_msgSend"]
    fn msg_send_id(receiver: Id, selector: Sel) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_f64(receiver: Id, selector: Sel, value: c_double) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_id_sel_id(receiver: Id, selector: Sel, a: Id, b: Sel, c: Id) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_id_id(receiver: Id, selector: Sel, a: Id, b: Id) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_id_id(receiver: Id, selector: Sel, a: Id) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_void(receiver: Id, selector: Sel);
    #[link_name = "objc_msgSend"]
    fn msg_send_void_size(receiver: Id, selector: Sel, width: c_double, height: c_double);
    #[link_name = "objc_msgSend"]
    fn msg_send_id_ptr(receiver: Id, selector: Sel, value: *const c_char) -> Id;
    #[link_name = "objc_msgSend"]
    fn msg_send_ptr(receiver: Id, selector: Sel) -> *const c_char;
    #[link_name = "objc_msgSend"]
    fn msg_send_isize(receiver: Id, selector: Sel) -> isize;
    #[link_name = "objc_msgSend"]
    fn msg_send_void_isize(receiver: Id, selector: Sel, value: isize);
    #[link_name = "objc_msgSend"]
    fn msg_send_void_bool(receiver: Id, selector: Sel, value: Bool);
    #[link_name = "objc_msgSend"]
    fn msg_send_void_id(receiver: Id, selector: Sel, value: Id);
    #[link_name = "objc_msgSend"]
    fn msg_send_bool_usize(receiver: Id, selector: Sel, value: usize) -> Bool;
    #[link_name = "objc_msgSend"]
    fn msg_send_void_id_sel_u32_u32(
        receiver: Id,
        selector: Sel,
        a: Id,
        b: Sel,
        c: c_uint,
        d: c_uint,
    );
}
