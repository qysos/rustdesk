use std::{
    collections::HashMap,
    iter::FromIterator,
    sync::{Arc, Mutex},
};

use sciter::Value;

use hbb_common::{
    allow_err,
    config::{LocalConfig, PeerConfig},
    log,
};

#[cfg(not(any(feature = "flutter", feature = "cli")))]
use crate::ui_session_interface::Session;
use crate::{common::get_app_name, ipc, ui_interface::*};

mod cm;
#[cfg(feature = "inline")]
pub mod inline;
pub mod remote;

#[allow(dead_code)]
type Status = (i32, bool, i64, String);

lazy_static::lazy_static! {
    // stupid workaround for https://sciter.com/forums/topic/crash-on-latest-tis-mac-sdk-sometimes/
    static ref STUPID_VALUES: Mutex<Vec<Arc<Vec<Value>>>> = Default::default();
}

#[cfg(not(any(feature = "flutter", feature = "cli")))]
lazy_static::lazy_static! {
    pub static ref CUR_SESSION: Arc<Mutex<Option<Session<remote::SciterHandler>>>> = Default::default();
}

struct UIHostHandler;

pub fn start(args: &mut [String]) {
    #[cfg(target_os = "macos")]
    crate::platform::delegate::show_dock();
    #[cfg(all(target_os = "linux", feature = "inline"))]
    {
        let app_dir = std::env::var("APPDIR").unwrap_or("".to_string());
        let mut so_path = "/usr/share/rustdesk/libsciter-gtk.so".to_owned();
        for (prefix, dir) in [
            ("", "/usr"),
            ("", "/app"),
            (&app_dir, "/usr"),
            (&app_dir, "/app"),
        ]
        .iter()
        {
            let path = format!("{prefix}{dir}/share/rustdesk/libsciter-gtk.so");
            if std::path::Path::new(&path).exists() {
                so_path = path;
                break;
            }
        }
        sciter::set_library(&so_path).ok();
    }
    #[cfg(windows)]
    // Check if there is a sciter.dll nearby.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let sciter_dll_path = parent.join("sciter.dll");
            if sciter_dll_path.exists() {
                // Try to set the sciter dll.
                let p = sciter_dll_path.to_string_lossy().to_string();
                log::debug!("Found dll:{}, \n {:?}", p, sciter::set_library(&p));
            }
        }
    }
    // https://github.com/c-smile/sciter-sdk/blob/master/include/sciter-x-types.h
    // https://github.com/rustdesk/rustdesk/issues/132#issuecomment-886069737
    #[cfg(windows)]
    allow_err!(sciter::set_options(sciter::RuntimeOptions::GfxLayer(
        sciter::GFX_LAYER::WARP
    )));
    use sciter::SCRIPT_RUNTIME_FEATURES::*;
    allow_err!(sciter::set_options(sciter::RuntimeOptions::ScriptFeatures(
        ALLOW_FILE_IO as u8 | ALLOW_SOCKET_IO as u8 | ALLOW_EVAL as u8 | ALLOW_SYSINFO as u8
    )));
    let mut frame = sciter::WindowBuilder::main_window().create();
    #[cfg(windows)]
    allow_err!(sciter::set_options(sciter::RuntimeOptions::UxTheming(true)));
    frame.set_title(&crate::get_app_name());
    #[cfg(target_os = "macos")]
    crate::platform::delegate::make_menubar(frame.get_host(), args.is_empty());
    #[cfg(windows)]
    crate::platform::try_set_window_foreground(frame.get_hwnd() as _);
    let page;
    if args.len() > 1 && args[0] == "--play" {
        args[0] = "--connect".to_owned();
        let path: std::path::PathBuf = (&args[1]).into();
        let id = path
            .file_stem()
            .map(|p| p.to_str().unwrap_or(""))
            .unwrap_or("")
            .to_owned();
        args[1] = id;
    }
    if args.is_empty() {
        std::thread::spawn(move || check_zombie());
        crate::common::check_software_update();
        frame.event_handler(UI {});
        frame.sciter_handler(UIHostHandler {});
        page = "index.html";
        // Start pulse audio local server.
        #[cfg(target_os = "linux")]
        std::thread::spawn(crate::ipc::start_pa);
    } else if args[0] == "--install" {
        frame.event_handler(UI {});
        frame.sciter_handler(UIHostHandler {});
        page = "install.html";
    } else if args[0] == "--cm" {
        frame.register_behavior("connection-manager", move || {
            Box::new(cm::SciterConnectionManager::new())
        });
        page = "cm.html";
        *cm::HIDE_CM.lock().unwrap() = crate::ipc::get_config("hide_cm")
            .ok()
            .flatten()
            .unwrap_or_default()
            == "true";
    } else if (args[0] == "--connect"
        || args[0] == "--file-transfer"
        || args[0] == "--port-forward"
        || args[0] == "--rdp")
        && args.len() > 1
    {
        #[cfg(windows)]
        {
            let hw = frame.get_host().get_hwnd();
            crate::platform::windows::enable_lowlevel_keyboard(hw as _);
        }
        let mut iter = args.iter();
        let Some(cmd) = iter.next() else {
            log::error!("Failed to get cmd arg");
            return;
        };
        let cmd = cmd.to_owned();
        let Some(id) = iter.next() else {
            log::error!("Failed to get id arg");
            return;
        };
        let id = id.to_owned();
        let pass = iter.next().unwrap_or(&"".to_owned()).clone();
        let args: Vec<String> = iter.map(|x| x.clone()).collect();
        frame.set_title(&id);
        frame.register_behavior("native-remote", move || {
            let handler =
                remote::SciterSession::new(cmd.clone(), id.clone(), pass.clone(), args.clone());
            #[cfg(not(any(feature = "flutter", feature = "cli")))]
            {
                *CUR_SESSION.lock().unwrap() = Some(handler.inner());
            }
            Box::new(handler)
        });
        page = "remote.html";
    } else {
        log::error!("Wrong command: {:?}", args);
        return;
    }
    #[cfg(feature = "inline")]
    {
        let html = if page == "index.html" {
            inline::get_index()
        } else if page == "cm.html" {
            inline::get_cm()
        } else if page == "install.html" {
            inline::get_install()
        } else {
            inline::get_remote()
        };
        frame.load_html(html.as_bytes(), Some(page));
    }
    #[cfg(not(feature = "inline"))]
    frame.load_file(&format!(
        "file://{}/src/ui/{}",
        std::env::current_dir()
            .map(|c| c.display().to_string())
            .unwrap_or("".to_owned()),
        page
    ));
    let hide_cm = *cm::HIDE_CM.lock().unwrap();
    if !args.is_empty() && args[0] == "--cm" && hide_cm {
        // run_app calls expand(show) + run_loop, we use collapse(hide) + run_loop instead to create a hidden window
        frame.collapse(true);
        frame.run_loop();
        return;
    }
    frame.run_app();
}

