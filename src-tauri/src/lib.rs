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

mod profiles;

// Passado ao processo sucessor num relaunch: o PID que ele precisa esperar
// morrer antes de subir.
const RELAUNCH_ARG: &str = "--relaunch-after=";

// Nunca recarrega mais cedo que isto desde o último reload — evita churn de
// re-sync/rede mesmo se a memória disparar logo depois de recarregar.
const RELOAD_MIN_INTERVAL: Duration = Duration::from_secs(30 * 60);
// Teto de segurança: recarrega mesmo sem pressão de memória (ou quando a engine
// não reporta heap, ex.: WebKitGTK no Linux) depois deste tempo aberto.
const RELOAD_MAX_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
// Heap JS reportado pela página (via `performance.memory`) acima disto conta
// como pressão de memória e agenda um reload assim que o usuário ficar ocioso.
// WhatsApp Web idle fica em ~200-350 MB e sobe com o vazamento; 500 é folgado.
const HEAP_RELOAD_THRESHOLD_MB: u64 = 500;
// Escondida na bandeja / minimizada: ninguém está olhando, 3 min bastam.
const MAINTENANCE_IDLE_HIDDEN_FOR: Duration = Duration::from_secs(3 * 60);
// Visível mas sem foco (ex.: segundo monitor): o usuário pode estar lendo,
// então exige uma ausência bem mais longa antes de recarregar na frente dele.
const MAINTENANCE_IDLE_UNFOCUSED_FOR: Duration = Duration::from_secs(20 * 60);
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

/// Estado compartilhado entre o JS injetado e a thread de manutenção.
struct RuntimeState {
    /// `true` enquanto o WhatsApp Web tem uma chamada de voz/vídeo em curso
    /// (reportado pelo hook de RTCPeerConnection no JS). A manutenção nunca
    /// recarrega durante uma chamada — derrubaria a ligação.
    call_active: std::sync::atomic::AtomicBool,
    /// Último tamanho do heap JS reportado pela página, em MB. `performance.memory`
    /// só existe no Chromium/WebView2; sem ele fica 0 e o reload cai no teto de tempo.
    heap_mb: std::sync::atomic::AtomicU64,
}

impl RuntimeState {
    fn new() -> Self {
        Self {
            call_active: std::sync::atomic::AtomicBool::new(false),
            heap_mb: std::sync::atomic::AtomicU64::new(0),
        }
    }
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

/// Aceita o corpo bruto normal e o fallback JSON do Tauri, usado quando a CSP
/// do WhatsApp bloqueia o protocolo IPC interno.
fn download_bytes(body: &tauri::ipc::InvokeBody) -> Result<std::borrow::Cow<'_, [u8]>, String> {
    match body {
        tauri::ipc::InvokeBody::Raw(bytes) => Ok(std::borrow::Cow::Borrowed(bytes)),
        tauri::ipc::InvokeBody::Json(serde_json::Value::Array(bytes)) => bytes
            .iter()
            .map(|byte| byte.as_u64().and_then(|byte| u8::try_from(byte).ok()))
            .collect::<Option<Vec<_>>>()
            .map(std::borrow::Cow::Owned)
            .ok_or_else(|| "Corpo da requisicao nao contem bytes validos".to_string()),
        _ => Err("Corpo da requisicao nao e binario".to_string()),
    }
}

/// Anexa um chunk de download ao arquivo.
#[tauri::command]
fn append_binary_file(request: tauri::ipc::Request<'_>) -> Result<(), String> {
    use base64::Engine as _;

    let encoded = request
        .headers()
        .get("x-wa-path")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "Header x-wa-path ausente".to_string())?;
    let path_bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("Header x-wa-path invalido: {e}"))?;
    let path = String::from_utf8(path_bytes)
        .map_err(|e| format!("Caminho de destino invalido: {e}"))?;

    let bytes = download_bytes(request.body())?;

    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .map_err(|e| format!("Falha ao abrir arquivo para append: {e}"))?;

    file.write_all(&bytes)
        .map_err(|e| format!("Falha ao escrever bytes no arquivo: {e}"))
}

