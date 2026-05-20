use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder, CheckMenuItemBuilder},
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
};

#[tauri::command]
async fn open_save_dialog(app: tauri::AppHandle, suggested_name: Option<String>) -> Result<Option<String>, String> {
    let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();

    let mut dialog = app.dialog().file();
    if let Some(name) = suggested_name.as_deref() {
        dialog = dialog.set_file_name(name);
    }

    dialog.save_file(move |path| {
        let selected = path.map(|p| p.to_string());
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

            document.addEventListener('keydown', (e) => {
                // Ctrl+W -> fecha conversa (equivalente a Escape no WhatsApp Web)
                if (e.ctrlKey && !e.shiftKey && !e.altKey && (e.key === 'w' || e.key === 'W')) {
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

                    const input = document.querySelector('div[contenteditable="true"][data-tab="10"]')
                        || document.querySelector('footer div[contenteditable="true"]')
                        || document.querySelector('div[title="Digite uma mensagem"]')
                        || document.activeElement;

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

  })();
"#;

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
            // Log imediato para confirmar que setup() esta sendo executado
            let _ = std::fs::write(
                r"C:\Users\Levi\Desktop\whatsapp_lite_debug.log",
                "setup() iniciou\n"
            );

            // Habilita o autostart na primeira execução
            use tauri_plugin_autostart::ManagerExt;
            let autostart = app.autolaunch();
            let _ = autostart.enable();

            // Verifica se foi iniciado com --hidden (autostart)
            let args: Vec<String> = std::env::args().collect();
            let start_hidden = args.iter().any(|a| a == "--hidden");

            // Carrega o ícone do WhatsApp Lite
            let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))
                .expect("failed to load tray icon");

            // Menu de contexto do tray
            let is_autostart = autostart.is_enabled().unwrap_or(false);
            let autostart_item = CheckMenuItemBuilder::with_id("autostart", "Iniciar com o sistema")
                .checked(is_autostart)
                .build(app)?;
            let quit_item = MenuItemBuilder::with_id("quit", "Sair").build(app)?;
            let tray_menu = MenuBuilder::new(app)
                .item(&autostart_item)
                .item(&quit_item)
                .build()?;

            // Tray icon
            let _tray = TrayIconBuilder::new()
                .icon(icon)
                .tooltip("WhatsApp Lite")
                .menu(&tray_menu)
                .on_menu_event(|app, event| {
                    if event.id() == "quit" {
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
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(win) = app.get_webview_window("main") {
                            let _ = win.show();
                            let _ = win.unminimize();
                            let _ = win.set_focus();
                        }
                    }
                })
                .build(app)?;

            // Cria a janela principal com guard de navegação:
            // qualquer navegação para fora de *.whatsapp.com / *.whatsapp.net
            // é bloqueada e a URL é aberta no navegador do sistema. Isso impede
            // que o frame do WhatsApp Web seja redirecionado para facebook.com,
            // accountscenter.facebook.com etc. via navegação programática do
            // próprio JS do WhatsApp Web (que o click handler em <a> não pega).
            let nav_app_handle = app.handle().clone();
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
            .disable_drag_drop_handler()
            // initialization_script roda em document_start, antes de qualquer
            // script do WhatsApp Web. Isso garante que o proxy do Notification
            // já esteja instalado quando o bundle deles consultar permission
            // ou chamar `new Notification(...)`. Substitui a antiga thread que
            // ficava injetando via eval em loop.
            .initialization_script(WHATSAPP_PATCHES_JS)
            .on_navigation(move |url| {
                let host = url.host_str().unwrap_or("");
                let allowed = host == "web.whatsapp.com"
                    || host == "whatsapp.com"
                    || host.ends_with(".whatsapp.com")
                    || host.ends_with(".whatsapp.net");
                if !allowed {
                    let url_str = url.to_string();
                    let _ = nav_app_handle.opener().open_url(&url_str, None::<&str>);
                }
                allowed
            })
            .build()?;

            // Se iniciou com --hidden, esconde a janela (fica só na tray)
            if start_hidden {
                let _ = main_window.hide();
            }

            // Minimiza para tray ao invés de fechar
            let win_handle = main_window.app_handle().clone();
            main_window.on_window_event(move |event| {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    if let Some(win) = win_handle.get_webview_window("main") {
                        let _ = win.hide();
                    }
                }
            });

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