struct UI {}

impl UI {
    fn recent_sessions_updated(&self) -> bool {
        recent_sessions_updated()
    }

    fn get_id(&self) -> String {
        ipc::get_id()
    }

    fn temporary_password(&mut self) -> String {
        temporary_password()
    }

    fn update_temporary_password(&self) {
        update_temporary_password()
    }

    fn set_permanent_password(&self, password: String) {
        let _ = set_permanent_password_with_result(password);
    }

    fn is_local_permanent_password_set(&self) -> bool {
        is_local_permanent_password_set()
    }

    fn is_permanent_password_set(&self) -> bool {
        is_permanent_password_set()
    }

    fn get_remote_id(&mut self) -> String {
        LocalConfig::get_remote_id()
    }

    fn set_remote_id(&mut self, id: String) {
        LocalConfig::set_remote_id(&id);
    }

    fn goto_install(&mut self) {
        goto_install();
    }

    fn install_me(&mut self, _options: String, _path: String) {
        install_me(_options, _path, false, false);
    }

    fn update_me(&self, _path: String) {
        update_me(_path);
    }

    fn run_without_install(&self) {
        run_without_install();
    }

    fn show_run_without_install(&self) -> bool {
        show_run_without_install()
    }

    fn get_license(&self) -> String {
        get_license()
    }

    fn get_option(&self, key: String) -> String {
        get_option(key)
    }

    fn get_local_option(&self, key: String) -> String {
        get_local_option(key)
    }

    fn set_local_option(&self, key: String, value: String) {
        set_local_option(key, value);
    }

    fn peer_has_password(&self, id: String) -> bool {
        peer_has_password(id)
    }

    fn forget_password(&self, id: String) {
        forget_password(id)
    }

    fn get_peer_option(&self, id: String, name: String) -> String {
        get_peer_option(id, name)
    }

    fn set_peer_option(&self, id: String, name: String, value: String) {
        set_peer_option(id, name, value)
    }

    fn using_public_server(&self) -> bool {
        crate::using_public_server()
    }

    fn is_incoming_only(&self) -> bool {
        hbb_common::config::is_incoming_only()
    }

    pub fn is_outgoing_only(&self) -> bool {
        hbb_common::config::is_outgoing_only()
    }

    pub fn is_custom_client(&self) -> bool {
        crate::common::is_custom_client()
    }

    pub fn is_disable_settings(&self) -> bool {
        hbb_common::config::is_disable_settings()
    }

    pub fn is_disable_account(&self) -> bool {
        hbb_common::config::is_disable_account()
    }

    pub fn is_disable_installation(&self) -> bool {
        hbb_common::config::is_disable_installation()
    }

    pub fn is_disable_ab(&self) -> bool {
        hbb_common::config::is_disable_ab()
    }

    fn get_options(&self) -> Value {
        let hashmap: HashMap<String, String> =
            serde_json::from_str(&get_options()).unwrap_or_default();
        let mut m = Value::map();
        for (k, v) in hashmap {
            m.set_item(k, v);
        }
        m
    }

    fn test_if_valid_server(&self, host: String, test_with_proxy: bool) -> String {
        test_if_valid_server(host, test_with_proxy)
    }

    fn get_sound_inputs(&self) -> Value {
        Value::from_iter(get_sound_inputs())
    }

    fn set_options(&self, v: Value) {
        let mut m = HashMap::new();
        for (k, v) in v.items() {
            if let Some(k) = k.as_string() {
                if let Some(v) = v.as_string() {
                    if !v.is_empty() {
                        m.insert(k, v);
                    }
                }
            }
        }
        set_options(m);
    }

