//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! GridCraft in an `<iframe>` (such as a Nextcloud app). Messages are plain objects with a `type`:
//!
//! - `gridcraft:ready` (to the parent): `{ version }`, the app is listening; send a file now.
//! - `gridcraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`. It opens like a workbook picked with File › Open or dropped on the window.
//! - `gridcraft:save` (to the parent): `{ name, bytes }` (a transferred `ArrayBuffer`) for every
//!   file GridCraft would otherwise download: Save and Save As (an .xlsx workbook) and Export
//!   as PDF. The parent stores it and reports failures itself.
//! - `gridcraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//! - `gridcraft:failed` (to the parent): `{ error }` when the app couldn't start.
//!
//! xrm extension (the section at the end of this file; protocol 1):
//! - `gridcraft:command` (from the parent): `{ id, command, params }` runs one of GridCraft's own
//!   commands (`file.saveBytes`, `home.bold`, …) or a bridge command (`host.save`, `host.state`);
//!   the answer is `gridcraft:result` `{ id, ok, result }` or `{ id, ok: false, error }`.
//! - `gridcraft:saved` (from the parent): `{ name }`, the file handed over was stored.
//! - `gridcraft:save-failed` (from the parent): `{ name, error }`, it was not: the workbook counts
//!   as unsaved again.
//!
//! Only the parent window on the page's own origin is heard or answered. In host mode the page
//! starts with an empty workbook instead of a sample, and the parent guards leaving the page.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gridcraft_ui_egui::{Inbox, Services, SheetApp};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

/// Saving one of these hands the whole workbook to the parent, which stores it: the workbook
/// counts as saved under that name, as it would after Save on the desktop.
const WORKBOOK_EXTS: &[&str] = &["xlsx"];

/// The longest user name taken from `?author=`.
const MAX_AUTHOR_CHARS: usize = 200;

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    /// Names of the files handed to the parent since the last frame.
    saved: Rc<RefCell<Vec<String>>>,
    /// The unsaved-changes state last reported to the parent.
    reported: Rc<Cell<Option<bool>>>,
    /// xrm: messages from the parent waiting for the frame (commands, save confirmations).
    inbound: Rc<RefCell<Vec<Inbound>>>,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to).
    pub fn new() -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        Some(Self { window, parent, origin, saved: Rc::default(), reported: Rc::default(), inbound: Rc::default() })
    }

    /// Send saved and exported files to the parent instead of downloading them, and hear files
    /// from it.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, ctx: egui::Context) {
        self.listen(inbox, ctx);
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            if let Err(e) = bridge.post_save(name, bytes) {
                log::error!("couldn't hand {name} to the page: {e}");
            }
        }));
    }

    /// `?author=`: the host's name for the user signs new notes and comments.
    pub fn set_author(app: &mut SheetApp, author: &str) {
        let author: String = author.trim().chars().take(MAX_AUTHOR_CHARS).collect();
        if !author.is_empty() {
            app.session.prefs.user_name = author;
        }
    }

    pub fn post_ready(&self) {
        self.send(&[("type", "gridcraft:ready".into()), ("version", env!("CARGO_PKG_VERSION").into()), ("protocol", PROTOCOL.into())]);
    }

    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "gridcraft:failed".into()), ("error", error.into())]);
    }

    /// After each frame: a workbook handed to the parent this frame counts as saved under the
    /// name it was saved as, and the parent hears when unsaved changes appear or go away.
    pub fn sync(&self, app: &mut SheetApp) {
        self.run_inbound(app); // xrm: the parent's commands and save confirmations, on the UI thread
        let saved = std::mem::take(&mut *self.saved.borrow_mut());
        if let Some(name) = saved.iter().rev().find(|name| is_workbook(name))
            && let Some(doc) = app.session.active_mut()
        {
            doc.saved = doc.wb.clone();
            if doc.path.is_none() {
                doc.title = name.clone();
            }
        }
        let dirty = app.session.documents().iter().any(|d| d.is_dirty());
        if self.reported.get() != Some(dirty) && self.send(&[("type", "gridcraft:dirty".into()), ("dirty", dirty.into())]) {
            self.reported.set(Some(dirty));
        }
    }

    fn post_save(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let message = message(&[("type", "gridcraft:save".into()), ("name", name.as_str().into()), ("bytes", buffer.clone().into())])
            .ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
        self.saved.borrow_mut().push(name);
        Ok(())
    }

    fn listen(&self, inbox: Inbox, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let inbound = self.inbound.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            let data = event.data();
            if let Some(message) = Inbound::parse(&data) {
                // xrm: handled by the frame (`sync`), where the app is.
                inbound.borrow_mut().push(message);
                ctx.request_repaint();
                return;
            }
            if string(&data, "type").as_deref() != Some("gridcraft:open") {
                return;
            }
            let name = string(&data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Book.xlsx".to_string());
            let Some(bytes) = bytes(&data) else { return };
            inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
            ctx.request_repaint();
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

fn is_workbook(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| WORKBOOK_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

/// `bytes` as an `ArrayBuffer` or `Uint8Array`.
fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(array.to_vec())
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(&bytes).to_vec())
    } else {
        None
    }
}

