//! App-owned global shortcuts via the standard XDG portal. This mode uses Gio
//! without initializing GTK. Settings inspect legacy bindings read-only; no config writes.
use glib::variant::ToVariant;
use gtk4::prelude::*;
use gtk4::{gio, glib};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub const BUS: &str = "ai.vellum.Shortcuts";
const PATH: &str = "/ai/vellum/Shortcuts";
const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const XML: &str = r#"<node><interface name="ai.vellum.Shortcuts">
<method name="GetStatus"><arg type="s" direction="out"/></method>
<method name="Enable"/><method name="Disable"/><method name="Configure"/>
</interface></node>"#;
pub const SPECS: &[(&str, &str, &str)] = &[
    ("region", "区域截图", "LOGO+Print"),
    ("long", "开始 / 完成长截图", "LOGO+SHIFT+Print"),
    ("pin-last", "钉住剪贴板", "LOGO+CTRL+Print"),
];

#[derive(Debug, Default)]
struct ActivationGate(HashSet<String>);
impl ActivationGate {
    fn press(&mut self, id: &str) -> bool {
        SPECS.iter().any(|spec| spec.0 == id) && self.0.insert(id.to_string())
    }
    fn release(&mut self, id: &str) {
        self.0.remove(id);
    }
}
#[derive(Clone, Copy, Debug)]
enum RequestKind {
    Create,
    Bind,
    List,
}
struct Pending {
    kind: RequestKind,
    started: Instant,
}
struct Service {
    connection: gio::DBusConnection,
    enabled: Cell<bool>,
    registered: Cell<bool>,
    phase: RefCell<String>,
    message: RefCell<String>,
    version: Cell<u32>,
    session: RefCell<Option<String>>,
    pending: RefCell<HashMap<String, Pending>>,
    bindings: RefCell<BTreeMap<String, String>>,
    gate: RefCell<ActivationGate>,
    count: Cell<u64>,
    last_action: RefCell<String>,
    sequence: Cell<u64>,
    test: bool,
}
fn preferences_path() -> PathBuf {
    vellum_core::paths::config_path().with_file_name("shortcuts.json")
}
fn load_enabled() -> bool {
    std::fs::read_to_string(preferences_path())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("enabled").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}
fn store_enabled(enabled: bool) -> std::io::Result<()> {
    let path = preferences_path();
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("missing preference directory"))?;
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    writeln!(temp, "{}", serde_json::json!({"enabled":enabled}))?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}
fn object_path(value: &str) -> glib::Variant {
    glib::variant::ObjectPath::try_from(value)
        .expect("validated D-Bus object path")
        .to_variant()
}
fn dict_options(token: &str) -> glib::Variant {
    let dict = glib::VariantDict::new(None);
    dict.insert("handle_token", token);
    dict.end()
}
fn bound_shortcuts(details: &glib::Variant) -> BTreeMap<String, String> {
    let dict = glib::VariantDict::new(Some(details));
    let Some(shortcuts) = dict.lookup_value("shortcuts", None) else {
        return BTreeMap::new();
    };
    let mut found = BTreeMap::new();
    if shortcuts.type_().as_str() != "a(sa{sv})" {
        return found;
    }
    for tuple in shortcuts.iter() {
        let id = tuple.child_value(0);
        let Some(id) = id.str() else {
            continue;
        };
        if !SPECS.iter().any(|spec| spec.0 == id) {
            continue;
        }
        let properties = tuple.child_value(1);
        let props = glib::VariantDict::new(Some(&properties));
        if let Ok(Some(trigger)) = props.lookup::<String>("trigger_description")
            && !trigger.is_empty()
        {
            found.insert(id.to_string(), trigger);
        }
    }
    found
}