    fn set_option(&self, key: String, value: String) {
        set_option(key, value);
    }

    fn install_path(&mut self) -> String {
        install_path()
    }

    fn install_options(&self) -> String {
        install_options()
    }

    fn get_socks(&self) -> Value {
        Value::from_iter(get_socks())
    }

    fn set_socks(&self, proxy: String, username: String, password: String) {
        set_socks(proxy, username, password)
    }

    fn is_installed(&self) -> bool {
        is_installed()
    }

    fn is_root(&self) -> bool {
        is_root()
    }

    fn is_release(&self) -> bool {
        #[cfg(not(debug_assertions))]
        return true;
        #[cfg(debug_assertions)]
        return false;
    }

    fn is_share_rdp(&self) -> bool {
        is_share_rdp()
    }

    fn set_share_rdp(&self, _enable: bool) {
        set_share_rdp(_enable);
    }

    fn is_installed_lower_version(&self) -> bool {
        is_installed_lower_version()
    }

    fn closing(&mut self, x: i32, y: i32, w: i32, h: i32) {
        crate::server::input_service::fix_key_down_timeout_at_exit();
        LocalConfig::set_size(x, y, w, h);
    }

    fn get_size(&mut self) -> Value {
        let s = LocalConfig::get_size();
        let mut v = Vec::new();
        v.push(s.0);
        v.push(s.1);
        v.push(s.2);
        v.push(s.3);
        Value::from_iter(v)
    }

    fn get_mouse_time(&self) -> f64 {
        get_mouse_time()
    }

    fn check_mouse_time(&self) {
        check_mouse_time()
    }

    fn get_connect_status(&mut self) -> Value {
        let mut v = Value::array(0);
        let x = get_connect_status();
        v.push(x.status_num);
        v.push(x.key_confirmed);
        v.push(x.id);
        v
    }

    #[inline]
    fn get_peer_value(id: String, p: PeerConfig) -> Value {
        let values = vec![
            id,
            p.info.username.clone(),
            p.info.hostname.clone(),
            p.info.platform.clone(),
            p.options.get("alias").unwrap_or(&"".to_owned()).to_owned(),
        ];
        Value::from_iter(values)
    }

    fn get_peer(&self, id: String) -> Value {
        let c = get_peer(id.clone());
        Self::get_peer_value(id, c)
    }

    fn get_fav(&self) -> Value {
        Value::from_iter(get_fav())
    }

    fn store_fav(&self, fav: Value) {
        let mut tmp = vec![];
        fav.values().for_each(|v| {
            if let Some(v) = v.as_string() {
                if !v.is_empty() {
                    tmp.push(v);
                }
            }
        });
        store_fav(tmp);
    }

    fn get_recent_sessions(&mut self) -> Value {
        // to-do: limit number of recent sessions, and remove old peer file
        let peers: Vec<Value> = PeerConfig::peers(None)
            .drain(..)
            .map(|p| Self::get_peer_value(p.0, p.2))
            .collect();
        Value::from_iter(peers)
    }

    fn get_icon(&mut self) -> String {
        get_icon()
    }

    fn remove_peer(&mut self, id: String) {
        PeerConfig::remove(&id);
    }

    fn remove_discovered(&mut self, id: String) {
        remove_discovered(id);
    }

    fn send_wol(&mut self, id: String) {
        crate::lan::send_wol(id)
    }

    fn new_remote(&mut self, id: String, remote_type: String, force_relay: bool) {
        new_remote(id, remote_type, force_relay)
    }

    fn is_process_trusted(&mut self, _prompt: bool) -> bool {
        is_process_trusted(_prompt)
    }

    fn is_can_screen_recording(&mut self, _prompt: bool) -> bool {
        is_can_screen_recording(_prompt)
    }

    fn is_installed_daemon(&mut self, _prompt: bool) -> bool {
        is_installed_daemon(_prompt)
    }

    fn get_error(&mut self) -> String {
        get_error()
    }

    fn is_login_wayland(&mut self) -> bool {
        is_login_wayland()
    }

    fn current_is_wayland(&mut self) -> bool {
        current_is_wayland()
    }

    fn get_software_update_url(&self) -> String {
        crate::SOFTWARE_UPDATE_URL.lock().unwrap().clone()
    }

    fn get_new_version(&self) -> String {
        get_new_version()
    }

    fn get_version(&self) -> String {
        get_version()
    }

    fn get_fingerprint(&self) -> String {
        get_fingerprint()
    }

    fn get_app_name(&self) -> String {
        get_app_name()
    }

    fn get_software_ext(&self) -> String {
        #[cfg(windows)]
        let p = "exe";
        #[cfg(target_os = "macos")]
        let p = "dmg";
        #[cfg(target_os = "linux")]
        let p = "deb";
        p.to_owned()
    }

    fn get_software_store_path(&self) -> String {
        let mut p = std::env::temp_dir();
        let name = crate::SOFTWARE_UPDATE_URL
            .lock()
            .unwrap()
            .split("/")
            .last()
            .map(|x| x.to_owned())
            .unwrap_or(crate::get_app_name());
        p.push(name);
        format!("{}.{}", p.to_string_lossy(), self.get_software_ext())
    }

