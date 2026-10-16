//! The estudio bridge: lets the page that embeds VectorCraft in an iframe open a document in it,
//! drive the app through its control interface (`vectorcraft_ui_egui::control`, the same methods
//! as the desktop app's `--control` port) and hear about changes, all with `postMessage`.
//!
//! Only active when the editor is loaded inside a frame with `?bridge` in its URL; otherwise the
//! web build behaves exactly as upstream's. Messages are accepted only from the page's own origin
//! (the editor is served from the same origin as the embedding app); anything else is ignored.
//!
//! From the embedding page:
//! - `{type: "open", name, bytes}`: opens the document (through the inbox, like a dropped file);
//! - `{type: "control", id, method, params}`: one control request (`engine.execute`,
//!   `document.inspect`, `ui.set`, `ui.render`…); the answer comes back as `reply {id, result}`.
//!   Saving is `engine.execute {command: "document.serialize"}`: the bytes come back in the reply;
//! - `{type: "embed", bare}`: with `bare`, only the canvas shows (no menus, tools, panels, status
//!   bar or document tabs): estudio draws its own around it;
//! - `{type: "font", id, bytes}`: a font file (TTF/OTF) for the text engine, before documents that
//!   use it open (estudio's brand fonts); answered `reply {id, result: {ok, faces}}`.
//!
//! To the embedding page:
//! - `{type: "ready", version}` once the app runs;
//! - `{type: "reply", id, result}`;
//! - `{type: "state", revision, tool, unsaved, documents}` whenever the active document changes
//!   (any edit, selection or view change bumps its revision), the tool changes, or documents open
//!   or close: estudio's panels ask for what they show then;
//! - `{type: "download", name, bytes}` for a file the app would have downloaded (File › Save,
//!   Export): the page decides what to do with it.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use vectorcraft_ui_egui::{ControlRequest, ControlResponse, VectorcraftApp};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::prelude::Closure;

/// Files the app wanted to download while embedded: (name, bytes), posted to the page instead.
pub type Downloads = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

enum Incoming {
    Control(wasm_bindgen::JsValue, String, serde_json::Value),
    Embed(bool),
    Font(wasm_bindgen::JsValue, Vec<u8>),
}

pub struct Bridge {
    origin: String,
    incoming: Arc<Mutex<Vec<Incoming>>>,
    downloads: Downloads,
    control: Sender<ControlRequest>,
    waiting: Vec<(wasm_bindgen::JsValue, Receiver<ControlResponse>)>,
    /// The last state told to the page: (revision, tool, unsaved, documents).
    told: Option<(u64, String, bool, usize)>,
}

/// Whether this page is an embedded editor that should talk to its parent.
pub fn wanted(query: &str) -> bool {
    let embedded = web_sys::window()
        .and_then(|w| w.parent().ok().flatten())
        .is_some_and(|parent| web_sys::window().is_some_and(|me| !js_sys::Object::is(&parent, &me)));
    embedded && query.trim_start_matches('?').split('&').any(|p| p == "bridge")
}

fn get(value: &wasm_bindgen::JsValue, key: &str) -> wasm_bindgen::JsValue {
    js_sys::Reflect::get(value, &key.into()).unwrap_or(wasm_bindgen::JsValue::UNDEFINED)
}

