use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItemBuilder, CheckMenuItemBuilder},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, WindowEvent,
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

#[tauri::command]
fn send_notification(
    app: tauri::AppHandle,
    title: String,
    body: String,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use tauri_winrt_notification::{Duration, Toast};

        let app_id = app.config().identifier.clone();
        let app_handle = app.clone();

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

        let toast = Toast::new(&app_id)
            .title(&clean_title)
            .text1("")
            .text2(&clean_body)
            .duration(Duration::Short)
            .on_activated(move |_| {
                if let Some(win) = app_handle.get_webview_window("main") {
                    let _ = win.show();
                    let _ = win.unminimize();
                    let _ = win.set_focus();
                }
                Ok(())
            });

        toast
            .show()
            .map_err(|e| format!("Falha ao enviar notificação: {e}"))?;

        return Ok(());
    }

    #[cfg(not(target_os = "windows"))]
    {
    app.notification()
        .builder()
        .id(1) // ID fixo para substituir sempre a notificação anterior
        .title(&title)
        .body(&body)
        .show()
        .map_err(|e| format!("Falha ao enviar notificação: {e}"))
    }
}

#[tauri::command]
fn is_window_visible(app: tauri::AppHandle) -> Result<bool, String> {
    let win = app.get_webview_window("main")
        .ok_or_else(|| "Janela principal nao encontrada".to_string())?;
    let visible = win.is_visible().unwrap_or(true);
    let focused = win.is_focused().unwrap_or(false);
    let minimized = win.is_minimized().unwrap_or(false);
    // Janela esta "ativa" se esta visivel, nao minimizada, e com foco
    Ok(visible && !minimized && focused)
}