// ── xrm host extension ───────────────────────────────────────────────────────────────────────
// Everything below is the xrm fork's addition, kept apart from the upstream bridge above so that
// upstream changes rebase cleanly. The same section, with its app's prefix, lives in
// wordcraft-web and deckcraft-web; the protocol is documented in the xrm repo
// (docs/craft-host-protocol.md).

/// The protocol version announced in `gridcraft:ready`.
const PROTOCOL: u32 = 1;

/// A message from the parent that the frame handles (the app lives on the UI thread).
enum Inbound {
    /// `gridcraft:command` `{ id, command, params }`: run a command, answer with `gridcraft:result`.
    Command { id: JsValue, command: String, params: serde_json::Value },
    /// `gridcraft:saved` `{ name }`: the parent stored the file it was handed.
    Saved { name: String },
    /// `gridcraft:save-failed` `{ name, error }`: the parent couldn't store it.
    SaveFailed { name: String, error: String },
}

impl Inbound {
    fn parse(data: &JsValue) -> Option<Self> {
        match string(data, "type")?.as_str() {
            "gridcraft:command" => {
                let command = string(data, "command")?;
                let id = js_sys::Reflect::get(data, &JsValue::from_str("id")).unwrap_or(JsValue::NULL);
                let params = json_field(data, "params").unwrap_or_else(|| serde_json::json!({}));
                Some(Self::Command { id, command, params })
            }
            "gridcraft:saved" => Some(Self::Saved { name: string(data, "name").unwrap_or_default() }),
            "gridcraft:save-failed" => {
                Some(Self::SaveFailed { name: string(data, "name").unwrap_or_default(), error: string(data, "error").unwrap_or_default() })
            }
            _ => None,
        }
    }
}

impl Bridge {
    /// The parent's messages since the last frame: commands run against the app and are answered;
    /// a failed store makes the workbook unsaved again.
    fn run_inbound(&self, app: &mut SheetApp) {
        let queue = std::mem::take(&mut *self.inbound.borrow_mut());
        for message in queue {
            match message {
                Inbound::Command { id, command, params } => {
                    let result = self.run_command(app, &command, params);
                    self.post_result(&id, result);
                }
                Inbound::Saved { name } => log::debug!("the page stored {name}"),
                Inbound::SaveFailed { name, error } => {
                    log::error!("the page couldn't store {name}: {error}");
                    // The workbook was counted as saved when it was handed over (`sync`); a fresh
                    // copy as the saved state makes it dirty again without touching its content.
                    if let Some(doc) = app.session.active_mut() {
                        doc.saved = Arc::new((*doc.wb).clone());
                    }
                }
            }
        }
    }

    /// `host.*` commands are the bridge's own; anything else is one of GridCraft's commands
    /// (`crates/engine/src/cmd/*`, the UI commands), as the control channel runs them.
    fn run_command(&self, app: &mut SheetApp, command: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        match command {
            "host.save" => self.save_active(app, &params),
            "host.state" => Ok(serde_json::json!({
                "dirty": app.session.documents().iter().any(|d| d.is_dirty()),
                "title": app.session.active().map(|d| d.display_title()),
                "protocol": PROTOCOL,
            })),
            other => app.run(other, params),
        }
    }

    /// Hand the active workbook to the parent as a file (`params.format`: xlsx by default), as
    /// Save would, so the parent can store it on its own schedule (autosave).
    fn save_active(&self, app: &mut SheetApp, params: &serde_json::Value) -> Result<serde_json::Value, String> {
        let format = params.get("format").and_then(serde_json::Value::as_str).unwrap_or("xlsx");
        let r = app.run("file.saveBytes", serde_json::json!({"format": format}))?;
        let bytes = r
            .get("base64")
            .and_then(serde_json::Value::as_str)
            .and_then(gridcraft_engine::io::base64_decode)
            .ok_or_else(|| "file.saveBytes returned no bytes".to_string())?;
        let name = match r.get("name").and_then(serde_json::Value::as_str) {
            Some(n) if !n.trim().is_empty() => n.to_string(),
            _ => {
                let title = app.session.active().map(|d| d.display_title()).unwrap_or_else(|| "Book1".into());
                std::path::Path::new(&title).with_extension(format).to_string_lossy().to_string()
            }
        };
        self.post_save(&name, &bytes)?;
        Ok(serde_json::json!({"saved": name, "bytes": bytes.len()}))
    }

    fn post_result(&self, id: &JsValue, result: Result<serde_json::Value, String>) {
        let fields: Vec<(&str, JsValue)> = match result {
            Ok(value) => vec![("type", "gridcraft:result".into()), ("id", id.clone()), ("ok", true.into()), ("result", to_js(&value))],
            Err(error) => vec![("type", "gridcraft:result".into()), ("id", id.clone()), ("ok", false.into()), ("error", error.as_str().into())],
        };
        self.send(&fields);
    }
}

/// A JSON field of a message as serde JSON (through `JSON.stringify`; absent or null → `None`).
fn json_field(data: &JsValue, key: &str) -> Option<serde_json::Value> {
    let value = js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    let text: String = js_sys::JSON::stringify(&value).ok()?.into();
    serde_json::from_str(&text).ok()
}

/// serde JSON as a JavaScript value (`null` when it can't be converted).
fn to_js(value: &serde_json::Value) -> JsValue {
    js_sys::JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL)
}