impl Service {
    fn new(connection: gio::DBusConnection) -> Rc<Self> {
        let test = std::env::var("VELLUM_SHORTCUTS_TEST").as_deref() == Ok("1");
        Rc::new(Self {
            connection,
            registered: Cell::new(false),
            enabled: Cell::new(!test && load_enabled()),
            phase: RefCell::new("disabled".into()),
            message: RefCell::new("点击申请系统授权；需要桌面提供可用的快捷键后端".into()),
            version: Cell::new(0),
            session: RefCell::new(None),
            pending: RefCell::new(HashMap::new()),
            bindings: RefCell::new(BTreeMap::new()),
            gate: RefCell::new(ActivationGate::default()),
            count: Cell::new(0),
            last_action: RefCell::new(String::new()),
            sequence: Cell::new(0),
            test,
        })
    }
    fn status(&self) -> serde_json::Value {
        serde_json::json!({"phase":*self.phase.borrow(),"message":*self.message.borrow(),"enabled":self.enabled.get(),
            "portal_version":self.version.get(),"can_configure":self.version.get()>=2,"bindings":*self.bindings.borrow(),
            "activations":self.count.get(),"last_action":*self.last_action.borrow(),"test_mode":self.test})
    }
    fn state(&self, phase: &str, message: &str) {
        *self.phase.borrow_mut() = phase.into();
        *self.message.borrow_mut() = message.into();
    }
    fn persist(&self, enabled: bool) -> Result<(), String> {
        if !self.test {
            store_enabled(enabled).map_err(|e| format!("无法保存快捷键偏好：{e}"))?;
        }
        self.enabled.set(enabled);
        Ok(())
    }
    fn initialize(self: &Rc<Self>) {
        if !self.pending.borrow().is_empty() || self.session.borrow().is_some() {
            return;
        }
        self.state("connecting", "正在连接系统快捷键服务…");
        if !self.registered.get() {
            let app_id = "ai.vellum";
            let registered = self.connection.call_sync(
                Some(PORTAL),
                PORTAL_PATH,
                "org.freedesktop.host.portal.Registry",
                "Register",
                Some(&(app_id, HashMap::<String, glib::Variant>::new()).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                3000,
                None::<&gio::Cancellable>,
            );
            if let Err(error) = registered
                && !error.matches(gio::DBusError::UnknownMethod)
                && !error.matches(gio::DBusError::UnknownInterface)
            {
                self.state("unavailable", &format!("系统无法识别应用身份：{error}"));
                return;
            }
            // Older portals identify native apps from their desktop activation.
            self.registered.set(true);
        }
        let version = self.connection.call_sync(
            Some(PORTAL),
            PORTAL_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(IFACE, "version").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            3000,
            None::<&gio::Cancellable>,
        );
        let version = version
            .ok()
            .and_then(|v| v.child_value(0).as_variant())
            .and_then(|v| v.get::<u32>())
            .unwrap_or(0);
        self.version.set(version);
        if version == 0 {
            self.state(
                "unavailable",
                "当前桌面未提供全局快捷键接口。请检查桌面与 portal 兼容性，更新后重试；截图按钮仍可使用",
            );
            return;
        }
        let token = self.token();
        let dict = glib::VariantDict::new(None);
        dict.insert("handle_token", &token);
        dict.insert("session_handle_token", &token);
        self.request(
            "CreateSession",
            RequestKind::Create,
            &token,
            glib::Variant::tuple_from_iter([dict.end()]),
        );
    }
    fn token(&self) -> String {
        let seq = self.sequence.get() + 1;
        self.sequence.set(seq);
        format!("vellum_{}_{}", std::process::id(), seq)
    }
    fn request(self: &Rc<Self>, method: &str, kind: RequestKind, token: &str, args: glib::Variant) {
        let sender = self
            .connection
            .unique_name()
            .unwrap()
            .trim_start_matches(':')
            .replace('.', "_");
        let expected = format!("{PORTAL_PATH}/request/{sender}/{token}");
        self.pending.borrow_mut().insert(
            expected.clone(),
            Pending {
                kind,
                started: Instant::now(),
            },
        );
        let weak = Rc::downgrade(self);
        self.connection.call(
            Some(PORTAL),
            PORTAL_PATH,
            IFACE,
            method,
            Some(&args),
            None,
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            move |reply| {
                let Some(service) = weak.upgrade() else {
                    return;
                };
                match reply {
                    Ok(reply) => {
                        let actual = reply.child_value(0);
                        if let Some(actual) = actual.str()
                            && actual != expected
                        {
                            let shifted = service.pending.borrow_mut().remove(&expected);
                            if let Some(pending) = shifted {
                                service.pending.borrow_mut().insert(actual.into(), pending);
                            }
                        }
                    }
                    Err(error) => {
                        if service.pending.borrow_mut().remove(&expected).is_some() {
                            eprintln!("[vellum-shortcuts] request failed: {error}");
                            service.state("unavailable", "系统快捷键后端请求失败。请检查桌面与 portal 兼容性，更新后重试；暂用截图按钮或已有桌面绑定");
                            service.close_session();
                        }
                    }
                }
            },
        );
    }
    fn response(self: &Rc<Self>, path: &str, params: &glib::Variant) {
        let pending = self.pending.borrow_mut().remove(path);
        let Some(pending) = pending else {
            return;
        };
        let Some(code) = params.child_value(0).get::<u32>() else {
            return;
        };
        if code != 0 {
            // Some v1 backends cannot list a session before its first bind.
            // Listing is a restore hint, never a prerequisite to authorization.
            if matches!(pending.kind, RequestKind::List) && code != 1 {
                self.bind();
                return;
            }
            self.close_session();
            if code == 1 {
                let _ = self.persist(false);
                self.state("denied", "系统授权已取消；点击启用可再次申请");
            } else {
                self.state(
                    "unavailable",
                    &format!(
                        "桌面后端未能完成{}；接口存在不代表后端可用。请检查桌面与 portal 后端兼容性，更新后重试；暂用截图按钮或已有桌面绑定",
                        match pending.kind {
                            RequestKind::Create => "会话创建",
                            RequestKind::Bind => "全局快捷键绑定",
                            RequestKind::List => "快捷键查询",
                        }
                    ),
                );
            }
            return;
        }
        let details = params.child_value(1);
        match pending.kind {
            RequestKind::Create => {
                let dict = glib::VariantDict::new(Some(&details));
                let Ok(Some(session)) = dict.lookup::<String>("session_handle") else {
                    self.state("error", "系统未返回有效的快捷键会话");
                    return;
                };
                if glib::variant::ObjectPath::try_from(session.as_str()).is_err() {
                    self.state("error", "系统返回了无效会话路径");
                    return;
                }
                *self.session.borrow_mut() = Some(session.clone());
                let token = self.token();
                self.request(
                    "ListShortcuts",
                    RequestKind::List,
                    &token,
                    glib::Variant::tuple_from_iter([object_path(&session), dict_options(&token)]),
                );
            }
            RequestKind::List => self.bind(),
            RequestKind::Bind => self.apply_bindings(&details),
        }
    }
    fn apply_bindings(&self, details: &glib::Variant) {
        let found = bound_shortcuts(details);
        let count = found.len();
        *self.bindings.borrow_mut() = found;
        self.gate.borrow_mut().0.clear();
        if count == SPECS.len() {
            self.state("active", "系统已返回授权组合键；实际触发仍需按键验证");
        } else if count > 0 {
            self.state("partial", "仅部分快捷键获准，请检查系统返回的组合键与冲突");
        } else {
            self.state(
                "unavailable",
                "系统没有返回任何已绑定快捷键；不会显示为已启用",
            );
        }
    }

    fn bind(self: &Rc<Self>) {
        let Some(session) = self.session.borrow().clone() else {
            return;
        };
        self.state("authorizing", "请在系统窗口中授权并确认组合键…");
        let shortcuts: Vec<(String, HashMap<String, glib::Variant>)> = SPECS
            .iter()
            .map(|(id, label, trigger)| {
                let mut options = HashMap::new();
                options.insert("description".into(), label.to_variant());
                options.insert("preferred_trigger".into(), trigger.to_variant());
                (id.to_string(), options)
            })
            .collect();
        let token = self.token();
        self.request(
            "BindShortcuts",
            RequestKind::Bind,
            &token,
            glib::Variant::tuple_from_iter([
                object_path(&session),
                shortcuts.to_variant(),
                "".to_variant(),
                dict_options(&token),
            ]),
        );
    }
    fn enable(self: &Rc<Self>) -> Result<(), String> {
        self.persist(true)?;
        // An empty grant leaves a session but no usable shortcuts. A retry must
        // create a fresh session, not silently do nothing or bind twice on v1.
        if self.pending.borrow().is_empty() && self.bindings.borrow().is_empty() {
            self.close_session();
        }
        if self.session.borrow().is_none() {
            self.initialize();
        }
        Ok(())
    }
    fn close_session(&self) {
        self.gate.borrow_mut().0.clear();
        self.bindings.borrow_mut().clear();
        let session = self.session.borrow_mut().take();
        if let Some(session) = session {
            self.connection.call(
                Some(PORTAL),
                &session,
                "org.freedesktop.portal.Session",
                "Close",
                None,
                None,
                gio::DBusCallFlags::NONE,
                2000,
                None::<&gio::Cancellable>,
                |_| {},
            );
        }
    }
    fn cancel_pending(&self) {
        let pending: Vec<_> = self
            .pending
            .borrow_mut()
            .drain()
            .map(|(path, _)| path)
            .collect();
        for path in pending {
            self.connection.call(
                Some(PORTAL),
                &path,
                "org.freedesktop.portal.Request",
                "Close",
                None,
                None,
                gio::DBusCallFlags::NONE,
                2000,
                None::<&gio::Cancellable>,
                |_| {},
            );
        }
    }

    fn disable(&self) -> Result<(), String> {
        self.persist(false)?;
        self.cancel_pending();
        self.close_session();
        self.state("disabled", "应用授权已停用；已有桌面绑定不受影响");
        Ok(())
    }
    fn configure(self: &Rc<Self>) -> Result<(), String> {
        if self.version.get() < 2 {
            return Err("当前桌面后端仅提供版本1，没有标准的再次配置接口。若桌面提供快捷键设置，请在那里调整；否则等待兼容后端更新".into());
        }
        if !self.pending.borrow().is_empty() || self.bindings.borrow().is_empty() {
            return Err("请先完成系统授权，再修改组合键".into());
        }
        let session = self
            .session
            .borrow()
            .clone()
            .ok_or("请先启用并授权快捷键")?;
        let args = glib::Variant::tuple_from_iter([
            object_path(&session),
            "".to_variant(),
            glib::VariantDict::new(None).end(),
        ]);
        let weak = Rc::downgrade(self);
        self.connection.call(
            Some(PORTAL),
            PORTAL_PATH,
            IFACE,
            "ConfigureShortcuts",
            Some(&args),
            None,
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::Cancellable>,
            move |result| {
                if let Err(error) = result
                    && let Some(service) = weak.upgrade()
                    && service.session.borrow().as_deref() == Some(session.as_str())
                {
                    *service.message.borrow_mut() = format!("无法打开系统快捷键设置：{error}");
                }
            },
        );
        Ok(())
    }
    fn activated(&self, params: &glib::Variant, pressed: bool) {
        let session = params.child_value(0);
        if self.session.borrow().as_deref() != session.str() {
            return;
        }
        let id = params.child_value(1);
        let Some(id) = id.str() else {
            return;
        };
        if !pressed {
            self.gate.borrow_mut().release(id);
            return;
        }
        if !self.bindings.borrow().contains_key(id) || !self.gate.borrow_mut().press(id) {
            return;
        }
        self.count.set(self.count.get() + 1);
        *self.last_action.borrow_mut() = id.into();
        if self.test {
            eprintln!("[vellum-shortcuts-test] activated {id}");
            return;
        }
        let id = id.to_string();
        std::thread::spawn(move || {
            let exe = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.join("vellumctl")))
                .filter(|p| vellum_core::proc::is_executable(p))
                .or_else(|| vellum_core::proc::which("vellumctl"));
            let Some(exe) = exe else {
                vellum_core::io::notify("Vellum 快捷键", "未找到截图入口，请检查安装", "normal");
                return;
            };
            let args = if id == "pin-last" {
                Vec::new()
            } else {
                vellum_core::prefs::load().args()
            };
            match std::process::Command::new(exe).arg(id).args(args).spawn() {
                Ok(child) => vellum_core::proc::reap_in_background(child),
                Err(_) => vellum_core::io::notify("Vellum 快捷键", "截图入口无法启动", "normal"),
            }
        });
    }
}