#[cfg(test)]
#[test]
fn download_bytes_accepts_raw_and_json() {
    use tauri::ipc::InvokeBody;

    assert_eq!(
        download_bytes(&InvokeBody::Raw(vec![1, 2]))
            .unwrap()
            .as_ref(),
        [1, 2]
    );
    assert_eq!(
        download_bytes(&InvokeBody::Json(serde_json::json!([3, 4])))
            .unwrap()
            .as_ref(),
        [3, 4]
    );
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
fn set_call_active(state: tauri::State<'_, RuntimeState>, active: bool) {
    state
        .call_active
        .store(active, std::sync::atomic::Ordering::Relaxed);
}

#[tauri::command]
fn report_heap_mb(state: tauri::State<'_, RuntimeState>, mb: u64) {
    state.heap_mb.store(mb, std::sync::atomic::Ordering::Relaxed);
}

/// Mesma checagem usada por `send_notification` pra pular o toast — exposta ao
/// JS pra que o proprio WhatsApp Web possa silenciar o som de notificacao que
/// ele toca por conta propria (independente da Notification API, que ja
/// suprimimos no toast nativo).
#[tauri::command]
fn is_do_not_disturb() -> bool {
    #[cfg(target_os = "windows")]
    {
        windows_do_not_disturb_enabled()
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
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

    // Passou do intervalo mínimo desde o último reload (pré-condição comum).
    fn past_min(now: Instant) -> Instant {
        now - RELOAD_MIN_INTERVAL
    }

    #[test]
    fn reload_waits_for_hidden_grace() {
        let now = Instant::now();
        let last = past_min(now);
        let high_heap = HEAP_RELOAD_THRESHOLD_MB;
        // Escondida há pouco (179s < 3min): ainda não.
        assert!(!due_reload(
            now,
            last,
            high_heap,
            Some(now - Duration::from_secs(179)),
            None,
            false,
        ));
        // Escondida tempo suficiente + heap alto: recarrega.
        assert!(due_reload(
            now,
            last,
            high_heap,
            Some(now - MAINTENANCE_IDLE_HIDDEN_FOR),
            None,
            false,
        ));
    }

    #[test]
    fn reload_needs_longer_grace_when_visible_but_unfocused() {
        let now = Instant::now();
        let last = past_min(now);
        let high_heap = HEAP_RELOAD_THRESHOLD_MB;
        // Sem foco há só 3min (grace de escondida): insuficiente pra janela visível.
        assert!(!due_reload(
            now,
            last,
            high_heap,
            None,
            Some(now - MAINTENANCE_IDLE_HIDDEN_FOR),
            false,
        ));
        // Sem foco há 20min: recarrega.
        assert!(due_reload(
            now,
            last,
            high_heap,
            None,
            Some(now - MAINTENANCE_IDLE_UNFOCUSED_FOR),
            false,
        ));
    }

    #[test]
    fn reload_never_runs_during_call() {
        let now = Instant::now();
        assert!(!due_reload(
            now,
            now - RELOAD_MAX_INTERVAL,
            HEAP_RELOAD_THRESHOLD_MB,
            Some(now - MAINTENANCE_IDLE_HIDDEN_FOR),
            Some(now - MAINTENANCE_IDLE_UNFOCUSED_FOR),
            true,
        ));
    }

    #[test]
    fn reload_respects_min_interval() {
        let now = Instant::now();
        // Recarregou agora há pouco: nem heap alto força reload cedo demais.
        assert!(!due_reload(
            now,
            now - Duration::from_secs(60),
            HEAP_RELOAD_THRESHOLD_MB + 1000,
            Some(now - MAINTENANCE_IDLE_HIDDEN_FOR),
            None,
            false,
        ));
    }

    #[test]
    fn reload_skips_when_memory_ok_before_ceiling() {
        let now = Instant::now();
        // Passou do mínimo, ocioso, mas heap baixo e ainda longe do teto: não recarrega.
        assert!(!due_reload(
            now,
            past_min(now),
            HEAP_RELOAD_THRESHOLD_MB - 1,
            Some(now - MAINTENANCE_IDLE_HIDDEN_FOR),
            None,
            false,
        ));
    }

    #[test]
    fn reload_ceiling_fires_without_memory_signal() {
        let now = Instant::now();
        // Heap 0 (engine sem performance.memory) mas passou do teto de tempo: recarrega.
        assert!(due_reload(
            now,
            now - RELOAD_MAX_INTERVAL,
            0,
            Some(now - MAINTENANCE_IDLE_HIDDEN_FOR),
            None,
            false,
        ));
    }

    #[test]
    fn reload_needs_idle_even_under_memory_pressure() {
        let now = Instant::now();
        // Heap altíssimo, passou do mínimo, mas usuário ativo (nada ocioso): não recarrega.
        assert!(!due_reload(
            now,
            past_min(now),
            HEAP_RELOAD_THRESHOLD_MB + 2000,
            None,
            None,
            false,
        ));
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

        // Invoca passando um ArrayBuffer como corpo BRUTO da IPC (sem virar array
        // JSON). Usado no download pra transferir os bytes sem serializar milhões
        // de números por MB. `headers` leva metadados (ex.: caminho do arquivo).
        const tauriInvokeRaw = async (cmd, buffer, headers) => {
            const internals = window.__TAURI_INTERNALS__;
            const opts = headers ? { headers } : undefined;
            if (internals && typeof internals.invoke === 'function') {
                return internals.invoke(cmd, buffer, opts);
            }
            if (window.__TAURI__ && window.__TAURI__.core && typeof window.__TAURI__.core.invoke === 'function') {
                return window.__TAURI__.core.invoke(cmd, buffer, opts);
            }
            throw new Error('Tauri invoke indisponivel');
        };

        // Header HTTP só aceita ASCII; o caminho pode ter acentos. Base64 de
        // UTF-8 (o Rust decodifica de volta pra String).
        const __waPathToHeader = (p) => {
            const utf8 = new TextEncoder().encode(String(p));
            let bin = '';
            for (let i = 0; i < utf8.length; i++) bin += String.fromCharCode(utf8[i]);
            return btoa(bin);
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

        // === Guarda de chamada ativa ===
        // A manutenção no Rust recarrega a página quando o app fica muito tempo
        // aberto (o WhatsApp Web vaza memória e começa a travar). Recarregar no
        // meio de uma chamada de voz/vídeo derrubaria a ligação, então hookamos
        // o RTCPeerConnection: enquanto houver conexão WebRTC viva, avisamos o
        // Rust para segurar o reload.
        if (!window.__whatsapp_lite_call_guard_installed) {
            window.__whatsapp_lite_call_guard_installed = true;

            let __waActiveCalls = 0;
            const __waReportCalls = () => {
                // Exposto pro mute de DND: nunca silenciar áudio durante chamada.
                window.__waLiteCallActive = __waActiveCalls > 0;
                tauriInvoke('set_call_active', { active: __waActiveCalls > 0 }).catch(() => {});
            };
            // Cada load zera o estado no Rust — se a página morreu no meio de
            // uma chamada, o "true" antigo não pode travar a manutenção pra sempre.
            __waReportCalls();

            const __waOrigRTC = window.RTCPeerConnection;
            if (typeof __waOrigRTC === 'function') {
                const PatchedRTC = function(...args) {
                    const pc = new __waOrigRTC(...args);
                    __waActiveCalls++;
                    __waReportCalls();

                    let released = false;
                    const release = () => {
                        if (released) return;
                        released = true;
                        __waActiveCalls = Math.max(0, __waActiveCalls - 1);
                        __waReportCalls();
                    };

                    pc.addEventListener('connectionstatechange', () => {
                        if (pc.connectionState === 'closed' || pc.connectionState === 'failed') {
                            release();
                        }
                    });

                    const origClose = pc.close.bind(pc);
                    pc.close = function() {
                        release();
                        return origClose();
                    };

                    return pc;
                };
                PatchedRTC.prototype = __waOrigRTC.prototype;
                try { Object.setPrototypeOf(PatchedRTC, __waOrigRTC); } catch (_) {}
                window.RTCPeerConnection = PatchedRTC;
            }
        }

        // === Reporte de heap JS pro Rust ===
        // A manutenção decide recarregar por PRESSÃO DE MEMÓRIA (o vazamento
        // real do WhatsApp Web), não só pelo relógio. `performance.memory` é do
        // Chromium/WebView2; onde não existe (ex.: WebKitGTK), ficamos sem sinal
        // e a manutenção cai no teto de tempo. Reporta a cada 30s; cada reload
        // reinicia esta página, então o valor volta a refletir o heap limpo.
        if (!window.__whatsapp_lite_heap_report_installed
            && window.performance && performance.memory) {
            window.__whatsapp_lite_heap_report_installed = true;
            const __waReportHeap = () => {
                try {
                    const used = performance.memory.usedJSHeapSize || 0;
                    tauriInvoke('report_heap_mb', {
                        mb: Math.round(used / (1024 * 1024))
                    }).catch(() => {});
                } catch (_) {}
            };
            __waReportHeap();
            setInterval(__waReportHeap, 30000);
        }

        // === Silenciar som de notificação durante Não Perturbe (Windows) ===
        // O toast nativo já é suprimido no Rust (send_notification checa o Focus
        // Assist antes de mostrar), mas o "tin-ting" de nova mensagem é um som
        // que a PRÓPRIA página toca, à parte da Notification API — continua
        // saindo com o toast suprimido. WhatsApp Web pode tocar isso por dois
        // caminhos, então cobrimos os dois:
        //   1) HTMLMediaElement.play  (<audio> / new Audio())
        //   2) Web Audio API          (AudioBufferSource/Oscillator.start)
        //
        // Só bloqueamos efeitos sonoros AUTOMÁTICOS: DND ligado e sem chamada
        // em curso. Proteções contra silenciar áudio legítimo:
        //   - `srcObject` presente => é MediaStream (áudio de chamada / stream
        //     ao vivo), nunca um chime => nunca silencia;
        //   - `loop` => ringtone / áudio de fundo => nunca silencia;
        //   - `src` blob: => mídia de conversa (nota de voz, vídeo, áudio),
        //     que o WhatsApp Web descriptografa na página e serve por
        //     createObjectURL => nunca silencia. O chime, ao contrário, é um
        //     asset estático (https:/data:) — é ESSE que bloqueamos.
        //   - Web Audio: só mexemos em BufferSource/Oscillator (efeitos), que
        //     não são o caminho de streams de chamada (MediaStreamAudioSource).
        //
        // Nota: as versões anteriores usavam "gesto recente do usuário" (janela
        // de 5s via navigator.userActivation) + allow-list permanente de
        // elementos tocados com gesto. Isso vazava o chime exatamente no caso
        // comum: usuário digitando quando a mensagem chega => cada tecla renova
        // a ativação => chime passa; e como o WhatsApp reutiliza o mesmo
        // <audio> pro chime, uma única passada dessas o aprovava pra sempre.
        // O discriminador por blob: é estrutural e não depende de timing.
        if (!window.__whatsapp_lite_dnd_mute_installed) {
            window.__whatsapp_lite_dnd_mute_installed = true;

            let __waDndActive = false;
            const __waRefreshDnd = () => {
                tauriInvoke('is_do_not_disturb', {})
                    .then((active) => { __waDndActive = !!active; })
                    .catch(() => {});
            };
            __waRefreshDnd();
            setInterval(__waRefreshDnd, 5000);

            // --- Caminho 1: HTMLMediaElement --------------------------------
            const __waShouldMuteMedia = (el) => {
                if (!__waDndActive || window.__waLiteCallActive) return false;
                if (el.srcObject || el.loop || el.muted) return false;
                const src = String(el.currentSrc || el.src || '');
                return !src.startsWith('blob:');
            };

            const __waOrigPlay = HTMLMediaElement.prototype.play;
            HTMLMediaElement.prototype.play = function(...args) {
                if (__waShouldMuteMedia(this)) {
                    return Promise.resolve();
                }
                return __waOrigPlay.apply(this, args);
            };

            // Rede extra: cobre playback que começa sem passar pelo play() em
            // JS (ex.: atributo autoplay) em elementos anexados ao DOM. O
            // evento 'play' não borbulha, mas captura no document alcança.
            document.addEventListener('play', (ev) => {
                const el = ev.target;
                if (el instanceof HTMLMediaElement && __waShouldMuteMedia(el)) {
                    try { el.pause(); } catch (_) {}
                }
            }, true);

            // --- Caminho 2: Web Audio ---------------------------------------
            // Aqui não existe src pra discriminar, então usamos gesto direto:
            // efeito disparado por clique/tecla toca; efeito espontâneo (chime)
            // não. Janela de 1s — curta de propósito: o start() de um efeito
            // user-initiated acontece no mesmo tick do evento, enquanto um
            // chime só coincide com um gesto por azar. (A janela antiga de 5s,
            // renovada a cada tecla, deixava o chime passar sempre que o
            // usuário estava digitando.)
            let __waLastGestureAt = 0;
            ['pointerdown', 'mousedown', 'keydown', 'touchstart'].forEach((ev) => {
                window.addEventListener(ev, () => { __waLastGestureAt = Date.now(); }, true);
            });
            const __waHasDirectGesture = () => (Date.now() - __waLastGestureAt) < 1000;

            // Em vez de bloquear start() (quebraria start/stop/onended),
            // desconectamos o nó da saída: ele roda em silêncio mas todo o
            // maquinário de estado continua normal. Também neutralizamos o
            // connect() DESTE nó, senão um `source.start(); source.connect(dest)`
            // (conectar depois de agendar) reconectaria o som bloqueado.
            const __waPatchScheduledStart = (Ctor) => {
                if (!Ctor || !Ctor.prototype || typeof Ctor.prototype.start !== 'function') {
                    return;
                }
                const orig = Ctor.prototype.start;
                Ctor.prototype.start = function(...args) {
                    if (__waDndActive && !window.__waLiteCallActive && !__waHasDirectGesture()) {
                        try { this.disconnect(); } catch (_) {}
                        try { this.connect = function(dest) { return dest; }; } catch (_) {}
                    }
                    return orig.apply(this, args);
                };
            };
            __waPatchScheduledStart(window.AudioBufferSourceNode);
            __waPatchScheduledStart(window.OscillatorNode);
        }

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

                const pathHeader = __waPathToHeader(selectedPath);
                const appendChunk = (view) => {
                    // Manda o ArrayBuffer exato do chunk (sem cópia quando a view
                    // já cobre o buffer inteiro).
                    const ab = (view.byteOffset === 0 && view.byteLength === view.buffer.byteLength)
                        ? view.buffer
                        : view.buffer.slice(view.byteOffset, view.byteOffset + view.byteLength);
                    return tauriInvokeRaw('append_binary_file', ab, { 'x-wa-path': pathHeader });
                };

                if (response.body && response.body.getReader) {
                    const reader = response.body.getReader();
                    while (true) {
                        const part = await reader.read();
                        if (part.done) break;
                        if (part.value && part.value.length) {
                            await appendChunk(part.value);
                        }
                    }
                } else {
                    const buffer = await response.arrayBuffer();
                    await appendChunk(new Uint8Array(buffer));
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

        // O WhatsApp Web muta o DOM continuamente (typing, timestamps, lista de
        // conversas). O observer antigo rodava __waFindBannerCard() — um
        // querySelectorAll do documento inteiro + regex por nó — a CADA mutação,
        // queimando CPU o tempo todo. Agora:
        //  1) pré-filtro barato: só reagimos quando alguma mutação de fato
        //     adiciona um nó cujo texto casa com o padrão do banner;
        //  2) coalescência: agenda no máximo um scan por período ocioso, fora do
        //     caminho de pintura (requestIdleCallback).
        // Trocamos a substituição "antes de pintar" por um flash raríssimo do
        // card — troca barata pelo ganho contínuo de CPU.
        let __waScanScheduled = false;
        const __waRunScan = () => {
            __waScanScheduled = false;
            try { __waReplaceBanner(); } catch (e) {}
        };
        const __waScheduleScan = () => {
            if (__waScanScheduled) return;
            __waScanScheduled = true;
            if (typeof window.requestIdleCallback === 'function') {
                window.requestIdleCallback(__waRunScan, { timeout: 1500 });
            } else {
                setTimeout(__waRunScan, 400);
            }
        };
        const __waMutationsHaveCandidate = (mutations) => {
            for (const m of mutations) {
                const added = m.addedNodes;
                for (let i = 0; i < added.length; i++) {
                    const node = added[i];
                    if (!node || node.nodeType !== 1) continue; // só elementos
                    const t = node.textContent;
                    if (!t || t.length < 5) continue;
                    if (__waTitleRe.test(t) || __waButtonRe.test(t)) return true;
                }
            }
            return false;
        };

        new MutationObserver((mutations) => {
            if (__waMutationsHaveCandidate(mutations)) {
                __waScheduleScan();
            }
        }).observe(
            document.documentElement || document,
            { childList: true, subtree: true }
        );
        try { __waReplaceBanner(); } catch (e) {}

  })();
"#;

fn due_reload(
    now: Instant,
    last_reload: Instant,
    heap_mb: u64,
    hidden_since: Option<Instant>,
    unfocused_since: Option<Instant>,
    call_active: bool,
) -> bool {
    if call_active {
        return false;
    }

    let since_reload = now.duration_since(last_reload);
    if since_reload < RELOAD_MIN_INTERVAL {
        return false;
    }

    // Recarrega por pressão de memória (sinal real do vazamento) OU pelo teto de
    // tempo (rede de segurança). Sem nenhum dos dois, não há motivo pra recarregar.
    let memory_pressure = heap_mb >= HEAP_RELOAD_THRESHOLD_MB;
    let ceiling_hit = since_reload >= RELOAD_MAX_INTERVAL;
    if !memory_pressure && !ceiling_hit {
        return false;
    }

    // ...mas só quando o usuário não está olhando.
    let hidden_long_enough = hidden_since
        .map(|since| now.duration_since(since) >= MAINTENANCE_IDLE_HIDDEN_FOR)
        .unwrap_or(false);
    let unfocused_long_enough = unfocused_since
        .map(|since| now.duration_since(since) >= MAINTENANCE_IDLE_UNFOCUSED_FOR)
        .unwrap_or(false);

    hidden_long_enough || unfocused_long_enough
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
    // Com perfis em uso o nome vai no título: são duas sessões diferentes e
    // digitar na janela errada é fácil demais sem essa dica.
    let title = match profiles::managed_active_name(app) {
        Some(name) => format!("WhatsAppLite — {name}"),
        None => "WhatsAppLite".to_string(),
    };

    let mut builder = WebviewWindowBuilder::new(
        app,
        "main",
        WebviewUrl::External("https://web.whatsapp.com".parse().unwrap()),
    )
    .title(&title)
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
    .on_navigation(move |url| allow_whatsapp_navigation(nav_app_handle.clone(), url));

    // Sem perfis, `None`: o Tauri usa a pasta padrão dele e nada muda de lugar
    // pra quem nunca tocou no recurso.
    if let Some(dir) = profiles::managed_data_dir(app) {
        builder = builder.data_directory(dir);
    }

    let main_window = builder.build()?;

    install_close_to_tray(&main_window);
    Ok(main_window)
}

/// Abre o diálogo de "Adicionar perfil", ou foca o que já estiver aberto.
fn open_profiles_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("profiles") {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }

    let mut builder =
        WebviewWindowBuilder::new(app, "profiles", WebviewUrl::App("profiles.html".into()))
            .title("Adicionar perfil — WhatsApp Lite")
            .inner_size(420.0, 440.0)
            .resizable(false)
            .center();

    // Mesma pasta da janela principal de propósito: dois ambientes WebView2 com
    // pastas diferentes no mesmo processo é território de bug, e este diálogo
    // não guarda estado nenhum.
    if let Some(dir) = profiles::managed_data_dir(app) {
        builder = builder.data_directory(dir);
    }

    let _ = builder.build();
}

/// Reinicia o app no perfil que estiver gravado como ativo.
///
/// Não dá pra usar `app.restart()`: o processo novo subiria enquanto este ainda
/// segura o mutex do single-instance — e morreria achando que já existe uma
/// instância aberta. Além disso a pasta da WebView continua travada até este
/// processo morrer, e é ela que a adoção de perfil precisa mover. Por isso o
/// sucessor recebe o PID atual e espera.
fn relaunch(app: &tauri::AppHandle) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("executável atual: {err}"))?;
    std::process::Command::new(exe)
        .arg(format!("{RELAUNCH_ARG}{}", std::process::id()))
        .spawn()
        .map_err(|err| format!("não foi possível reiniciar: {err}"))?;
    app.exit(0);
    Ok(())
}

/// Espera o processo anterior morrer antes de seguir com o boot. Roda antes de
/// o Tauri subir, porque o single-instance registra o mutex durante o build.
fn wait_for_relaunch_predecessor() {
    let pid = std::env::args().find_map(|arg| {
        arg.strip_prefix(RELAUNCH_ARG)
            .and_then(|value| value.parse::<u32>().ok())
    });
    if let Some(pid) = pid {
        wait_for_pid_exit(pid);
    }
}

#[cfg(target_os = "windows")]
fn wait_for_pid_exit(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };

    unsafe {
        // Erro aqui quer dizer que o processo já morreu — que é justamente o que
        // estávamos esperando.
        let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return;
        };
        // Teto de 10s: se o anterior travar, é melhor tentar subir mesmo assim do
        // que deixar o usuário sem app.
        let _ = WaitForSingleObject(handle, 10_000);
        let _ = CloseHandle(handle);
    }
}

