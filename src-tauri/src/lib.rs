use tauri::{
    image::Image,
    menu::{CheckMenuItemBuilder, MenuBuilder, MenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_dialog::DialogExt;
#[cfg(not(target_os = "windows"))]
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

const AUTO_RELOAD_EVERY: Duration = Duration::from_secs(60 * 60);
const WEBVIEW_RESTART_EVERY: Duration = Duration::from_secs(4 * 60 * 60);
const MAINTENANCE_IDLE_FOR: Duration = Duration::from_secs(3 * 60);
const MAINTENANCE_POLL_EVERY: Duration = Duration::from_secs(10);

// Argumentos extras pro WebView2 (Windows). Sem isso, o Chromium trata a janela
// escondida (na bandeja / minimizada) como ocluída e estrangula o renderer:
// congela timers, rebaixa prioridade do processo e a detecção de oclusão nativa
// pausa o pintar. Resultado: o reload/restart automático que roda com a janela
// escondida carrega pela metade, e ao abrir você espera o WhatsApp terminar de
// bootar. Desligando occlusion + backgrounding, o WhatsApp Web carrega em
// velocidade total mesmo escondido e a janela já abre pronta.
//
// O `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection` é o default
// do wry; additional_browser_args SUBSTITUI esse default, então repetimos aqui
// (e só então acrescentamos CalculateNativeWinOcclusion). No-op fora do Windows.
const WEBVIEW2_BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion --disable-background-timer-throttling --disable-renderer-backgrounding --disable-backgrounding-occluded-windows";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MaintenanceAction {
    Reload,
    Restart,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavigationAction {
    Allow,
    OpenExternal,
    Block,
}

fn original_extension(name: &str) -> Option<String> {
    PathBuf::from(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .map(|ext| ext.to_string())
}

fn keep_original_extension_if_missing(selected: String, original_ext: Option<&str>) -> String {
    let Some(ext) = original_ext.filter(|ext| !ext.is_empty()) else {
        return selected;
    };

    let mut path = PathBuf::from(&selected);
    if path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| !ext.is_empty()) {
        return selected;
    }

    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return selected;
    };
    let clean_name = file_name.trim_end_matches(['.', ' ']).to_string();
    if clean_name.is_empty() {
        return selected;
    }

    path.set_file_name(&clean_name);
    path.set_extension(ext);
    path.to_string_lossy().into_owned()
}

#[tauri::command]
async fn open_save_dialog(app: tauri::AppHandle, suggested_name: Option<String>) -> Result<Option<String>, String> {
    let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
    let suggested_ext = suggested_name
        .as_deref()
        .and_then(original_extension);

    let mut dialog = app.dialog().file();
    if let Some(name) = suggested_name.as_deref() {
        dialog = dialog.set_file_name(name);
    }

    dialog.save_file(move |path| {
        let selected = path.map(|p| {
            keep_original_extension_if_missing(p.to_string(), suggested_ext.as_deref())
        });
        let _ = tx.send(selected);
    });

    rx.recv().map_err(|e| format!("Falha ao abrir dialogo de salvar: {e}"))
}

#[tauri::command]
fn prepare_binary_file(path: String) -> Result<(), String> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .map(|_| ())
        .map_err(|e| format!("Falha ao preparar arquivo para escrita: {e}"))
}

#[tauri::command]
fn append_binary_file(path: String, bytes: Vec<u8>) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .map_err(|e| format!("Falha ao abrir arquivo para append: {e}"))?;

    file.write_all(&bytes)
        .map_err(|e| format!("Falha ao escrever bytes no arquivo: {e}"))
}

#[tauri::command]
fn open_external_url(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err("Apenas URLs HTTP/HTTPS sao permitidas".to_string());
    }

    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| format!("Falha ao abrir URL externa: {e}"))
}

#[tauri::command]
fn focus_main_window(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

#[cfg(target_os = "windows")]
fn quiet_hours_state_blocks_notifications(state: Option<u32>) -> bool {
    state.is_some_and(|state| state != 0)
}

#[cfg(target_os = "windows")]
fn utf16le_contains_ascii(data: &[u8], needle: &str) -> bool {
    let pattern: Vec<u8> = needle.bytes().flat_map(|b| [b, 0]).collect();
    data.windows(pattern.len())
        .any(|chunk| chunk == pattern.as_slice())
}

#[cfg(target_os = "windows")]
fn cloudstore_quiet_hours_blocks_notifications(data: &[u8]) -> Option<bool> {
    if utf16le_contains_ascii(data, "Microsoft.QuietHoursProfile.PriorityOnly")
        || utf16le_contains_ascii(data, "Microsoft.QuietHoursProfile.AlarmsOnly")
    {
        Some(true)
    } else if utf16le_contains_ascii(data, "Microsoft.QuietHoursProfile.Unrestricted") {
        Some(false)
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn read_cloudstore_quiet_hours_settings() -> Option<Vec<u8>> {
    use windows::{
        core::HSTRING,
        Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, REG_VALUE_TYPE, RRF_RT_REG_BINARY,
        },
    };

    let mut data = vec![0u8; 512];
    let mut data_size = data.len() as u32;
    let mut value_type = REG_VALUE_TYPE::default();
    let subkey = HSTRING::from(
        "Software\\Microsoft\\Windows\\CurrentVersion\\CloudStore\\Store\\DefaultAccount\\Current\\default$windows.data.donotdisturb.quiethourssettings\\windows.data.donotdisturb.quiethourssettings",
    );
    let value_name = HSTRING::from("Data");

    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &value_name,
            RRF_RT_REG_BINARY,
            Some(&mut value_type),
            Some(data.as_mut_ptr().cast()),
            Some(&mut data_size),
        )
    };

    if status.0 != 0 {
        return None;
    }

    data.truncate(data_size as usize);
    Some(data)
}