/// JavaScript injetado para:
/// 1. Ctrl+W fecha conversa (Escape)
/// 2. Ctrl+Seta para cima edita a última mensagem enviada
/// 3. Dialogo nativo para salvar downloads e abertura segura de links externos
/// 4. Remocao do banner de instalacao no rodape da sidebar (lista de conversas)
///
/// Escopo de manutenção:
/// - Não adicionar automações de clique direito/context menu no WebView.
/// - O clique direito da tray permanece permitido por design (menu do sistema).
const WHATSAPP_PATCHES_JS: &str = r#"
  (() => {
        if (window.__whatsapp_lite_notif_installed) {
            return;
        }
        window.__whatsapp_lite_notif_installed = true;

    let unreadChats = new Map();
    let debugMode = false;
        let firstScanDone = false;
    
    function logDebug(...args) {
      if (debugMode) console.log('[WA-Tauri]', ...args);
    }

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

        const isWindowActive = async () => {
            try {
                return await tauriInvoke('is_window_visible', {});
            } catch (_) {
                return document.hasFocus();
            }
        };

        const getChatRows = () => {
            const pane = document.getElementById('pane-side');
            if (!pane) {
                return [];
            }
            const grid = pane.querySelector('[role="grid"]') || pane;
            const rows = grid.querySelectorAll('[role="row"]');
            return Array.from(rows);
        };

        const extractContactName = (row) => {
            const titled = row.querySelectorAll('span[title]');
            for (const el of titled) {
                const text = (el.getAttribute('title') || '').trim();
                const low = text.toLowerCase();
                if (!text) continue;
                if (low.startsWith('ic-')) continue;
                if (low.includes('mensagem n\u00e3o lida') || low.includes('mensagens n\u00e3o lidas')) continue;
                if (low.includes('unread')) continue;
                return text;
            }

            const autos = row.querySelectorAll('span[dir="auto"], span[dir="ltr"]');
            for (const el of autos) {
                const text = (el.textContent || '').trim();
                const low = text.toLowerCase();
                if (!text) continue;
                if (low.startsWith('ic-')) continue;
                if (/^\d{1,2}:\d{2}$/.test(text)) continue;
                if (/^\d{1,4}$/.test(text)) continue;
                if (low.includes('conversa favorita') || low.includes('conversa silenciada')) continue;
                return text;
            }

            return '';
        };

        const isMuted = (row) => {
            if (row.querySelector('[data-icon="muted"]')) {
                return true;
            }

            const ariaNodes = row.querySelectorAll('[aria-label]');
            for (const n of ariaNodes) {
                const label = ((n.getAttribute('aria-label') || '') + '').toLowerCase();
                if (!label) continue;
                if (label.includes('conversa silenciada') || label.includes('chat muted') || label.includes('muted')) {
                    return true;
                }
            }

            const iconTitles = row.querySelectorAll('svg title');
            for (const t of iconTitles) {
                const val = ((t.textContent || '') + '').toLowerCase().trim();
                if (!val) continue;
                if (val.includes('notifications-off') || val.includes('muted')) {
                    return true;
                }
            }

            return false;
        };

        const getUnreadCount = (row) => {
            const badges = row.querySelectorAll('span[aria-label]');
            for (const badge of badges) {
                const ariaLabel = (badge.getAttribute('aria-label') || '').toLowerCase();
                if (!ariaLabel) continue;

                const isUnreadLabel =
                    ariaLabel.includes('mensagem n\u00e3o lida') ||
                    ariaLabel.includes('mensagens n\u00e3o lidas') ||
                    ariaLabel.includes('mensagem nao lida') ||
                    ariaLabel.includes('mensagens nao lidas') ||
                    ariaLabel.includes('unread');

                if (!isUnreadLabel) continue;

                const numberText = (badge.textContent || '').trim();
                const numberFromText = numberText.match(/\d+/);
                if (numberFromText) {
                    return parseInt(numberFromText[0], 10);
                }

                const numberFromLabel = ariaLabel.match(/\d+/);
                if (numberFromLabel) {
                    return parseInt(numberFromLabel[0], 10);
                }

                return 1;
            }

            return 0;
        };

        const mediaLabelFromRow = (row) => {
            const iconSpans = row.querySelectorAll('span[data-icon]');
            for (const el of iconSpans) {
                const iconType = (el.getAttribute('data-icon') || '').toLowerCase();
                if (!iconType) continue;
                if (iconType.includes('sticker')) return 'Figurinha';
                if (iconType.includes('video')) return 'Video';
                if (iconType.includes('audio')) return 'Audio';
                if (iconType.includes('camera') || iconType.includes('image') || iconType.includes('photo')) return 'Foto';
                if (iconType.includes('document') || iconType.includes('doc')) return 'Documento';
                if (iconType.includes('gif')) return 'GIF';
            }

            const iconTitles = row.querySelectorAll('svg title');
            for (const title of iconTitles) {
                const text = ((title.textContent || '') + '').toLowerCase();
                if (!text) continue;
                if (text.includes('sticker')) return 'Figurinha';
                if (text.includes('video')) return 'Video';
                if (text.includes('audio') || text.includes('mic')) return 'Audio';
                if (text.includes('camera') || text.includes('image') || text.includes('photo')) return 'Foto';
                if (text.includes('document')) return 'Documento';
                if (text.includes('gif')) return 'GIF';
            }

            return '';
        };

        const extractPreview = (row, contactName) => {
            const mediaLabel = mediaLabelFromRow(row);
            const candidates = row.querySelectorAll('span[dir="auto"], span[dir="ltr"], span[title]');
            let best = '';

            for (const sp of candidates) {
                const raw = (sp.getAttribute('title') || sp.textContent || '').replace(/\s+/g, ' ').trim();
                if (!raw) continue;

                const low = raw.toLowerCase();
                if (low.startsWith('ic-')) continue;
                if (low === 'conversa favorita') continue;
                if (low === 'conversa silenciada') continue;
                if (low.includes('mensagem n\u00e3o lida') || low.includes('mensagens n\u00e3o lidas')) continue;
                if (low.includes('mensagem nao lida') || low.includes('mensagens nao lidas')) continue;
                if (low.includes('unread')) continue;
                if (/^\d{1,2}:\d{2}$/.test(raw)) continue;
                if (/^\d{1,4}$/.test(raw)) continue;
                if (contactName && raw === contactName) continue;
                if (raw === ':') continue;

                best = raw;
            }

            let preview = best || mediaLabel || 'Nova mensagem';
            if (preview.length > 180) {
                preview = preview.slice(0, 180) + '...';
            }
            return preview;
        };

        const scanUnreadChats = async () => {
            const chatRows = getChatRows();
            const currentUnreadChats = new Set();
            const windowActive = await isWindowActive();

            for (const row of chatRows) {
                const contactName = extractContactName(row);
                if (!contactName) {
                    continue;
                }

                const muted = isMuted(row);
                const unreadCount = getUnreadCount(row);

                logDebug('Scan result:', contactName, 'Muted:', muted, 'Unread:', unreadCount);

                if (unreadCount <= 0) {
                    unreadChats.delete(contactName);
                    continue;
                }

                currentUnreadChats.add(contactName);
                const previousCount = unreadChats.get(contactName) || 0;

                // Regra de ouro #2: silenciado nunca notifica.
                if (muted) {
                    unreadChats.set(contactName, unreadCount);
                    continue;
                }

                // Regra de ouro #1: so notifica quando aumenta contador de nao lidas.
                if (!firstScanDone && unreadCount >= 0) {
                    unreadChats.set(contactName, unreadCount);
                    continue;
                }

                if (unreadCount > previousCount && !windowActive) {
                    const previewText = extractPreview(row, contactName);
                    logDebug('New unread message from', contactName, 'Count:', unreadCount, 'Preview:', previewText);

                    tauriInvoke('send_notification', {
                        title: contactName,
                        body: previewText
                    }).catch(console.error);
                }

                unreadChats.set(contactName, unreadCount);
            }

            for (const key of Array.from(unreadChats.keys())) {
                if (!currentUnreadChats.has(key)) {
                    unreadChats.delete(key);
                }
            }

            firstScanDone = true;
        };

    // Observer setup
    logDebug('Initializing observer...');
    let scanTimer = null;
    const observer = new MutationObserver((mutations) => {
        let shouldScan = false;
        
        for (const mutation of mutations) {
            // Looking for changes in the chat list or elements inside list items
            if (mutation.target && mutation.target.closest && mutation.target.closest('#pane-side')) {
                shouldScan = true;
                break;
            }
            if (mutation.target.id === 'pane-side') {
                shouldScan = true;
                break;
            }
        }

        if (shouldScan) {
                        if (scanTimer) {
                            clearTimeout(scanTimer);
                        }
                        scanTimer = setTimeout(() => {
                            scanUnreadChats().catch(console.error);
                        }, 250);
        }
    });

    // Start observing when the chat list is available
    const startObserving = () => {
        const chatListContainer = document.getElementById('pane-side');
        if (chatListContainer) {
            logDebug('Found pane-side, starting observer');
            observer.observe(chatListContainer, {
                childList: true,
                subtree: true,
                characterData: true,
                attributes: true,
                attributeFilter: ['aria-label', 'title', 'data-icon']
            });
            // Initial scan
            scanUnreadChats().catch(console.error);
            setInterval(() => {
              scanUnreadChats().catch(console.error);
            }, 2500);
        } else {
            setTimeout(startObserving, 1000);
        }
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

    // Start the process once DOM is ready
    if (document.readyState === 'loading') {
        document.addEventListener('DOMContentLoaded', startObserving);
    } else {
        startObserving();
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

            // Se iniciou com --hidden, esconde a janela (fica só na tray)
            let main_window = app.get_webview_window("main").unwrap();
            if start_hidden {
                let _ = main_window.hide();
            }

            // Injeta patches JS
            let patches_js = WHATSAPP_PATCHES_JS.to_string();
            
            let win_for_patches = main_window.clone();
            std::thread::spawn(move || {
                // Injeta repetidamente no início para garantir que o script seja executado
                // assim que o documento HTML estiver minimamente preparado, evitando delay
                for _ in 0..5 {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    let _ = win_for_patches.eval(&patches_js);
                }
                
                // Depois mantém injetando ocasionalmente em caso de reload total do site
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(10));
                    let _ = win_for_patches.eval(&patches_js);
                }
            });

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
            send_notification,
            is_window_visible
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