#[cfg(not(target_os = "windows"))]
fn wait_for_pid_exit(_pid: u32) {
    std::thread::sleep(Duration::from_millis(1500));
}

/// Informa ao diálogo se ele precisa pedir também o nome da sessão atual — o que
/// só acontece na primeira vez, quando ainda não existe perfil nenhum.
#[tauri::command]
fn profiles_info(app: tauri::AppHandle) -> serde_json::Value {
    let store = profiles::load(&app);
    serde_json::json!({ "needs_current_name": store.profiles.is_empty() })
}

#[tauri::command]
fn profiles_add(
    app: tauri::AppHandle,
    current_name: Option<String>,
    new_name: String,
) -> Result<(), String> {
    profiles::add(&app, current_name, new_name)?;
    relaunch(&app)
}

#[tauri::command]
fn profiles_cancel(app: tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("profiles") {
        let _ = win.close();
    }
}

// ============================================================================
// Gerenciamento de energia / footprint do WebView2 (Windows).
//
// Os flags anti-throttling (WEBVIEW2_BROWSER_ARGS) fazem o WhatsApp Web rodar a
// todo vapor mesmo escondido — ótimo pra abrir instantâneo, ruim pra dividir CPU
// com outros apps pesados. Estas funções compensam isso: enquanto a janela está
// escondida na bandeja, pedimos ao WebView2 pra devolver memória e marcamos os
// processos dele como "modo eficiência" (EcoQoS) pro scheduler do Windows. Ao
// mostrar, revertemos. Tudo best-effort: qualquer falha é ignorada.
// ============================================================================