    fn create_shortcut(&self, _id: String) {
        #[cfg(windows)]
        create_shortcut(_id)
    }

    fn discover(&self) {
        std::thread::spawn(move || {
            allow_err!(crate::lan::discover());
        });
    }

    fn get_lan_peers(&self) -> String {
        // let peers = get_lan_peers()
        //     .into_iter()
        //     .map(|mut peer| {
        //         (
        //             peer.remove("id").unwrap_or_default(),
        //             peer.remove("username").unwrap_or_default(),
        //             peer.remove("hostname").unwrap_or_default(),
        //             peer.remove("platform").unwrap_or_default(),
        //         )
        //     })
        //     .collect::<Vec<(String, String, String, String)>>();
        serde_json::to_string(&get_lan_peers()).unwrap_or_default()
    }

    fn get_uuid(&self) -> String {
        get_uuid()
    }

    fn open_url(&self, url: String) {
        #[cfg(windows)]
        let p = "explorer";
        #[cfg(target_os = "macos")]
        let p = "open";
        #[cfg(target_os = "linux")]
        let p = if std::path::Path::new("/usr/bin/firefox").exists() {
            "firefox"
        } else {
            "xdg-open"
        };
        allow_err!(std::process::Command::new(p).arg(url).spawn());
    }

    fn change_id(&self, id: String) {
        reset_async_job_status();
        let old_id = self.get_id();
        change_id_shared(id, old_id);
    }

    fn http_request(&self, url: String, method: String, body: Option<String>, header: String) {
        http_request(url, method, body, header)
    }

    fn post_request(&self, url: String, body: String, header: String) {
        post_request(url, body, header)
    }

    fn is_ok_change_id(&self) -> bool {
        hbb_common::machine_uid::get().is_ok()
    }

    fn get_async_job_status(&self) -> String {
        get_async_job_status()
    }

    fn get_http_status(&self, url: String) -> Option<String> {
        get_async_http_status(url)
    }

    fn t(&self, name: String) -> String {
        crate::client::translate(name)
    }

    fn is_xfce(&self) -> bool {
        crate::platform::is_xfce()
    }

    fn get_api_server(&self) -> String {
        get_api_server()
    }

    fn has_hwcodec(&self) -> bool {
        has_hwcodec()
    }

    fn has_vram(&self) -> bool {
        has_vram()
    }

    fn get_langs(&self) -> String {
        get_langs()
    }

    fn video_save_directory(&self, root: bool) -> String {
        video_save_directory(root)
    }

    fn handle_relay_id(&self, id: String) -> String {
        handle_relay_id(&id).to_owned()
    }

    fn get_login_device_info(&self) -> String {
        get_login_device_info_json()
    }

    fn support_remove_wallpaper(&self) -> bool {
        support_remove_wallpaper()
    }

    fn has_valid_2fa(&self) -> bool {
        has_valid_2fa()
    }

    fn generate2fa(&self) -> String {
        generate2fa()
    }

    pub fn verify2fa(&self, code: String) -> bool {
        verify2fa(code)
    }

    fn verify_login(&self, raw: String, id: String) -> bool {
        crate::verify_login(&raw, &id)
    }

    fn generate_2fa_img_src(&self, data: String) -> String {
        let v = qrcode_generator::to_png_to_vec(data, qrcode_generator::QrCodeEcc::Low, 128)
            .unwrap_or_default();
        let s = hbb_common::sodiumoxide::base64::encode(
            v,
            hbb_common::sodiumoxide::base64::Variant::Original,
        );
        format!("data:image/png;base64,{s}")
    }

    pub fn check_hwcodec(&self) {
        check_hwcodec()
    }

    fn is_option_fixed(&self, key: String) -> bool {
        crate::ui_interface::is_option_fixed(&key)
    }

    fn get_builtin_option(&self, key: String) -> String {
        crate::ui_interface::get_builtin_option(&key)
    }

    fn is_remote_modify_enabled_by_control_permissions(&self) -> String {
        match crate::ui_interface::is_remote_modify_enabled_by_control_permissions() {
            Some(true) => "true",
            Some(false) => "false",
            None => "",
        }
        .to_string()
    }
}