#[cfg(target_os = "windows")]
fn read_quiet_hours_service_state() -> Option<u32> {
    use windows::{
        core::HSTRING,
        Win32::System::Registry::{
            RegGetValueW, HKEY_CURRENT_USER, REG_VALUE_TYPE, RRF_RT_REG_DWORD,
        },
    };

    let mut data = 0u32;
    let mut data_size = std::mem::size_of::<u32>() as u32;
    let mut value_type = REG_VALUE_TYPE::default();
    let subkey = HSTRING::from("Software\\Microsoft\\Windows\\CurrentVersion\\Notifications\\QuietHours");
    let value_name = HSTRING::from("QuietHoursServiceState");

    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            &subkey,
            &value_name,
            RRF_RT_REG_DWORD,
            Some(&mut value_type),
            Some((&mut data as *mut u32).cast()),
            Some(&mut data_size),
        )
    };

    (status.0 == 0).then_some(data)
}

#[cfg(target_os = "windows")]
fn windows_do_not_disturb_enabled() -> bool {
    if let Some(blocked) = read_cloudstore_quiet_hours_settings()
        .and_then(|data| cloudstore_quiet_hours_blocks_notifications(&data))
    {
        return blocked;
    }

    quiet_hours_state_blocks_notifications(read_quiet_hours_service_state())
}

/// Apaga avatares `wa_lite_avatar_*.png` do `%TEMP%` com mais de 24h. Roda no
/// startup. Não falha; erros são silenciosos (é só hygiene).
fn cleanup_old_avatar_temp_files() {
    let cutoff = match std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(24 * 60 * 60))
    {
        Some(t) => t,
        None => return,
    };

    let temp_dir = std::env::temp_dir();
    let entries = match std::fs::read_dir(&temp_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !name.starts_with("wa_lite_avatar_") || !name.ends_with(".png") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
}

/// Monta o XML do toast manualmente, cria uma `ToastNotification`, seta `Tag` e
/// `Group` (que o wrapper `tauri-winrt-notification` 0.7.2 só expõe pra toasts
/// de progresso) e registra o handler de Activated com bridge pra JS. Tag+Group
/// é o que faz o Windows substituir a toast anterior do mesmo chat em vez de
/// empilhar — limite de 64 chars cada (Win10 1903+).
#[cfg(target_os = "windows")]
fn show_chat_toast(
    app: &tauri::AppHandle,
    app_id: &str,
    title: &str,
    body: &str,
    avatar_path: Option<&std::path::Path>,
    tag: &str,
    click_id: String,
) -> Result<(), String> {
    use windows::{
        core::{IInspectable, HSTRING},
        Data::Xml::Dom::XmlDocument,
        Foundation::TypedEventHandler,
        UI::Notifications::{ToastNotification, ToastNotificationManager},
    };

    fn xml_escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            match c {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                '"' => out.push_str("&quot;"),
                '\'' => out.push_str("&apos;"),
                _ => out.push(c),
            }
        }
        out
    }

    let image_xml = match avatar_path {
        Some(p) => format!(
            r#"<image placement="appLogoOverride" hint-crop="circle" src="file:///{}" alt="avatar" />"#,
            xml_escape(&p.display().to_string()),
        ),
        None => String::new(),
    };

    let xml = format!(
        r#"<toast duration="short"><visual><binding template="ToastGeneric">{image}<text>{title}</text><text>{body}</text></binding></visual></toast>"#,
        image = image_xml,
        title = xml_escape(title),
        body = xml_escape(body),
    );

    let xml_doc = XmlDocument::new().map_err(|e| format!("XmlDocument::new: {e}"))?;
    xml_doc
        .LoadXml(&HSTRING::from(xml.as_str()))
        .map_err(|e| format!("LoadXml: {e}"))?;

    let toast = ToastNotification::CreateToastNotification(&xml_doc)
        .map_err(|e| format!("CreateToastNotification: {e}"))?;

    if !tag.is_empty() {
        let tag_sanitized: String = tag
            .chars()
            .filter(|c| !c.is_control() && !c.is_whitespace())
            .take(64)
            .collect();
        if !tag_sanitized.is_empty() {
            toast
                .SetTag(&HSTRING::from(tag_sanitized.as_str()))
                .map_err(|e| format!("SetTag: {e}"))?;
            toast
                .SetGroup(&HSTRING::from("wa-lite"))
                .map_err(|e| format!("SetGroup: {e}"))?;
        }
    }

    let app_handle = app.clone();
    let handler = TypedEventHandler::<ToastNotification, IInspectable>::new(
        move |_sender, _args| {
            if let Some(win) = app_handle.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
                if !click_id.is_empty() {
                    let escaped = click_id
                        .replace('\\', "\\\\")
                        .replace('"', "\\\"");
                    let js = format!(
                        "window.__waLiteOnNotifClick && window.__waLiteOnNotifClick(\"{}\");",
                        escaped
                    );
                    let _ = win.eval(&js);
                }
            }
            Ok(())
        },
    );

    toast
        .Activated(&handler)
        .map_err(|e| format!("Activated: {e}"))?;

    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(app_id))
        .map_err(|e| format!("CreateToastNotifierWithId: {e}"))?;
    notifier
        .Show(&toast)
        .map_err(|e| format!("Show: {e}"))?;

    Ok(())
}