pub fn run_service() -> anyhow::Result<i32> {
    let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)?;
    let reply = connection.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "RequestName",
        Some(&(BUS, 4u32).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        3000,
        None::<&gio::Cancellable>,
    )?;
    if reply.child_value(0).get::<u32>() != Some(1) {
        return Ok(0);
    }
    let service = Service::new(connection.clone());
    let info = gio::DBusNodeInfo::for_xml(XML)?;
    let interface = info.lookup_interface(BUS).unwrap();
    let weak = Rc::downgrade(&service);
    let _registration = connection
        .register_object(PATH, &interface)
        .method_call(move |_, _, _, _, method, _, invocation| {
            let Some(service) = weak.upgrade() else {
                invocation.return_dbus_error("ai.vellum.Error", "服务已关闭");
                return;
            };
            if method == "GetStatus" {
                invocation.return_value(Some(&(service.status().to_string(),).to_variant()));
                return;
            }
            let result = match method {
                "Enable" => service.enable(),
                "Disable" => service.disable(),
                "Configure" => service.configure(),
                _ => Err("未知快捷键操作".into()),
            };
            match result {
                Ok(()) => invocation.return_value(None),
                Err(error) => invocation.return_dbus_error("ai.vellum.Shortcuts.Error", &error),
            }
        })
        .build()?;
    let weak = Rc::downgrade(&service);
    let _responses = connection.subscribe_to_signal(
        Some(PORTAL),
        Some("org.freedesktop.portal.Request"),
        Some("Response"),
        None,
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            if let Some(s) = weak.upgrade() {
                s.response(signal.object_path, signal.parameters);
            }
        },
    );
    let weak = Rc::downgrade(&service);
    let _activated = connection.subscribe_to_signal(
        Some(PORTAL),
        Some(IFACE),
        None,
        Some(PORTAL_PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            if let Some(s) = weak.upgrade() {
                match signal.signal_name {
                    "Activated" => s.activated(signal.parameters, true),
                    "Deactivated" => s.activated(signal.parameters, false),
                    "ShortcutsChanged"
                        if s.session.borrow().as_deref()
                            == signal.parameters.child_value(0).str() =>
                    {
                        let details = glib::VariantDict::new(None);
                        details.insert_value("shortcuts", &signal.parameters.child_value(1));
                        s.apply_bindings(&details.end());
                    }
                    _ => {}
                }
            }
        },
    );
    let weak = Rc::downgrade(&service);
    let _closed = connection.subscribe_to_signal(
        Some(PORTAL),
        Some("org.freedesktop.portal.Session"),
        Some("Closed"),
        None,
        None,
        gio::DBusSignalFlags::NONE,
        move |signal| {
            if let Some(s) = weak.upgrade()
                && s.session.borrow().as_deref() == Some(signal.object_path)
            {
                // An outstanding Bind response belongs to this dead session.
                // Cancel it before any late success can revive an invalid state.
                s.cancel_pending();
                s.session.borrow_mut().take();
                s.bindings.borrow_mut().clear();
                s.gate.borrow_mut().0.clear();
                s.state("closed", "系统已关闭快捷键会话，请重新启用");
            }
        },
    );
    let weak = Rc::downgrade(&service);
    let _owner = connection.subscribe_to_signal(
        Some("org.freedesktop.DBus"),
        Some("org.freedesktop.DBus"),
        Some("NameOwnerChanged"),
        Some("/org/freedesktop/DBus"),
        Some(PORTAL),
        gio::DBusSignalFlags::NONE,
        move |signal| {
            if let Some(s) = weak.upgrade() {
                let old = signal.parameters.child_value(1);
                if old.str() == Some("")
                    && (!s.pending.borrow().is_empty() || s.session.borrow().is_some())
                {
                    return;
                }
                s.registered.set(false);
                s.version.set(0);
                s.session.borrow_mut().take();
                s.pending.borrow_mut().clear();
                s.bindings.borrow_mut().clear();
                s.gate.borrow_mut().0.clear();
                let new = signal.parameters.child_value(2);
                if new.str().is_some_and(|n| !n.is_empty()) && s.enabled.get() {
                    s.initialize();
                } else if s.enabled.get() {
                    s.state("reconnecting", "系统快捷键服务正在重启，恢复后将自动重连");
                }
            }
        },
    );
    let weak = Rc::downgrade(&service);
    let _timeout = glib::timeout_add_local(Duration::from_secs(2), move || {
        let Some(s) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if s.pending
            .borrow()
            .values()
            .any(|p| p.started.elapsed() > Duration::from_secs(180))
        {
            let _ = s.disable();
            s.state("timeout", "系统授权等待超时，请重新启用");
        }
        glib::ControlFlow::Continue
    });
    if service.enabled.get() {
        service.initialize();
    }
    let loop_ = glib::MainLoop::new(None, false);
    let on_closed = loop_.clone();
    connection.set_exit_on_close(false);
    connection.connect_closed(move |_, _, _| on_closed.quit());
    loop_.run();
    Ok(1)
}