impl sciter::EventHandler for UI {
    sciter::dispatch_script_call! {
        fn t(String);
        fn get_api_server();
        fn is_xfce();
        fn using_public_server();
        fn is_custom_client();
        fn is_outgoing_only();
        fn is_incoming_only();
        fn is_disable_settings();
        fn is_disable_account();
        fn is_disable_installation();
        fn is_disable_ab();
        fn get_id();
        fn temporary_password();
        fn update_temporary_password();
        fn set_permanent_password(String);
        fn is_local_permanent_password_set();
        fn is_permanent_password_set();
        fn get_remote_id();
        fn set_remote_id(String);
        fn closing(i32, i32, i32, i32);
        fn get_size();
        fn new_remote(String, String, bool);
        fn send_wol(String);
        fn remove_peer(String);
        fn remove_discovered(String);
        fn get_connect_status();
        fn get_mouse_time();
        fn check_mouse_time();
        fn get_recent_sessions();
        fn get_peer(String);
        fn get_fav();
        fn store_fav(Value);
        fn recent_sessions_updated();
        fn get_icon();
        fn install_me(String, String);
        fn is_installed();
        fn is_root();
        fn is_release();
        fn set_socks(String, String, String);
        fn get_socks();
        fn is_share_rdp();
        fn set_share_rdp(bool);
        fn is_installed_lower_version();
        fn install_path();
        fn install_options();
        fn goto_install();
        fn is_process_trusted(bool);
        fn is_can_screen_recording(bool);
        fn is_installed_daemon(bool);
        fn get_error();
        fn is_login_wayland();
        fn current_is_wayland();
        fn get_options();
        fn get_option(String);
        fn get_local_option(String);
        fn set_local_option(String, String);
        fn get_peer_option(String, String);
        fn peer_has_password(String);
        fn forget_password(String);
        fn set_peer_option(String, String, String);
        fn get_license();
        fn test_if_valid_server(String, bool);
        fn get_sound_inputs();
        fn set_options(Value);
        fn set_option(String, String);
        fn get_software_update_url();
        fn get_new_version();
        fn get_version();
        fn get_fingerprint();
        fn update_me(String);
        fn show_run_without_install();
        fn run_without_install();
        fn get_app_name();
        fn get_software_store_path();
        fn get_software_ext();
        fn open_url(String);
        fn change_id(String);
        fn get_async_job_status();
        fn post_request(String, String, String);
        fn is_ok_change_id();
        fn create_shortcut(String);
        fn discover();
        fn get_lan_peers();
        fn get_uuid();
        fn has_hwcodec();
        fn has_vram();
        fn get_langs();
        fn video_save_directory(bool);
        fn handle_relay_id(String);
        fn get_login_device_info();
        fn support_remove_wallpaper();
        fn has_valid_2fa();
        fn generate2fa();
        fn generate_2fa_img_src(String);
        fn verify2fa(String);
        fn check_hwcodec();
        fn verify_login(String, String);
        fn is_option_fixed(String);
        fn get_builtin_option(String);
        fn is_remote_modify_enabled_by_control_permissions();
    }
}

impl sciter::host::HostHandler for UIHostHandler {
    fn on_graphics_critical_failure(&mut self) {
        log::error!("Critical rendering error: e.g. DirectX gfx driver error. Most probably bad gfx drivers.");
    }
}