#[tauri::command]
fn send_notification(
    app: tauri::AppHandle,
    title: String,
    body: String,
    image_bytes: Option<Vec<u8>>,
    notification_id: Option<String>,
    tag: Option<String>,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        if windows_do_not_disturb_enabled() {
            return Ok(());
        }

        let app_id = app.config().identifier.clone();

        let clean_title = {
            let t = title.trim();
            if t.is_empty() {
                "WhatsApp Lite".to_string()
            } else {
                t.to_string()
            }
        };

        let clean_body = {
            let b = body.trim();
            if b.is_empty() {
                "Nova mensagem".to_string()
            } else {
                b.to_string()
            }
        };

        let notif_id = notification_id.unwrap_or_default();
        let tag_str = tag.unwrap_or_default();

        // Avatar: chaveia o arquivo temp pelo tag (chat id) quando existe — mesma
        // pessoa = mesmo arquivo, sem proliferação. Fallback pro notif_id quando
        // não tem tag (notificações sem chat associado).
        let avatar_path: Option<std::path::PathBuf> = match image_bytes {
            Some(bytes) if !bytes.is_empty() => {
                let key: &str = if !tag_str.is_empty() {
                    &tag_str
                } else {
                    &notif_id
                };
                let safe_key: String = key
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric())
                    .take(64)
                    .collect();
                let filename = if safe_key.is_empty() {
                    "wa_lite_avatar.png".to_string()
                } else {
                    format!("wa_lite_avatar_{}.png", safe_key)
                };
                let mut path = std::env::temp_dir();
                path.push(filename);
                match OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&path)
                    .and_then(|mut f| f.write_all(&bytes))
                {
                    Ok(_) => Some(path),
                    Err(_) => None,
                }
            }
            _ => None,
        };

        return show_chat_toast(
            &app,
            &app_id,
            &clean_title,
            &clean_body,
            avatar_path.as_deref(),
            &tag_str,
            notif_id,
        );
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (image_bytes, notification_id, tag);
        app.notification()
            .builder()
            .id(1)
            .title(&title)
            .body(&body)
            .show()
            .map_err(|e| format!("Falha ao enviar notificação: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_dialog_keeps_original_extension_when_user_deletes_it() {
        let path = keep_original_extension_if_missing(
            r"C:\Users\Levi\Desktop\novo_nome".to_string(),
            Some("pdf"),
        );
        assert!(path.ends_with(r"novo_nome.pdf"));
    }

    #[test]
    fn save_dialog_respects_extension_user_typed() {
        let path = keep_original_extension_if_missing(
            r"C:\Users\Levi\Desktop\novo_nome.txt".to_string(),
            Some("pdf"),
        );
        assert!(path.ends_with(r"novo_nome.txt"));
    }

    #[test]
    fn maintenance_waits_for_idle_grace() {
        let now = Instant::now();
        assert_eq!(
            due_maintenance(
                now,
                now - Duration::from_secs(1),
                now + Duration::from_secs(1),
                Some(now - Duration::from_secs(179)),
            ),
            None
        );
    }

    #[test]
    fn maintenance_restart_wins_over_reload() {
        let now = Instant::now();
        assert_eq!(
            due_maintenance(
                now,
                now - Duration::from_secs(1),
                now - Duration::from_secs(1),
                Some(now - MAINTENANCE_IDLE_FOR),
            ),
            Some(MaintenanceAction::Restart)
        );
    }

    #[test]
    fn navigation_blocks_about_blank_without_opener() {
        let url = tauri::Url::parse("about:blank").unwrap();
        assert_eq!(navigation_action(&url), NavigationAction::Block);
    }

    #[test]
    fn navigation_opens_only_external_http_urls() {
        let whatsapp = tauri::Url::parse("https://web.whatsapp.com/").unwrap();
        let external = tauri::Url::parse("https://example.com/").unwrap();

        assert_eq!(navigation_action(&whatsapp), NavigationAction::Allow);
        assert_eq!(navigation_action(&external), NavigationAction::OpenExternal);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn quiet_hours_blocks_only_nonzero_state() {
        assert!(!quiet_hours_state_blocks_notifications(None));
        assert!(!quiet_hours_state_blocks_notifications(Some(0)));
        assert!(quiet_hours_state_blocks_notifications(Some(1)));
        assert!(quiet_hours_state_blocks_notifications(Some(2)));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn cloudstore_quiet_hours_blocks_only_restricted_profiles() {
        fn utf16le(s: &str) -> Vec<u8> {
            s.bytes().flat_map(|b| [b, 0]).collect()
        }

        assert_eq!(
            cloudstore_quiet_hours_blocks_notifications(&utf16le(
                "Microsoft.QuietHoursProfile.PriorityOnly"
            )),
            Some(true)
        );
        assert_eq!(
            cloudstore_quiet_hours_blocks_notifications(&utf16le(
                "Microsoft.QuietHoursProfile.AlarmsOnly"
            )),
            Some(true)
        );
        assert_eq!(
            cloudstore_quiet_hours_blocks_notifications(&utf16le(
                "Microsoft.QuietHoursProfile.Unrestricted"
            )),
            Some(false)
        );
        assert_eq!(
            cloudstore_quiet_hours_blocks_notifications(b"no profile"),
            None
        );
    }
}

/// JavaScript injetado em document_start na WebView, antes do bundle do
/// WhatsApp Web rodar. Responsável por:
/// 1. Proxy do `Notification`: WhatsApp Web chama `new Notification(...)` e a
///    chamada é forwardada para o toast nativo (`tauri-winrt-notification`),
///    com avatar do contato, branding "WhatsApp Lite" e bridge de click que
///    dispara o `onclick` original (abre a conversa certa, não só foca janela).
///    Toda a lógica de quando notificar (mute, foco da janela, preview ligado,
///    canais, status) fica com o próprio WhatsApp Web.
/// 2. Atalhos: Ctrl+W (Escape) e Ctrl+Seta-pra-cima (editar última mensagem).
/// 3. Diálogo nativo para salvar downloads.
/// 4. Abertura segura de links externos no navegador do sistema.
/// 5. Defesa em camadas (com o guard `on_navigation`) contra navegação
///    programática externa via `window.open` / `location.assign|replace`.
const WHATSAPP_PATCHES_JS: &str = r#"
  (() => {

        if (window.__whatsapp_lite_patches_installed) {
            return;
        }
        window.__whatsapp_lite_patches_installed = true;

        const tauriInvoke = async (cmd, payload) => {
            const internals = window.__TAURI_INTERNALS__;
            if (internals && typeof internals.invoke === 'function') {
                return internals.invoke(cmd, payload || {});
            }
            if (window.__TAURI__ && window.__TAURI__.core && typeof window.__TAURI__.core.invoke === 'function') {
                return window.__TAURI__.core.invoke(cmd, payload || {});
            }
            throw new Error('Tauri invoke indisponivel');
        };

        // === Proxy do Notification ==============================================
        // Quando WhatsApp Web faz `new Notification(title, options)`, instanciamos
        // a nossa classe, forwardamos `title` + `body` + `icon` (avatar) para o
        // Toast nativo (Rust), e mantemos o objeto em __waNotifs pra que, quando
        // o usuário clicar na toast, possamos invocar de volta o `onclick` que o
        // WhatsApp Web instalou — assim o app navega para a conversa certa.
        const __waNotifs = new Map();
        let __waNotifCounter = 0;

        class WALiteNotification extends EventTarget {
            constructor(title, options) {
                super();
                options = options || {};

                this.title = String(title || '');
                this.body = String(options.body || '');
                this.icon = options.icon || '';
                this.badge = options.badge || '';
                this.tag = options.tag || '';
                this.data = options.data;
                this.silent = !!options.silent;
                this.requireInteraction = !!options.requireInteraction;
                this.lang = options.lang || '';
                this.dir = options.dir || 'auto';
                this.timestamp = options.timestamp || Date.now();

                this.onclick = null;
                this.onshow = null;
                this.onclose = null;
                this.onerror = null;

                const notifId = String(++__waNotifCounter);
                this.__waNotifId = notifId;
                __waNotifs.set(notifId, this);
                // GC: limpa depois de 60s pra não vazar memória
                setTimeout(() => __waNotifs.delete(notifId), 60000);

                this.__waSend().catch((err) => {
                    console.error('[WhatsApp Lite] Falha ao enviar notificação:', err);
                    this.__waFire('error');
                });
            }

            async __waSend() {
                let imageBytes = null;
                const iconUrl = this.icon;
                if (iconUrl) {
                    try {
                        const resp = await fetch(iconUrl);
                        if (resp.ok) {
                            const buf = await resp.arrayBuffer();
                            imageBytes = Array.from(new Uint8Array(buf));
                        }
                    } catch (_) {
                        // Sem avatar — segue sem
                    }
                }

                await tauriInvoke('send_notification', {
                    title: this.title,
                    body: this.body,
                    imageBytes: imageBytes,
                    notificationId: this.__waNotifId,
                    // `tag` é o chat id que o WhatsApp Web põe no options.tag
                    // (tipicamente "<numero>@c.us" ou "<id>@g.us"). É o que o
                    // Rust usa pra setar Toast.Tag/Group e fazer substituição
                    // por chat no Windows.
                    tag: this.tag || ''
                });

                this.__waFire('show');
            }

            __waFire(name, evInit) {
                let ev;
                try { ev = new Event(name, evInit || {}); }
                catch (_) { ev = new Event(name); }
                const handler = this['on' + name];
                if (typeof handler === 'function') {
                    try { handler.call(this, ev); }
                    catch (e) { console.error('[WhatsApp Lite] handler ' + name + ':', e); }
                }
                try { this.dispatchEvent(ev); } catch (_) {}
            }

            close() {
                __waNotifs.delete(this.__waNotifId);
                this.__waFire('close');
            }
        }

        Object.defineProperty(WALiteNotification, 'permission', {
            get() { return 'granted'; },
            configurable: true
        });
        WALiteNotification.requestPermission = function(cb) {
            if (typeof cb === 'function') {
                try { cb('granted'); } catch (_) {}
            }
            return Promise.resolve('granted');
        };
        WALiteNotification.maxActions = 0;

        try {
            Object.defineProperty(window, 'Notification', {
                value: WALiteNotification,
                writable: true,
                configurable: true
            });
        } catch (_) {
            try { window.Notification = WALiteNotification; } catch (_) {}
        }

        // Chamado pelo Rust via eval quando a toast nativa é clicada.
        // Re-dispara o onclick que o WhatsApp Web instalou no objeto Notification
        // (que tipicamente faz `window.focus()` + abre a conversa correspondente).
        window.__waLiteOnNotifClick = function(notifId) {
            const notif = __waNotifs.get(String(notifId));
            if (!notif) return;
            try {
                notif.__waFire('click', { cancelable: true });
            } catch (e) {
                console.error('[WhatsApp Lite] Erro no click handler:', e);
            }
            __waNotifs.delete(String(notifId));
        };

        // === Atalhos de teclado ===
        if (!window.__whatsapp_lite_shortcuts_installed) {
            window.__whatsapp_lite_shortcuts_installed = true;

            const findChatInput = () => {
                // aria-label distingue chat input de search input — search usa
                // "Caixa de texto de pesquisa"/"Search input textbox"; chat usa
                // "Digite uma mensagem"/"Type a message". É o jeito mais estável.
                const byAria = document.querySelector(
                    'div[contenteditable="true"][aria-label*="mensagem" i],'
                    + ' div[contenteditable="true"][aria-label*="message" i],'
                    + ' div[contenteditable="true"][aria-label*="mensaje" i]'
                );
                if (byAria) return byAria;

                // Fallback histórico (data-tab WhatsApp Web)
                const byTab = document.querySelector('div[contenteditable="true"][data-tab="10"]');
                if (byTab) return byTab;

                // Fallback geográfico: chat input é o contenteditable visível mais
                // próximo do fim da tela. Search fica no topo do painel esquerdo,
                // então o "mais baixo" nunca é ela.
                const editables = Array.from(document.querySelectorAll('div[contenteditable="true"]'));
                let best = null;
                let bestY = -Infinity;
                for (const ed of editables) {
                    const r = ed.getBoundingClientRect();
                    if (r.height < 16 || r.width < 100) continue;
                    if (r.top > bestY) {
                        best = ed;
                        bestY = r.top;
                    }
                }
                return best;
            };

            document.addEventListener('keydown', (e) => {
                // Ctrl+W -> Escape, fecha conversa. Exige !altKey: no ABNT2
                // brasileiro Ctrl+(left Alt) é alias de AltGr, e AltGr+W gera
                // "?" no SO. Se não excluíssemos altKey, qualquer tentativa de
                // digitar "?" no input do chat fecharia a conversa.
                if (e.ctrlKey && !e.shiftKey && !e.altKey && e.code === 'KeyW') {
                    e.preventDefault();
                    e.stopPropagation();

                    const escEvent = new KeyboardEvent('keydown', {
                        key: 'Escape',
                        code: 'Escape',
                        keyCode: 27,
                        which: 27,
                        bubbles: true,
                        cancelable: true
                    });

                    document.dispatchEvent(escEvent);
                    if (document.activeElement) {
                        document.activeElement.dispatchEvent(escEvent);
                    }
                    return;
                }

                // Ctrl+Seta para cima -> editar ultima mensagem enviada
                if (e.ctrlKey && !e.shiftKey && !e.altKey && e.key === 'ArrowUp') {
                    e.preventDefault();
                    e.stopPropagation();

                    const input = findChatInput();
                    if (input) {
                        input.focus();
                        input.dispatchEvent(new KeyboardEvent('keydown', {
                            key: 'ArrowUp',
                            code: 'ArrowUp',
                            keyCode: 38,
                            which: 38,
                            bubbles: true,
                            cancelable: true
                        }));
                    }
                    return;
                }

                // Ctrl+Shift+E -> abre o seletor de emoji (clica no botão do footer).
                // WhatsApp Web já trata abrir/fechar via toggle, então um clique
                // basta; um segundo Ctrl+Shift+E fecha.
                if (e.ctrlKey && e.shiftKey && !e.altKey && (e.key === 'e' || e.key === 'E')) {
                    e.preventDefault();
                    e.stopPropagation();

                    // Múltiplas heurísticas porque WhatsApp varia o atributo.
                    const emojiBtn =
                        document.querySelector('button[aria-label="Inserir emoji" i]')
                        || document.querySelector('button[aria-label*="emoji" i]')
                        || document.querySelector('[data-icon="smiley"]')?.closest('button, [role="button"]')
                        || document.querySelector('[data-icon="smiley-emoji-input"]')?.closest('button, [role="button"]')
                        || document.querySelector('button[title*="emoji" i]')
                        || document.querySelector('footer button:has(span[data-icon*="smiley"])');

                    if (emojiBtn) {
                        emojiBtn.click();
                    }
                    return;
                }

                // Ctrl+Alt+Q -> insere "/" no input do chat.
                // Detecta pelo CÓDIGO físico da tecla (KeyQ), não pela e.key,
                // porque em teclado ABNT2 (Brasil) Ctrl+(left Alt) é alias de
                // AltGr no Windows, e AltGr+Q gera "/" no nível do SO. Sem
                // isso, o e.key chegaria como "/" e a condição não casaria.
                if (e.ctrlKey && e.altKey && !e.shiftKey && e.code === 'KeyQ') {
                    e.preventDefault();
                    e.stopPropagation();

                    const input = findChatInput();
                    if (input && input.isContentEditable) {
                        input.focus();
                        // setTimeout(0) deixa o focus aplicar antes do
                        // execCommand, que opera no elemento focado AGORA.
                        // Sem isso, se o foco anterior era a busca, o "/"
                        // ia parar lá.
                        setTimeout(() => {
                            input.focus();
                            try {
                                document.execCommand('insertText', false, '/');
                            } catch (_) {
                                const ev = new InputEvent('beforeinput', {
                                    inputType: 'insertText',
                                    data: '/',
                                    bubbles: true,
                                    cancelable: true
                                });
                                input.dispatchEvent(ev);
                            }
                        }, 0);
                    }
                    return;
                }
            }, true);
        }

        // === Downloads com dialogo nativo + abertura de links externos ===
        if (!window.__whatsapp_lite_link_download_installed) {
            window.__whatsapp_lite_link_download_installed = true;

            const sanitizeFileName = (rawName) => {
                let name = (rawName || '').trim();
                if (!name) return 'arquivo.bin';

                name = name.replace(/[<>:"/\\|?*\x00-\x1F]/g, '_').replace(/\s+/g, ' ');
                name = name.replace(/^\.+/, '').trim();
                if (!name) return 'arquivo.bin';

                if (name.length > 180) {
                    const extMatch = name.match(/(\.[a-zA-Z0-9]{1,10})$/);
                    const ext = extMatch ? extMatch[1] : '';
                    const baseMax = 180 - ext.length;
                    name = name.slice(0, Math.max(1, baseMax)) + ext;
                }

                return name;
            };

            const parseContentDispositionFileName = (headerVal) => {
                if (!headerVal) return '';

                const utf8Match = headerVal.match(/filename\*=UTF-8''([^;]+)/i);
                if (utf8Match && utf8Match[1]) {
                    try {
                        return decodeURIComponent(utf8Match[1].trim().replace(/^"|"$/g, ''));
                    } catch (_) {}
                }

                const normalMatch = headerVal.match(/filename="?([^";]+)"?/i);
                if (normalMatch && normalMatch[1]) {
                    return normalMatch[1].trim();
                }

                return '';
            };

            const inferFileNameFromAnchor = (anchor, urlObj, response) => {
                const fromDownload = (anchor.getAttribute('download') || '').trim();
                if (fromDownload) return sanitizeFileName(fromDownload);

                if (response) {
                    const cd = response.headers.get('content-disposition') || '';
                    const fromHeader = parseContentDispositionFileName(cd);
                    if (fromHeader) return sanitizeFileName(fromHeader);
                }

                if (urlObj && urlObj.pathname) {
                    const pathPart = (urlObj.pathname.split('/').pop() || '').trim();
                    if (pathPart) {
                        try {
                            return sanitizeFileName(decodeURIComponent(pathPart));
                        } catch (_) {
                            return sanitizeFileName(pathPart);
                        }
                    }
                }

                return 'arquivo.bin';
            };

            const isLikelyChatDownload = (anchor, urlObj) => {
                if (anchor.hasAttribute('download')) return true;
                if (urlObj.protocol === 'blob:') return true;

                const textSignals = [
                    anchor.getAttribute('aria-label') || '',
                    anchor.getAttribute('title') || '',
                    anchor.textContent || ''
                ].join(' ').toLowerCase();

                if (textSignals.includes('baixar') || textSignals.includes('download')) return true;

                const path = (urlObj.pathname || '').toLowerCase();
                const query = (urlObj.search || '').toLowerCase();
                if (path.includes('/download/') || query.includes('download=')) return true;
                if (query.includes('dl=1') || query.includes('dl=true')) return true;

                const fileExtPattern = /\.(pdf|doc|docx|xls|xlsx|ppt|pptx|zip|rar|7z|jpg|jpeg|png|gif|webp|mp4|mp3|wav|ogg|txt|csv)$/i;
                if (fileExtPattern.test(path)) return true;

                return false;
            };

            const isExternalHttp = (urlObj) => {
                const protocol = (urlObj.protocol || '').toLowerCase();
                if (protocol !== 'http:' && protocol !== 'https:') return false;
                return urlObj.origin !== window.location.origin;
            };

            const saveUrlToChosenPath = async (url, anchor, urlObj) => {
                const suggested = inferFileNameFromAnchor(anchor, urlObj, null);
                const selectedPath = await tauriInvoke('open_save_dialog', { suggestedName: suggested });
                if (!selectedPath) return;

                const response = await fetch(url, { credentials: 'include' });
                if (!response.ok) {
                    throw new Error('Falha no download HTTP ' + response.status);
                }

                await tauriInvoke('prepare_binary_file', { path: selectedPath });

                if (response.body && response.body.getReader) {
                    const reader = response.body.getReader();
                    while (true) {
                        const part = await reader.read();
                        if (part.done) break;
                        if (part.value && part.value.length) {
                            await tauriInvoke('append_binary_file', {
                                path: selectedPath,
                                bytes: Array.from(part.value)
                            });
                        }
                    }
                } else {
                    const buffer = await response.arrayBuffer();
                    const bytes = Array.from(new Uint8Array(buffer));
                    await tauriInvoke('append_binary_file', { path: selectedPath, bytes });
                }
            };

            document.addEventListener('click', (e) => {
                const target = e.target;
                if (!target || !target.closest) return;

                const anchor = target.closest('a[href]');
                if (!anchor) return;

                const hrefAttr = (anchor.getAttribute('href') || '').trim();
                if (!hrefAttr || hrefAttr.startsWith('#') || hrefAttr.toLowerCase().startsWith('javascript:')) {
                    return;
                }

                let urlObj;
                try {
                    urlObj = new URL(anchor.href, window.location.href);
                } catch (_) {
                    return;
                }

                const protocol = (urlObj.protocol || '').toLowerCase();
                const canHandle = protocol === 'http:' || protocol === 'https:' || protocol === 'blob:';
                if (!canHandle) return;

                if (isLikelyChatDownload(anchor, urlObj)) {
                    e.preventDefault();
                    e.stopPropagation();

                    saveUrlToChosenPath(urlObj.href, anchor, urlObj).catch((err) => {
                        console.error('[WhatsApp Lite] Falha ao salvar download:', err);
                    });
                    return;
                }

                if (isExternalHttp(urlObj)) {
                    e.preventDefault();
                    e.stopPropagation();

                    tauriInvoke('open_external_url', { url: urlObj.href }).catch((err) => {
                        console.error('[WhatsApp Lite] Falha ao abrir URL externa:', err);
                    });
                }
            }, true);
        }

        // === Rede de segurança extra contra navegação externa programática ===
        // O guard real é o on_navigation no nível da WebView (Rust). Estes hooks
        // em JS evitam que o WhatsApp Web sequer dispare a tentativa de navegar
        // para facebook.com / Meta Accounts Center via window.open, location.assign
        // ou location.replace — APIs que o click handler em <a> não cobre.
        if (!window.__whatsapp_lite_nav_hooks_installed) {
            window.__whatsapp_lite_nav_hooks_installed = true;

            const isExternalNavUrl = (urlStr) => {
                try {
                    const u = new URL(String(urlStr), window.location.href);
                    if (u.protocol !== 'http:' && u.protocol !== 'https:') return false;
                    return u.origin !== window.location.origin;
                } catch (_) {
                    return false;
                }
            };

            const __waOrigOpen = window.open.bind(window);
            window.open = function(url, target, features) {
                if (url && isExternalNavUrl(url)) {
                    tauriInvoke('open_external_url', { url: String(url) }).catch(console.error);
                    return null;
                }
                return __waOrigOpen(url, target, features);
            };

            try {
                const __waOrigAssign = window.location.assign.bind(window.location);
                const __waOrigReplace = window.location.replace.bind(window.location);

                window.location.assign = function(url) {
                    if (isExternalNavUrl(url)) {
                        tauriInvoke('open_external_url', { url: String(url) }).catch(console.error);
                        return;
                    }
                    return __waOrigAssign(url);
                };

                window.location.replace = function(url) {
                    if (isExternalNavUrl(url)) {
                        tauriInvoke('open_external_url', { url: String(url) }).catch(console.error);
                        return;
                    }
                    return __waOrigReplace(url);
                };
            } catch (e) {
                console.error('[WhatsApp Lite] Falha ao instalar hooks de location:', e);
            }
        }

        // Troca o banner "Baixar o WhatsApp" pela logo Lite. Sem rAF/setTimeout:
        // o observer roda no microtask da mutacao, antes do browser pintar o card.
        const __waLiteLogoUrl = "data:image/png;base64,__WA_LITE_LOGO_B64__";
        const __waLogoAttr = 'data-wa-lite-logo-placeholder';
        const __waTitleRe = /\b(baixar|baixe|baixa|obter|obtenha|get|download|descargar|descarga|t[ée]l[ée]charger|herunterladen|scarica)\b[^.\n]{0,60}\bwhatsapp\b/i;
        const __waButtonRe = /^(baixar|baixe|download|obter|obtenha|get|descargar|descarga|t[ée]l[ée]charger|herunterladen|scarica)(\s|$)/i;

        const __waEnsureLogoStyle = () => {
            if (document.getElementById('wa-lite-logo-style')) return;
            const style = document.createElement('style');
            style.id = 'wa-lite-logo-style';
            style.textContent = ''
                + '[' + __waLogoAttr + '="1"] {'
                +   'display: block !important;'
                +   'width: min(var(--wa-lite-logo-width, 440px), 70vw) !important;'
                +   'height: min(var(--wa-lite-logo-height, 360px), 55vh) !important;'
                +   'background-image: url("' + __waLiteLogoUrl + '") !important;'
                +   'background-repeat: no-repeat !important;'
                +   'background-position: center center !important;'
                +   'background-size: contain !important;'
                +   'pointer-events: none !important;'
                +   'box-shadow: none !important;'
                +   'border: 0 !important;'
                + '}';
            (document.head || document.documentElement).appendChild(style);
        };

        const __waIsVisible = (el) => {
            if (!el) return false;
            if (el.offsetParent === null && el.tagName !== 'BODY') {
                const r = el.getBoundingClientRect();
                if (r.width === 0 && r.height === 0) return false;
            }
            return true;
        };

        const __waFindBannerCard = () => {
            const nodes = document.querySelectorAll(
                'h1, h2, h3, h4, h5, h6, [role="heading"], button, [role="button"], a, [role="link"], div, span, p'
            );

            // ponytail: text heuristic; use a stable WhatsApp selector if one ever exists.
            for (const node of nodes) {
                const ownText = (node.textContent || '').replace(/\s+/g, ' ').trim();
                if (ownText.length < 5 || ownText.length > 180) continue;
                if (!__waTitleRe.test(ownText) && !__waButtonRe.test(ownText)) continue;
                if (!__waIsVisible(node)) continue;

                for (let el = node, i = 0; i < 12 && el && el !== document.body; i++, el = el.parentElement) {
                    const text = (el.textContent || '').replace(/\s+/g, ' ').trim();
                    if (text.length > 900) break;
                    if (!__waTitleRe.test(text) || !__waButtonRe.test(text)) continue;

                    const r = el.getBoundingClientRect();
                    if (r.width >= 240 && r.width <= 620 && r.height >= 220 && r.height <= 620) {
                        return el;
                    }
                }
            }

            return null;
        };

        const __waCreateLogo = (card) => {
            const r = card.getBoundingClientRect();
            const logo = document.createElement('div');
            logo.setAttribute(__waLogoAttr, '1');
            logo.setAttribute('aria-hidden', 'true');
            logo.style.setProperty('--wa-lite-logo-width', Math.max(240, Math.round(r.width)) + 'px');
            logo.style.setProperty('--wa-lite-logo-height', Math.max(220, Math.round(r.height)) + 'px');
            return logo;
        };

        const __waReplaceBanner = () => {
            try { __waEnsureLogoStyle(); } catch (e) {}
            const card = __waFindBannerCard();
            if (!card) return false;
            if (card.getAttribute(__waLogoAttr) === '1') return false;
            for (const logo of document.querySelectorAll('[' + __waLogoAttr + '="1"]')) {
                logo.remove();
            }
            card.replaceWith(__waCreateLogo(card));
            return true;
        };

        window.__waLiteReplaceBanner = __waReplaceBanner;
        window.__waLiteFindCard = __waFindBannerCard;

        new MutationObserver(() => {
            try { __waReplaceBanner(); } catch (e) {}
        }).observe(
            document.documentElement || document,
            { childList: true, characterData: true, subtree: true }
        );
        try { __waReplaceBanner(); } catch (e) {}

  })();
"#;

fn due_maintenance(
    now: Instant,
    next_reload: Instant,
    next_restart: Instant,
    inactive_since: Option<Instant>,
) -> Option<MaintenanceAction> {
    if !inactive_since
        .map(|since| now.duration_since(since) >= MAINTENANCE_IDLE_FOR)
        .unwrap_or(false)
    {
        return None;
    }

    if now >= next_restart {
        Some(MaintenanceAction::Restart)
    } else if now >= next_reload {
        Some(MaintenanceAction::Reload)
    } else {
        None
    }
}

fn main_window_is_inactive(win: &tauri::WebviewWindow) -> bool {
    let focused = win.is_focused().unwrap_or(true);
    let visible = win.is_visible().unwrap_or(true);
    let minimized = win.is_minimized().unwrap_or(false);
    !focused || !visible || minimized
}

fn whatsapp_patches_script() -> String {
    WHATSAPP_PATCHES_JS.replace("__WA_LITE_LOGO_B64__", &{
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .encode(include_bytes!("../../WhatsApp Lite Logo.png"))
    })
}

fn navigation_action(url: &tauri::Url) -> NavigationAction {
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return NavigationAction::Block;
    }

    let host = url.host_str().unwrap_or("");
    if host == "web.whatsapp.com"
        || host == "whatsapp.com"
        || host.ends_with(".whatsapp.com")
        || host.ends_with(".whatsapp.net")
    {
        NavigationAction::Allow
    } else {
        NavigationAction::OpenExternal
    }
}

fn allow_whatsapp_navigation(app: tauri::AppHandle, url: &tauri::Url) -> bool {
    match navigation_action(url) {
        NavigationAction::Allow => true,
        NavigationAction::OpenExternal => {
            let _ = app.opener().open_url(url.as_str(), None::<&str>);
            false
        }
        NavigationAction::Block => false,
    }
}

fn install_close_to_tray(main_window: &tauri::WebviewWindow) {
    let win_handle = main_window.app_handle().clone();
    main_window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            if let Some(win) = win_handle.get_webview_window("main") {
                let _ = win.hide();
            }
        }
    });
}