fn has_owner(connection: &gio::DBusConnection) -> bool {
    connection
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameHasOwner",
            Some(&(BUS,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            1000,
            None::<&gio::Cancellable>,
        )
        .ok()
        .and_then(|v| v.child_value(0).get::<bool>())
        .unwrap_or(false)
}
fn ensure_service(connection: &gio::DBusConnection) -> Result<(), String> {
    if has_owner(connection) {
        return Ok(());
    }
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let child = std::process::Command::new(exe)
        .arg("shortcuts-service")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| e.to_string())?;
    vellum_core::proc::reap_in_background(child);
    let end = Instant::now() + Duration::from_secs(3);
    while Instant::now() < end {
        if has_owner(connection) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    Err("快捷键服务未能启动".into())
}
/// Call from a worker or the management CLI, never synchronously on the GTK loop.
pub fn control(method: &str, start: bool) -> Result<serde_json::Value, String> {
    let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
        .map_err(|e| e.to_string())?;
    if start {
        ensure_service(&connection)?;
    }
    if !has_owner(&connection) {
        return Ok(
            serde_json::json!({"phase":"stopped","message":"点击申请系统授权；是否可用取决于桌面快捷键后端","bindings":{},"portal_version":0}),
        );
    }
    let reply = connection
        .call_sync(
            Some(BUS),
            PATH,
            BUS,
            method,
            None,
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            8000,
            None::<&gio::Cancellable>,
        )
        .map_err(|e| e.to_string())?;
    if method == "GetStatus" {
        let body = reply.child_value(0);
        serde_json::from_str(body.str().ok_or("无效状态回复")?).map_err(|e| e.to_string())
    } else {
        Ok(serde_json::json!({"requested":true}))
    }
}

/// Project only known actions and bounded key names from the legacy scanner.
/// Never pass command output, source paths, or arbitrary labels to the UI.
fn legacy_keys(output: &str) -> BTreeMap<String, String> {
    let mut found = BTreeMap::<String, String>::new();
    for line in output.lines() {
        let Some((key, rest)) = line.split_once('→') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty()
            || key.len() > 80
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+_- ".contains(c))
        {
            continue;
        }
        let Some((label, _)) = rest.trim().split_once(" (") else {
            continue;
        };
        let id = match label.trim() {
            "区域截图" => "region",
            "长截图" => "long",
            "钉住剪贴板" => "pin-last",
            _ => continue,
        };
        found
            .entry(id.into())
            .and_modify(|keys| {
                if !keys.split(" / ").any(|existing| existing == key) {
                    keys.push_str(" / ");
                    keys.push_str(key);
                }
            })
            .or_insert_with(|| key.to_string());
    }
    found
}

fn read_legacy() -> Result<BTreeMap<String, String>, &'static str> {
    let executable = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("vellum")))
        .filter(|path| vellum_core::proc::is_executable(path))
        .or_else(|| vellum_core::proc::which("vellum"))
        .ok_or("未找到只读扫描工具")?;
    let output = vellum_core::proc::run(
        &executable,
        &["shortcuts", "legacy-list"],
        Duration::from_secs(5),
    )
    .ok_or("只读扫描未完成")?;
    let found = legacy_keys(&output.combined());
    if !output.success && found.is_empty() {
        return Err("未发现可识别绑定，或扫描不可用");
    }
    Ok(found)
}