#[cfg(target_os = "windows")]
fn apply_low_power_mode(win: &tauri::WebviewWindow, low: bool) {
    let _ = win.with_webview(move |webview| unsafe {
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            ICoreWebView2_19, COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW,
            COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL,
        };
        use windows::core::Interface;

        let core = match webview.controller().CoreWebView2() {
            Ok(c) => c,
            Err(_) => return,
        };

        if let Ok(wv19) = core.cast::<ICoreWebView2_19>() {
            let level = if low {
                COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_LOW
            } else {
                COREWEBVIEW2_MEMORY_USAGE_TARGET_LEVEL_NORMAL
            };
            let _ = wv19.SetMemoryUsageTargetLevel(level);
        }

        let mut browser_pid: u32 = 0;
        if core.BrowserProcessId(&mut browser_pid).is_ok() && browser_pid != 0 {
            set_ecoqos_for_tree(browser_pid, low);
        }
    });
}

/// Aplica (ou remove) EcoQoS no processo browser do WebView2 e em todos os
/// filhos dele (renderers, GPU, utility) — que são quem de fato consome CPU.
#[cfg(target_os = "windows")]
fn set_ecoqos_for_tree(browser_pid: u32, eco: bool) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        set_process_ecoqos(browser_pid, eco);

        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };

        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                if entry.th32ParentProcessID == browser_pid {
                    set_process_ecoqos(entry.th32ProcessID, eco);
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }

        let _ = CloseHandle(snap);
    }
}