impl Bridge {
    /// Starts listening to the parent. `inbox` is where documents to open go (the app opens them
    /// on its next frame); `control` feeds the app's control channel (`VectorcraftApp::with_control`).
    pub fn start(inbox: Arc<Mutex<Vec<(String, Vec<u8>)>>>, downloads: Downloads, control: Sender<ControlRequest>, ctx: egui::Context) -> Option<Self> {
        let window = web_sys::window()?;
        let origin = window.location().origin().ok()?;
        let incoming: Arc<Mutex<Vec<Incoming>>> = Arc::default();
        let queue = incoming.clone();
        let accepted = origin.clone();
        let listener = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != accepted {
                return;
            }
            let data = event.data();
            match get(&data, "type").as_string().unwrap_or_default().as_str() {
                "open" => {
                    let name = get(&data, "name").as_string().unwrap_or_else(|| "document.vectorcraft".into());
                    let bytes = js_sys::Uint8Array::new(&get(&data, "bytes")).to_vec();
                    inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes));
                }
                "control" => {
                    let params = js_sys::JSON::stringify(&get(&data, "params"))
                        .ok()
                        .and_then(|s| s.as_string())
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or(serde_json::Value::Null);
                    let method = get(&data, "method").as_string().unwrap_or_default();
                    queue.lock().unwrap_or_else(|e| e.into_inner()).push(Incoming::Control(get(&data, "id"), method, params));
                }
                "embed" => {
                    let bare = get(&data, "bare").as_bool().unwrap_or(true);
                    queue.lock().unwrap_or_else(|e| e.into_inner()).push(Incoming::Embed(bare));
                }
                "font" => {
                    let bytes = js_sys::Uint8Array::new(&get(&data, "bytes")).to_vec();
                    queue.lock().unwrap_or_else(|e| e.into_inner()).push(Incoming::Font(get(&data, "id"), bytes));
                }
                _ => return,
            }
            ctx.request_repaint();
        });
        window.add_event_listener_with_callback("message", listener.as_ref().unchecked_ref()).ok()?;
        // The listener lives as long as the page.
        listener.forget();
        let bridge = Self { origin, incoming, downloads, control, waiting: Vec::new(), told: None };
        bridge.post(&[("type", "ready".into()), ("version", env!("CARGO_PKG_VERSION").into())]);
        Some(bridge)
    }

    fn post(&self, fields: &[(&str, wasm_bindgen::JsValue)]) {
        let message = js_sys::Object::new();
        for (key, value) in fields {
            let _ = js_sys::Reflect::set(&message, &(*key).into(), value);
        }
        if let Some(parent) = web_sys::window().and_then(|w| w.parent().ok().flatten()) {
            if let Err(e) = parent.post_message(&message, &self.origin) {
                log::error!("bridge: couldn't post to the parent: {e:?}");
            }
        }
    }

    fn json(value: &serde_json::Value) -> wasm_bindgen::JsValue {
        js_sys::JSON::parse(&value.to_string()).unwrap_or(wasm_bindgen::JsValue::NULL)
    }

    /// Only the canvas (`bare`), or the app's own interface back.
    fn embed(app: &mut VectorcraftApp, bare: bool) {
        app.ui.embedded = bare;
        app.ui.toolbar = !bare;
        app.ui.dock = !bare;
        app.ui.status_bar = !bare;
        app.ui.control_bar = !bare;
        app.ui.task_bar = !bare;
        // Screen mode 2 hides the app bar (the menus) and keeps everything editable.
        app.ui.screen_mode = if bare { 2 } else { 0 };
    }

    /// Once per frame, with the app: run what the parent asked for, and tell it what changed.
    pub fn tick(&mut self, app: &mut VectorcraftApp) {
        let incoming = std::mem::take(&mut *self.incoming.lock().unwrap_or_else(|e| e.into_inner()));
        for message in incoming {
            match message {
                Incoming::Control(id, method, params) => {
                    let (request, reply) = ControlRequest::new(method, params);
                    if self.control.send(request).is_ok() {
                        self.waiting.push((id, reply));
                    }
                }
                Incoming::Embed(bare) => Self::embed(app, bare),
                Incoming::Font(id, bytes) => {
                    let faces = vectorcraft_text::FontDb::global().add_font(bytes);
                    let result = serde_json::json!({"ok": faces > 0, "faces": faces});
                    self.post(&[("type", "reply".into()), ("id", id), ("result", Self::json(&result))]);
                }
            }
        }
        let downloads = std::mem::take(&mut *self.downloads.lock().unwrap_or_else(|e| e.into_inner()));
        for (name, bytes) in downloads {
            let array = js_sys::Uint8Array::from(bytes.as_slice());
            self.post(&[("type", "download".into()), ("name", name.into()), ("bytes", array.into())]);
        }
        let mut still = Vec::new();
        for (id, reply) in std::mem::take(&mut self.waiting) {
            match reply.try_recv() {
                Ok(result) => self.post(&[("type", "reply".into()), ("id", id), ("result", Self::json(&result))]),
                Err(std::sync::mpsc::TryRecvError::Empty) => still.push((id, reply)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.post(&[("type", "reply".into()), ("id", id), ("result", Self::json(&serde_json::json!({"ok": false, "error": "no reply"})))])
                }
            }
        }
        self.waiting = still;
        let documents = app.session.documents();
        let revision = app.session.active().map_or(0, |d| d.revision);
        let unsaved = documents.iter().any(|d| d.is_dirty());
        let now = (revision, app.session.tool_id().to_string(), unsaved, documents.len());
        if self.told.as_ref() != Some(&now) {
            self.post(&[
                ("type", "state".into()),
                ("revision", (now.0 as f64).into()),
                ("tool", now.1.as_str().into()),
                ("unsaved", now.2.into()),
                ("documents", (now.3 as f64).into()),
            ]);
            self.told = Some(now);
        }
    }
}