/// Settings own the intent; the desktop owns the actual granted combination.
/// No editable text field falsely claims a key is bound before portal approval.
pub fn settings_page(demo: bool) -> gtk4::ScrolledWindow {
    use gtk4::{Align, Box as GtkBox, Label, Orientation, ScrolledWindow};
    let content = GtkBox::new(Orientation::Vertical, 8);
    content.add_css_class("vellum-page-content");
    for set in [
        gtk4::Widget::set_margin_top,
        gtk4::Widget::set_margin_bottom,
        gtk4::Widget::set_margin_start,
        gtk4::Widget::set_margin_end,
    ] {
        set(content.upcast_ref(), 12);
    }
    let card = GtkBox::new(Orientation::Vertical, 8);
    card.add_css_class("vellum-section-card");
    card.set_margin_top(0);
    let inside = GtkBox::new(Orientation::Vertical, 8);
    inside.set_margin_top(12);
    inside.set_margin_bottom(12);
    inside.set_margin_start(12);
    inside.set_margin_end(12);
    let title = Label::new(Some("全局快捷键"));
    title.add_css_class("vellum-section-title");
    title.set_xalign(0.0);
    inside.append(&title);
    let message = Label::new(Some(
        "应用快捷键 · 需系统授权，与下方已有桌面绑定分开管理。",
    ));
    message.set_wrap(true);
    message.set_xalign(0.0);
    message.add_css_class("vellum-row-sub");
    inside.append(&message);
    let mut keys = Vec::new();
    for (id, label, _) in SPECS {
        let row = GtkBox::new(Orientation::Horizontal, 12);
        row.add_css_class("vellum-row");
        let name = Label::new(Some(label));
        name.set_xalign(0.0);
        name.set_hexpand(false);
        name.set_size_request(128, -1);
        row.append(&name);
        let key = Label::new(Some("未授权"));
        key.add_css_class("vellum-shortcut-key");
        key.set_halign(Align::Fill);
        key.set_hexpand(true);
        key.set_wrap(true);
        key.set_xalign(0.5);
        row.append(&key);
        inside.append(&row);
        keys.push((id.to_string(), key));
    }
    let actions = GtkBox::new(Orientation::Horizontal, 8);
    let enable = crate::controls::primary_button("启用快捷键");
    let configure = crate::controls::secondary_button(
        "修改组合键",
        Some("preferences-desktop-keyboard-shortcuts-symbolic"),
    );
    let disable = crate::controls::secondary_button("停用", None);
    configure.set_sensitive(false);
    disable.set_sensitive(false);
    actions.append(&enable);
    actions.append(&configure);
    actions.append(&disable);
    inside.append(&actions);
    let hint = Label::new(Some(
        "后端不可用时：检查桌面与 portal 兼容性，更新后重试；也可用截图按钮。本页不会写入桌面配置。",
    ));
    hint.add_css_class("vellum-caption");
    hint.set_wrap(true);
    hint.set_xalign(0.0);
    inside.append(&hint);
    card.append(&inside);
    content.append(&card);
    let legacy = GtkBox::new(Orientation::Vertical, 4);
    legacy.add_css_class("vellum-section-card");
    let heading = Label::new(Some("已有桌面绑定 · 只读"));
    heading.add_css_class("vellum-section-title");
    heading.set_xalign(0.0);
    legacy.append(&heading);
    let source = Label::new(Some(
        "来源：桌面配置扫描（可能含未加载文件）；不代表已验证触发，不属于本次应用授权。",
    ));
    source.set_wrap(true);
    source.set_xalign(0.0);
    source.add_css_class("vellum-caption");
    legacy.append(&source);
    let mut legacy_rows = Vec::new();
    for (id, label, _) in SPECS {
        let row = Label::new(Some(&format!("{label}：正在只读扫描…")));
        row.set_xalign(0.0);
        row.set_wrap(true);
        row.add_css_class("vellum-row-sub");
        legacy.append(&row);
        legacy_rows.push((*id, *label, row));
    }
    content.append(&legacy);
    if demo {
        source.set_text("演示模式 · 不扫描桌面配置，不验证触发");
        for (_, label, row) in &legacy_rows {
            row.set_text(&format!("{label}：未扫描"));
        }
    } else {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read_legacy());
        });
        glib::timeout_add_local(Duration::from_millis(80), move || match rx.try_recv() {
            Ok(result) => {
                for (id, label, row) in &legacy_rows {
                    let key = result.as_ref().ok().and_then(|keys| keys.get(*id));
                    row.set_text(&format!(
                        "{label}：{}",
                        key.map(|k| format!("{k} · 检测到，未验证"))
                            .unwrap_or_else(|| "未检测到 / 未验证".into())
                    ));
                }
                if let Err(reason) = result {
                    source.set_text(reason);
                }
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(_) => {
                source.set_text("只读扫描没有返回结果");
                glib::ControlFlow::Break
            }
        });
    }
    let pending = Rc::new(Cell::new(false));
    let ui_generation = Rc::new(Cell::new(0u64));
    for (button, method) in [
        (&enable, "Enable"),
        (&configure, "Configure"),
        (&disable, "Disable"),
    ] {
        let pending = pending.clone();
        let message = message.clone();
        let ui_generation = ui_generation.clone();
        button.connect_clicked(move |_| {
            if demo {
                message.set_text("演示模式 · 不申请系统权限，不修改快捷键");
                return;
            }
            if pending.replace(true) {
                return;
            }
            ui_generation.set(ui_generation.get() + 1);
            message.set_text("正在请求系统快捷键服务…");
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let result =
                    control(method, method == "Enable").and_then(|_| control("GetStatus", false));
                let _ = tx.send(result);
            });
            let pending = pending.clone();
            let weak = message.downgrade();
            glib::timeout_add_local(Duration::from_millis(60), move || {
                let Some(message) = weak.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                match rx.try_recv() {
                    Ok(result) => {
                        pending.set(false);
                        match result {
                            Ok(status) => {
                                message.set_text(status["message"].as_str().unwrap_or("请求已提交"))
                            }
                            Err(error) => message.set_text(&error),
                        }
                        glib::ControlFlow::Break
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Err(_) => {
                        pending.set(false);
                        message.set_text("快捷键服务没有响应");
                        glib::ControlFlow::Break
                    }
                }
            });
        });
    }
    let last_phase = Rc::new(std::cell::RefCell::new(String::new()));
    let weak = content.downgrade();
    let reading = Rc::new(Cell::new(false));
    glib::timeout_add_local(Duration::from_millis(1200), move || {
        let Some(content) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if demo || !content.is_mapped() || pending.get() || reading.replace(true) {
            return glib::ControlFlow::Continue;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(control("GetStatus", false));
        });
        let reading = reading.clone();
        let generation = ui_generation.get();
        let ui_generation = ui_generation.clone();
        let weak = message.downgrade();
        let keys = keys.clone();
        let configure = configure.clone();
        let enable = enable.clone();
        let disable = disable.clone();
        let last_phase = last_phase.clone();
        glib::timeout_add_local(Duration::from_millis(60), move || {
            let Some(message) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            match rx.try_recv() {
                Ok(result) => {
                    reading.set(false);
                    if generation != ui_generation.get() {
                        return glib::ControlFlow::Break;
                    }
                    match result {
                        Ok(status) => {
                            let phase = status["phase"].as_str().unwrap_or("");
                            // Refresh changed backend messages even in the same phase;
                            // unchanged polling must preserve local request errors.
                            let signature = format!("{phase}:{}", status["message"]);
                            if last_phase.borrow().as_str() != signature {
                                last_phase.replace(signature);
                                message.set_text(
                                    status["message"].as_str().unwrap_or("快捷键服务状态未知"),
                                );
                            }
                            let active = phase == "active" || phase == "partial";
                            enable.set_sensitive(
                                !active && phase != "authorizing" && phase != "connecting",
                            );
                            disable.set_sensitive(status["enabled"].as_bool() == Some(true));
                            configure.set_sensitive(
                                active && status["can_configure"].as_bool() == Some(true),
                            );
                            configure.set_tooltip_text(Some(
                                if status["portal_version"].as_u64() == Some(1) {
                                    "当前桌面仅支持版本1：再次配置需由系统设置提供"
                                } else {
                                    "打开系统的快捷键配置窗口"
                                },
                            ));
                            for (id, key) in &keys {
                                key.set_text(status["bindings"][id].as_str().unwrap_or("未绑定"));
                            }
                        }
                        Err(error) => message.set_text(&format!("无法读取快捷键状态：{error}")),
                    }
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(_) => {
                    reading.set(false);
                    glib::ControlFlow::Break
                }
            }
        });
        glib::ControlFlow::Continue
    });
    ScrolledWindow::builder()
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn held_keys_fire_once_until_release() {
        let mut gate = ActivationGate::default();
        assert!(gate.press("long"));
        assert!(!gate.press("long"));
        gate.release("long");
        assert!(gate.press("long"));
        assert!(!gate.press("shell-command"));
    }
    #[test]
    fn legacy_projection_discards_private_sources_and_unknown_actions() {
        let found = legacy_keys(
            "Mod+Print → 区域截图 (/private/secret:2)\nMod+Shift+Print → 长截图 (/private/wrapper:3)\nMod+Ctrl+Print → 钉住剪贴板 (/private/token:4)\nsecret;command → 区域截图 (/private:5)\nF1 → secret-action (/private:6)",
        );
        assert_eq!(found.len(), 3);
        assert_eq!(found["region"], "Mod+Print");
        assert_eq!(found["long"], "Mod+Shift+Print");
        assert_eq!(found["pin-last"], "Mod+Ctrl+Print");
        let rendered = format!("{found:?}");
        assert!(!rendered.contains("private"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn preferred_triggers_use_the_xdg_syntax() {
        for (_, _, trigger) in SPECS {
            assert!(trigger.starts_with("LOGO+"));
            assert!(!trigger.contains("Mod+"));
        }
    }
    #[test]
    fn actual_bindings_never_assume_the_preferred_trigger() {
        let details = glib::VariantDict::new(None);
        let mut properties = HashMap::new();
        properties.insert(
            "trigger_description".to_string(),
            "Ctrl+Alt+F10".to_variant(),
        );
        details.insert_value("shortcuts", &vec![("region", properties)].to_variant());
        let found = bound_shortcuts(&details.end());
        assert_eq!(
            found.get("region").map(String::as_str),
            Some("Ctrl+Alt+F10")
        );
        assert!(!found.contains_key("long"));
    }
}