#[cfg(target_os = "windows")]
unsafe fn set_process_ecoqos(pid: u32, eco: bool) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_STATE, PROCESS_SET_INFORMATION,
    };

    let handle = match OpenProcess(PROCESS_SET_INFORMATION, false, pid) {
        Ok(h) => h,
        Err(_) => return,
    };

    // ControlMask liga o controle de EXECUTION_SPEED; StateMask=liga/desliga.
    // eco=false com ControlMask setado força o modo de alta performance (tira
    // qualquer throttling), revertendo o EcoQoS quando a janela volta.
    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: if eco {
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED
        } else {
            0
        },
    };

    let _ = SetProcessInformation(
        handle,
        ProcessPowerThrottling,
        &state as *const _ as *const core::ffi::c_void,
        std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
    );
    let _ = CloseHandle(handle);
}

/// Limpa APENAS o cache de disco do perfil do WebView2 (mídia acumulada de
/// conversas). Não toca em cookies/IndexedDB/localStorage — a sessão do
/// WhatsApp permanece logada. Assíncrono; o handler de conclusão é no-op.
#[cfg(target_os = "windows")]
fn clear_webview_disk_cache(win: &tauri::WebviewWindow) {
    let _ = win.with_webview(|webview| unsafe {
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            ICoreWebView2Profile2, ICoreWebView2_13,
            COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE,
        };
        use webview2_com::ClearBrowsingDataCompletedHandler;
        use windows::core::Interface;

        let core = match webview.controller().CoreWebView2() {
            Ok(c) => c,
            Err(_) => return,
        };
        let wv13 = match core.cast::<ICoreWebView2_13>() {
            Ok(w) => w,
            Err(_) => return,
        };
        let profile = match wv13.Profile() {
            Ok(p) => p,
            Err(_) => return,
        };
        let profile2 = match profile.cast::<ICoreWebView2Profile2>() {
            Ok(p) => p,
            Err(_) => return,
        };

        let handler = ClearBrowsingDataCompletedHandler::create(Box::new(|_hr| Ok(())));
        let _ = profile2
            .ClearBrowsingData(COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE, &handler);
    });
}