#[cfg(not(target_os = "linux"))]
fn get_sound_inputs() -> Vec<String> {
    let mut out = Vec::new();
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    if let Ok(devices) = host.devices() {
        for device in devices {
            if device.default_input_config().is_err() {
                continue;
            }
            if let Ok(name) = device.name() {
                out.push(name);
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn get_sound_inputs() -> Vec<String> {
    crate::platform::linux::get_pa_sources()
        .drain(..)
        .map(|x| x.1)
        .collect()
}

// sacrifice some memory
pub fn value_crash_workaround(values: &[Value]) -> Arc<Vec<Value>> {
    let persist = Arc::new(values.to_vec());
    STUPID_VALUES.lock().unwrap().push(persist.clone());
    persist
}

pub fn get_icon() -> String {
    // 128x128
    #[cfg(target_os = "macos")]
    // 128x128 on 160x160 canvas, then shrink to 128, mac looks better with padding
    {
        //"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAIAAAACACAYAAADDPmHLAAABhGlDQ1BJQ0MgcHJvZmlsZQAAeJx9kT1Iw0AYht+mSkUqHewg4pChOlkQFXHUVihChVArtOpgcukfNGlIUlwcBdeCgz+LVQcXZ10dXAVB8AfE1cVJ0UVK/C4ptIjxjuMe3vvel7vvAKFZZZrVMwFoum1mUgkxl18VQ68QEEKYZkRmljEvSWn4jq97BPh+F+dZ/nV/jgG1YDEgIBLPMcO0iTeIZzZtg/M+cZSVZZX4nHjcpAsSP3Jd8fiNc8llgWdGzWwmSRwlFktdrHQxK5sa8TRxTNV0yhdyHquctzhr1Tpr35O/MFzQV5a5TmsEKSxiCRJEKKijgipsxGnXSbGQofOEj3/Y9UvkUshVASPHAmrQILt+8D/43VurODXpJYUTQO+L43yMAqFdoNVwnO9jx2mdAMFn4Erv+GtNYPaT9EZHix0BkW3g4rqjKXvA5Q4w9GTIpuxKQVpCsQi8n9E35YHBW6B/zetb+xynD0CWepW+AQ4OgbESZa/7vLuvu2//1rT79wPpl3Jwc6WkiQAAE5pJREFUeAHtXQt0VNW5/s5kkskkEyCEZwgQSIAEg6CgYBGKiFolwQDRlWW5BatiqiIWiYV6l4uq10fN9fq4rahYwAILXNAlGlAUgV5oSXiqDRggQIBAgJAEwmQeycycu//JDAwQyJzHPpPTmW+tk8yc2fucs//v23v/+3mMiCCsYQz1A0QQWkQEEOaICCDMERFAmCMigDBHRABhjogAwhwRAYQ5IgIIc0QEEOaICCDMobkAhg8f3m/cuHHjR40adXtGRkZmampqX4vFksR+MrPDoPXzhAgedtitVmttVVXVibKysn0lJSU7tm3btrm0tPSIlg+iiQDS0tK6FBQUzMjPz/+PlJSUIeyUoMV92zFI6PFM+PEsE/Rhx+i8vLyZ7JzIBFG2cuXKZQsXLlx8+PDhGt4PwlUAjPjuRUVFL2ZnZz9uNBrNPO/1bwKBMsjcuXPfZMeCzz///BP2/1UmhDO8bshFACaTybBgwYJZ7OFfZsR34HGPMIA5Nzf3GZZ5fsUy0UvMnu87nU6P2jdRXQCDBg3quXr16hVZWVnj1L52OIIy0Lx5895hQshl1cQjBw4cqFb1+mpe7L777hvOyP+C1W3Jal43AoAy1C4GJoJJGzZs2K3WdVUTwNSpU8cw56U4UuTzA2Ws4uLiTcyZzl6zZs1WNa6pigAo50fI1wZkY7I1qxLGq1ESKBaAr87/IkK+diBbk81HMCj1CRQJgLx9cvj0Uue7RRFnmSNd3+xBg0tEk0f0no82CLAYBSRGG9A9xuD93t5BNifbMw3craR1oEgA1NRrj96+yIiuaHRje10z9l5oRlmDCxU2N6ocLriIcy+/Yst/P9dCy3eBHT1MBgyIN2KwxYhhCdEY1SkGWZZoRAntSxhke+Jg/vz578q9hmwBUCcPtfPlxlcbF1mu/vpME76sdmLj2SZUOzw+glty+RVke78LpJTLv4nePyQLb9xqZxP+r9556ffEaAHjk2IxsUssctjRJSZKq6TdEMTBokWLVsrtLJItAOrhC3W972EEfnu6GUsqHVh7ygG7vyD05WYvm95sLbbyGdcVQWtx65tFrDljZ4cNRgNwLxPDjJ7xyO1qDmmVQRwQF5MnT35WVnw5kahvn7p35cRVA42sHF98xIF3Dtpw2OoJKMbRJpFKROAP72K+w/pzDqyvdaAnqy5+08uCp1Ms6BwdmlKBuGCcvMxKgXNS48oSQEFBwa9D0bfvcIv480EH3txvY86ceLl4J0giUrkI/OGrmf/10pEG/PH4RTzb24LCPh3QyajtoCZxwTh5tLCw8C3JceXcMD8//5dy4skFOXWrjzfhhT02VDLn7nJdroRI9URAP1lZqfRaZQM+PGXFK/064slkCwwaOo2Mk2maCGDkyJH9fEO6muCY1Y0nSxqx4VSzj3hpxGgpAgpf2+TBUwfr8c8LTnyamcSCaCMC4oS4KS0tPSolnmQB0GQOaDCeT2ZdesiJ2TttaGgOLOohixgtRUA/LmPO4rQe8bivs2Y1pUDcMAF8IiWSZAGMGDHidqlxpKKREV7wTxuWHbncDFOLGC1F8E2dQ0sBEDe3sX98BZCRkTFYahwpOMa8+ge/teKHOneLYTkQo5UIojSe+CSHG8kCSE1N7SM1TrDYe86FBzY04rTdoxKpwYQHt3tNTIpVxzBBguZXSo0jWQC+CZyqY9tpFyZ+3eir79XM2W2F53Mv6hf4eaK2ApDDjZxmoOqV2ncnXZjEyLe5fIblSEzr4dW91xOM/PcGdVLTRMFCMjdyBKBqL0fJGRce/IrIB+c6vq3w6tzriV7xWJjZSdM+gABI5iakC0MqLniQs97OvP6AkzoWwRO9GfmDQ0a+LIRMAA1NInLW2XDO7qvz/d263q/6E8HMPnH4QGfkE0IiAOrafXSjA+V1/iFbXGt4HYlgJsv5H9zUUXfkE0IigA/KmvG3w662SVOJVBqkG5FkxPDORmR2jELfeAO6mgyIMwreYDa36O3CPW7z4IDVhT3nm7Gjvtl7vq17eXN+lj7JJ2gugEPnPSjc2hR8zpUpAjNL2eQ+MXiorwkTekTDEi2NICcjf2ttE9accuKzk3bUNQVUVb57FaTG409DOsgin0rB4loHNtU7QI+W08WMMZ20bTYSNBUAJXrmRids5PRdIhCqiqCbWcCcwWY8MdCEzib5DRZTlIAJ3Uze4+0hCVhVZcefjtrwk9WN9PgoPJcWh+m9zbIGe5weEY+U1eJvNXZfmkS8deIi5vROwH+nJ8p+ZjnQVAB//cmFLVVu3zeJdXgbv8cywl64ORaFWbGSc3tbMLNrz+gb5z2UgsjP+6EWxefs1/g/bzMRjOloQm5X5fcJFpoJwNosYv62Zh+ZkOfIXef3O7pHYcnYeAzs2D7m6V0PNKFlKiOfZhNdLy3PV5zH/UlmmDSaZqaZAN7b04xT1gD2VRLB80Ni8fptse1+KjeRP+X7WnxF5PvRSlqP2F1YeNKK2aw60AKaCIDa/EU7XQG5X7kIWKmMD8fG4rFBJi2SoAhE/uQ9tfj6nBPBjHC+cawBM5PjWdXDf2qZJgL46AcX6gOEr1QERP6K8WY8nBajxeMrgp3I312HDV7yEVRaTzs9WFzdiKdS+JcC3AXgZk7P+7tdrRbfckXw0Vj9kP/grjp8S+RLrPreOWFFQS/+8wq5C2DdEQ+ONwScUCiCwmEm/Dqj/ZNPxf6kHXXY6M/5EtN6yObCxjqnd/0BT3AXwJJ/tZb75YlgdM8ovDay/df5hJcPWrGxpkmR4JewakDXAjjvELGuwnOd3CzNMGbWtl9ytxnGdu7tE6jD66NKW/BO7XVEsLbGDqvbAwtHZ5CrAIj8JteNivTgDTP/1hikd9THLnK0LLHWGZgOyBIBTZD5mjUb87rz6xjiLAB3EPV624bpGS/g+Vvaf73vB/UcDk4wYv9Fl7TmbSt2+lKvAvAu3DzqS4lCETx/azTiVO7e5Y1Z/ePwm+/J+5XYx3FV+G+ZAKhK4bXAhJsAys+JONeIAA8YkCOCeJbxH78pmtdjcsO03rF4oewiLvo3JJApAlp7WGF3YUAcHxtwE0DJSX/ul9LMu9YwU9ON6GjSV+4nWIwGTEmOxdLjdskdXVeH336+SX8C2Hval1jJbf0rDfPwgPY9wHMjTOlpwtJjdskdXVeH39vQjF9x2oSHmwD2nQ1MKGSJIJZxP76PfgUwvlsMjLSfgBhsutGqncqsLm7PyE0Ah2p92V92r5+A23sYYDbqr/j3g6qBYR2N2FVPBMoXwaFGnQmAdtCovggo7f8f3l0f7f4b4ZZO0S0CUDD4VWV3e3c447FJFRcBnG2kQaCAEzJFkJmkfwEMshhl+kKXw9McqpomD3qY1K8OuQigjqa6icravxS+bwf9Fv9+9DYbrkqrPBHUNetIAFanKClx1zNGV7P+BZAU4yvFFIqgpT9BfXARQJN/3qdCEXBq+moKasm0XgVIE4F/V1O1wakVIAQk2vddhgj0n/8pmcINmsPBi4AP/ZwE4N1EU4WlXLZm6B5Wf1ewwmVoMXoaC0jwD9wpFEHLwlF9o8bpCaI53LadLJz6Q7gIIJG2KVDY9KHPJy7oXwCVVneQgr+xnWgncx7gIoBuFoAm7ngUiqC8Vv8C2H/B5xErEAFR3z1GRwKgaVsprA1//Lz0zp/A8Lur9S+AnbW+XkAFS9OTYw3cpsJxGwtI7wwmAGnt/qsNU3pSZE1K5gBF6bM9cKLRjcMXL21hLlsE6fH8Jm5xu3JWdwGbDouSO38Cw1ubgH+cEHFXqj4FsO6kkrWQlz/flKBDAQzrGZg4+SJYU+5mAtDnmMCqSqfCllDLZxpR5AVuV77Dv52kxM6fq8Ov3OdB0QQRsTobFj7U4Mbfz/iGcRWK4I7O/CbEchPAoK4CulsEnLFK6/y52jC1jSJWMRFMH6qviSHv/uSASNW/AEUtoSSTgMwEfmnnJgBKz4R0YPleKWr3nbwq/J936UsAVY0efHLQtx5Q4VrIu7uauK4P5LouICdTwPI9Pi9IgQjKzuqrOfife+xweDe+hCL/h37K7sl3KRxXAdw/CKzuRosxFIigfyf91P9bqpvxaUVTyxeF/g91/mX35LsghqsAOsQKmDQY+OxHMegirzXDzB6pj1bA+SYRj261+ZKkvOp7oEcMEjn1APrBfXXwjBFMAD9ApgcMFNwWhcduaf8CoJVQM/5uQ2XDVZtfKhDB9FT+28ZxF8C9AwX07wwcqZPuAT/Fcv7/TjRwWxalJn5X6sDayubW0yJDBL3MBuQk818PyV0AtLJ59p3sWCvN+Xmakf++Tsh/ebcDRT86L59QQQSzBmizFF6TPYIeGwm8+h1QYw1OBLPuEPCuDsinYr9wuwNv/+jbCKItkoMUQcdoAU+ma7NrqCYCiI8R8LtxIuYWo816b/ZoA/7HS74WTyYf9U4R07+z48tjzdKqtiB2RZ+TYUYnzs6fH5rtE/jUaOD9bcCx87iuCJ4bLeBtHZC/8YQLj2224ziHfQ97xBrw2wzt3jSmmQBoi5e3ckQ8/ClaNcScMQKKFJBPxTGNHiaw0oaXgI4xD//3251YcShgqZeMzp0bieDVYXFI0HAvBE33Cs67WcC88SLe3OyzjUhkiXjxbgEv3yuPOIdLxB+2uPHhHo93L8L+icAztxswY2gUEmPVMeT+Wg/e+b4JS8td3vkJavTwtSaC0V2j8GiatptgaSoAssHrEwXk3yLim4Mtaf9FhoCsHvKIsjWLmLTCje+O+iZdsMscqWelyQY3XtzsRs5AA6YMMmBCfwOSJCwyIZ4qznuw/qgbqw66sP20+9L1LxMMVUVA6wc+/pm27xsmhOSFEUOTBXYouwaRn7PcjU1HxFY9cHuTiM/2efDZfo/358FdgVuY0AYlGZCSICApDt53ChAfVubH1dhFbxG/v1bEzjMenGz1tfS+LxzeVPL6rXHel1lojZC+NEoubPS+oeUeH/lo09D0d99ZdtQQqZdLi0se+TWfA26mRvHe1oBPSgyezQzN/oe6E4CX/GU+8pV64FeE55Oz2wqf3sGAT8fGheyVM7oSgJf8v3p8cw3BgRhtRZBoMuCLeyze/6GCbgTQyMiftJRyPjgTo40IzKy6//yeeGR2Cu1EFzkCoEpUU8kS+TlLRGw+EnBSxyKgae6rJ8RhbE/V85+n7SBXQs4T0PYP8TLiyQJtN5O7lJFfgVa9fb2JgFoeq++NwwN9uKx9t0uNIFkAVqu11mKxaCaAFXuAjQfBzQPXUgSJMQLW3h+HMcl8al7iRmocyU9SWVl5PCsrq0/bIdXBxkPg5oEHF16dew3oyBy+iWZkJPKr8xk3x6TGkSyA8vLy/UwAd0qNJxdGv7ehYxHk9DNi6T1m5u0LqtmlNRA3UuNIFsCuXbt25OXlzZQaTy5yBgOLd4ADqVLDS49rZtX86z+LwbNDozWZ21BSUrJDahzJAtiyZcsmtCSRf4oYcrMETB8hYuku6EoEdyYb8PGEWFbka9ZgErdt27ZJaiTJAigtLT1aVVX1r5SUlJulxpUDsvHifAETBoqYtw44STuwt2MR9Igz4LU7ozF9sFHT3j3ihHFTKTWeLHd05cqVy+bOnftHOXHlgOw4bbiAKUNEvLcNeGsLUGdrXyLoZALmjDDit7dGwxKjHfF+ECdy4skSwMKFCxc/99xzfzAajdpNXWGIi6H5BMDTo0V8XAK89w8Bx+pDK4LeCQJm3WrEzKGh29be5XLZiBM5cWUJ4PDhw+eKi4sX5ebmzpITXykSmKHn/ByYPUbEV+UCFjP/YF25CKfCFUjBho8xinggzYAZQ4yYmMZv945gwbj4hDiRE1d2jwSrAv4rOzt7OisFOsi9hlJEMcNns1YCHQ0OZohyYP1PIr6pEFDTqK4I6IXe4/sJyEmPwgPpBtVmGykFy/0NxIXc+LIFwBR3pqio6KV58+a9I/caaoKWoT0yDOwQvNyV14goOQ58Xy16F5dW1ArMgRTh9rdfrrchE/vXqwNtcWPATd0E7ySSkb0EZHYRQjZkeyMQB8SF3PiK+iQXLFjwPisFcrOyssYpuY7aIJ4yGXmZ3bzfLp2ncYWzVnjnDl50tmxpS3MSaREmVSu0vV23eIS8SA8WZWVlW4gDJddQJACn0+nJy8t7ZBeDxWLh9FIT9UDEJrPcnXxFpaUPsq+G1Wo9RbYnDpRcR/GoxIEDB6rZg+QwR2RzKP2BcALV+8zmk8j2Sq+lyrDUhg0b9uTn52eztmhxRAR8QeSTrZnNd6txPdXGJdesWbOV+QN3rV69+ks9VAd6hK/Yn6QW+QRVB6apJBjBwESwnDmGd6l57XAHOXxU56tR7AdC9ZkJ9IBMAxOYd/oMa5++EqkSlIGKfGrqkbev1OFrDVymptCDzp8//71FixateuONN36fm5v7OBMCvzcg/xuCEW+n3lbq5FHSzm8LXGcF04M/9NBDs9PS0l4pKCiYwZyXab5RRH22vfhDrKqqKqOBHerbZ/ar4X1DTaaFUz91YWFhER3Dhw9PHTdu3PhRo0bdnpGRMTg1NbUvcxqTWDAaWGr/mwGpAyrK7TSHj6bYlZeX7yspKdlJ4/k03K7lg2i+LmD37t2V7PgL+/gXre8dwbXQzcKQCPggIoAwR0QAYY6IAMIcEQGEOSICCHNEBBDmiAggzBERQJgjIoAwR0QAYY7/B1LDyJ6QBLUVAAAAAElFTkSuQmCC".into()
        "data:image/png;base64,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".into()    
    }
    #[cfg(not(target_os = "macos"))] // 128x128 no padding
    {
        //"data:image/png;base64,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".into()
        "data:image/png;base64,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".into()
    }
}