fn create_main_window(
    app: &tauri::AppHandle,
    visible: bool,
    focused: bool,
) -> tauri::Result<tauri::WebviewWindow> {
    let nav_app_handle = app.clone();
    let main_window = WebviewWindowBuilder::new(
        app,
        "main",
        WebviewUrl::External("https://web.whatsapp.com".parse().unwrap()),
    )
    .title("WhatsAppLite")
    .inner_size(1100.0, 720.0)
    .min_inner_size(780.0, 480.0)
    .center()
    .decorations(true)
    .resizable(true)
    .visible(visible)
    .focused(focused)
    .additional_browser_args(WEBVIEW2_BROWSER_ARGS)
    .disable_drag_drop_handler()
    .initialization_script(&whatsapp_patches_script())
    .on_navigation(move |url| allow_whatsapp_navigation(nav_app_handle.clone(), url))
    .build()?;

    install_close_to_tray(&main_window);
    Ok(main_window)
}

fn restart_main_webview(app: &tauri::AppHandle) -> Result<(), String> {
    let old = app
        .get_webview_window("main")
        .ok_or_else(|| "Janela principal nao encontrada".to_string())?;
    let was_visible = old.is_visible().unwrap_or(false);
    let was_minimized = old.is_minimized().unwrap_or(false);
    let show_new = was_visible && !was_minimized;

    old.destroy()
        .map_err(|e| format!("Falha ao destruir WebView antiga: {e}"))?;

    // ponytail: short wait for Tauri to free the "main" label after destroy.
    for _ in 0..10 {
        if app.get_webview_window("main").is_none() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let new = create_main_window(app, show_new, false)
        .map_err(|e| format!("Falha ao recriar WebView: {e}"))?;
    if was_minimized {
        let _ = new.minimize();
    }
    if !was_visible {
        let _ = new.hide();
    }
    Ok(())
}

fn start_webview_maintenance(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut next_reload = Instant::now() + AUTO_RELOAD_EVERY;
        let mut next_restart = Instant::now() + WEBVIEW_RESTART_EVERY;
        let mut inactive_since: Option<Instant> = None;

        loop {
            std::thread::sleep(MAINTENANCE_POLL_EVERY);

            let now = Instant::now();
            let Some(win) = app.get_webview_window("main") else {
                inactive_since = None;
                continue;
            };

            if main_window_is_inactive(&win) {
                inactive_since.get_or_insert(now);
            } else {
                inactive_since = None;
            }

            match due_maintenance(now, next_reload, next_restart, inactive_since) {
                Some(MaintenanceAction::Restart) => {
                    if restart_main_webview(&app).is_ok() {
                        let done = Instant::now();
                        next_restart = done + WEBVIEW_RESTART_EVERY;
                        next_reload = done + AUTO_RELOAD_EVERY;
                        inactive_since = Some(done);
                    }
                }
                Some(MaintenanceAction::Reload) => {
                    if win.reload().is_ok() {
                        next_reload = Instant::now() + AUTO_RELOAD_EVERY;
                    }
                }
                None => {}
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::Builder::new().open_js_links_on_click(false).build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_notification::init())
        // Single instance: se já tiver aberto, foca a janela existente
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
            }
        }))
        // Autostart: inicia com o Windows
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .setup(|app| {
            // Limpeza: avatares temporários com mais de 24h ficam órfãos quando o
            // usuário conversa com gente nova e nunca mais com a antiga. Não é
            // crítico (o Windows limpa %TEMP% periodicamente), mas mantém a pasta
            // enxuta. Custo: <50ms no startup.
            cleanup_old_avatar_temp_files();

            use tauri_plugin_autostart::ManagerExt;
            let autostart = app.autolaunch();

            // Verifica se foi iniciado com --hidden (autostart)
            let args: Vec<String> = std::env::args().collect();
            let start_hidden = args.iter().any(|a| a == "--hidden");

            // Carrega o ícone do WhatsApp Lite
            let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))
                .expect("failed to load tray icon");

            // Menu de contexto do tray
            let is_autostart = autostart.is_enabled().unwrap_or(false);
            let show_item = MenuItemBuilder::with_id("show", "Abrir").build(app)?;
            let autostart_item =
                CheckMenuItemBuilder::with_id("autostart", "Iniciar com o sistema")
                    .checked(is_autostart)
                    .build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "Sair").build(app)?;
            let tray_menu = MenuBuilder::new(app)
                .item(&show_item)
                .item(&autostart_item)
                .item(&quit_item)
                .build()?;

            // Tray icon
            let autostart_item_for_menu = autostart_item.clone();
            let _tray = TrayIconBuilder::new()
                .icon(icon)
                .tooltip("WhatsApp Lite")
                .menu(&tray_menu)
                .on_menu_event(move |app, event| {
                    if event.id() == "show" {
                        focus_main_window(app.clone());
                    } else if event.id() == "quit" {
                        app.exit(0);
                    } else if event.id() == "autostart" {
                        use tauri_plugin_autostart::ManagerExt;
                        let autostart = app.autolaunch();
                        let is_enabled = autostart.is_enabled().unwrap_or(false);
                        if is_enabled {
                            let _ = autostart.disable();
                        } else {
                            let _ = autostart.enable();
                        }
                        let _ = autostart_item_for_menu
                            .set_checked(autostart.is_enabled().unwrap_or(false));
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        focus_main_window(tray.app_handle().clone());
                    }
                })
                .build(app)?;

            // Cria a janela principal com guard de navegacao e patch JS em document_start.
            let app_handle = app.handle().clone();
            let main_window = create_main_window(&app_handle, !start_hidden, !start_hidden)?;

            // Devtools opcional: setar WA_LITE_DEVTOOLS=1 no ambiente abre
            // automaticamente. Sem isso, F12 também funciona (a feature
            // `devtools` no Cargo.toml mantém o atalho habilitado em release).
            if std::env::var("WA_LITE_DEVTOOLS").as_deref() == Ok("1") {
                main_window.open_devtools();
            }

            start_webview_maintenance(app_handle);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            open_save_dialog,
            prepare_binary_file,
            append_binary_file,
            open_external_url,
            focus_main_window,
            send_notification
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