#[cfg(not(target_os = "windows"))]
fn apply_low_power_mode(_win: &tauri::WebviewWindow, _low: bool) {}

#[cfg(not(target_os = "windows"))]
fn clear_webview_disk_cache(_win: &tauri::WebviewWindow) {}

fn start_webview_maintenance(app: tauri::AppHandle) {
    std::thread::spawn(move || {
        let mut last_reload = Instant::now();
        let mut hidden_since: Option<Instant> = None;
        let mut unfocused_since: Option<Instant> = None;
        // `None` até sabermos o estado; força aplicar o modo de energia na 1ª volta.
        let mut low_power: Option<bool> = None;

        loop {
            std::thread::sleep(MAINTENANCE_POLL_EVERY);

            let now = Instant::now();
            let Some(win) = app.get_webview_window("main") else {
                // Self-heal: a janela só some daqui se a WebView morreu (crash
                // do WebView2 / kill externo) — o fechar normal apenas esconde.
                // Recria escondida na thread principal; o usuário reabre pelo
                // tray como sempre.
                hidden_since = None;
                unfocused_since = None;
                low_power = None;
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || {
                    if handle.get_webview_window("main").is_none() {
                        let _ = create_main_window(&handle, false, false);
                    }
                });
                last_reload = Instant::now();
                continue;
            };

            let visible = win.is_visible().unwrap_or(true);
            let minimized = win.is_minimized().unwrap_or(false);
            let focused = win.is_focused().unwrap_or(true);
            let hidden = !visible || minimized;

            if hidden {
                hidden_since.get_or_insert(now);
            } else {
                hidden_since = None;
            }
            if !focused {
                unfocused_since.get_or_insert(now);
            } else {
                unfocused_since = None;
            }

            // Modo de baixo consumo enquanto escondido na bandeja/minimizado:
            // devolve memória e desprioriza os processos do WebView2 pra não
            // brigar por CPU com outros apps. Só aplica na transição.
            if low_power != Some(hidden) {
                apply_low_power_mode(&win, hidden);
                low_power = Some(hidden);
            }

            let state = app.state::<RuntimeState>();
            let call_active = state.call_active.load(std::sync::atomic::Ordering::Relaxed);
            let heap_mb = state.heap_mb.load(std::sync::atomic::Ordering::Relaxed);

            if due_reload(now, last_reload, heap_mb, hidden_since, unfocused_since, call_active)
                && win.reload().is_ok()
            {
                last_reload = Instant::now();
                // Zera o heap conhecido: o valor velho (alto) não pode disparar
                // outro reload antes da página nova reportar o seu.
                state.heap_mb.store(0, std::sync::atomic::Ordering::Relaxed);
                // Limpa só o cache de disco (mídia acumulada), sem tocar em
                // cookies/IndexedDB — não desloga. Casado com o reload pra
                // manter o perfil do WebView2 enxuto ao longo do tempo.
                clear_webview_disk_cache(&win);
            }
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    wait_for_relaunch_predecessor();

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
            app.manage(RuntimeState::new());

            // Perfis. Tem que vir antes de qualquer janela: a adoção da pasta
            // legada move o diretório da WebView, e isso só é possível enquanto
            // ela não abriu.
            let mut profile_store = profiles::load(app.handle());
            let adopt_error = profiles::adopt_pending(app.handle(), &mut profile_store).err();
            let profiles_snapshot = profile_store.clone();
            app.manage(std::sync::Mutex::new(profile_store));
            if let Some(err) = adopt_error {
                app.dialog()
                    .message(err)
                    .title("WhatsApp Lite")
                    .show(|_| {});
            }

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

            // Perfis: um item marcável por perfil, mais o "Adicionar perfil…".
            // Sem perfis criados aparece só o "Adicionar" — quem usa sozinho não
            // precisa saber que isto existe. O menu não muda em runtime porque
            // toda alteração de perfil reinicia o app.
            let mut profile_items = Vec::new();
            for profile in &profiles_snapshot.profiles {
                let item = CheckMenuItemBuilder::with_id(
                    format!("profile:{}", profile.slug),
                    &profile.name,
                )
                .checked(Some(profile.slug.as_str()) == profiles_snapshot.active.as_deref())
                .build(app)?;
                profile_items.push((profile.slug.clone(), item));
            }
            let add_profile_item =
                MenuItemBuilder::with_id("add_profile", "Adicionar perfil…").build(app)?;

            let mut menu_builder = MenuBuilder::new(app).item(&show_item).separator();
            for (_, item) in &profile_items {
                menu_builder = menu_builder.item(item);
            }
            let tray_menu = menu_builder
                .item(&add_profile_item)
                .separator()
                .item(&autostart_item)
                .item(&quit_item)
                .build()?;

            // Tray icon
            let autostart_item_for_menu = autostart_item.clone();
            let active_slug = profiles_snapshot.active.clone();
            let profile_items_for_menu = profile_items.clone();
            let tooltip = match profiles_snapshot.active_name() {
                Some(name) => format!("WhatsApp Lite — {name}"),
                None => "WhatsApp Lite".to_string(),
            };
            let _tray = TrayIconBuilder::new()
                .icon(icon)
                .tooltip(&tooltip)
                .menu(&tray_menu)
                .on_menu_event(move |app, event| {
                    if let Some(slug) = event.id().0.strip_prefix("profile:") {
                        // Clicar no perfil já ativo só desmarcaria o item; remarca
                        // e não faz mais nada.
                        if Some(slug) == active_slug.as_deref() {
                            if let Some((_, item)) =
                                profile_items_for_menu.iter().find(|(s, _)| s == slug)
                            {
                                let _ = item.set_checked(true);
                            }
                            return;
                        }
                        let switched = profiles::set_active(app, slug)
                            .and_then(|()| relaunch(app));
                        if let Err(err) = switched {
                            app.dialog().message(err).title("WhatsApp Lite").show(|_| {});
                        }
                        return;
                    }

                    if event.id() == "add_profile" {
                        open_profiles_window(app);
                    } else if event.id() == "show" {
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
            send_notification,
            set_call_active,
            report_heap_mb,
            is_do_not_disturb,
            profiles_info,
            profiles_add,
            profiles_cancel
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // App de bandeja: destruir a última janela (crash da WebView, kill
            // externo) não pode encerrar o processo — o Tauri dispararia
            // ExitRequested com code None e sairia por padrão. Sair de verdade
            // continua sendo só pelo "Sair" do tray, que usa app.exit(0) e
            // chega aqui com code Some(0).
            if let tauri::RunEvent::ExitRequested { code: None, api, .. } = event {
                api.prevent_exit();
            }
        });
}
