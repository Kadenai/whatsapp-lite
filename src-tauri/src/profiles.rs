// Perfis: cada um é uma pasta de dados própria do WebView2, o que na prática
// equivale a dois navegadores diferentes — sessão, cookies e IndexedDB
// separados. Só um perfil roda por vez; trocar de perfil reinicia o app.
//
// Enquanto o usuário não adicionar o segundo perfil nada disto existe no disco:
// `profiles.json` não é criado e a sessão continua na pasta padrão do Tauri
// (`<local_data>/EBWebView`). Quem usa sozinho nunca vê o recurso.
//
// A pasta legada só é adotada pelo primeiro perfil no boot seguinte ao
// "Adicionar perfil…", com a WebView ainda fechada. É a única hora em que dá
// pra mover: o WebView2 mantém a pasta travada enquanto está aberta.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauri::Manager as _;

const MAX_NAME_LEN: usize = 24;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub slug: String,
}

/// Estado persistido em `<local_data>/profiles.json`.
///
/// `profiles` vazio é o modo legado: sem perfis, pasta padrão, menu do tray
/// mostrando só "Adicionar perfil…".
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub profiles: Vec<Profile>,
    /// Slug do perfil ativo.
    #[serde(default)]
    pub active: Option<String>,
    /// Slug que ainda precisa adotar a pasta legada no próximo boot.
    #[serde(default)]
    pub pending_adopt: Option<String>,
    /// A tela de boas-vindas já foi respondida. Sem isto ela voltaria a cada
    /// abertura de quem respondeu "só eu uso" — que é justamente quem não quer
    /// ouvir falar de perfil nunca mais.
    #[serde(default)]
    pub onboarding_done: bool,
}

impl Store {
    pub fn active_profile(&self) -> Option<&Profile> {
        let slug = self.active.as_deref()?;
        self.profiles.iter().find(|p| p.slug == slug)
    }

    pub fn active_name(&self) -> Option<&str> {
        self.active_profile().map(|p| p.name.as_str())
    }
}

fn base_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path().app_local_data_dir().ok()
}

fn store_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    base_dir(app).map(|dir| dir.join("profiles.json"))
}

/// Carrega o estado. Qualquer falha cai no default (modo legado) — abrir sem
/// perfis é sempre seguro, a sessão da pasta padrão continua lá.
pub fn load(app: &tauri::AppHandle) -> Store {
    let Some(path) = store_path(app) else {
        return Store::default();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Store::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

pub fn save(app: &tauri::AppHandle, store: &Store) -> std::io::Result<()> {
    let path = store_path(app).ok_or_else(|| std::io::Error::other("sem app_local_data_dir"))?;
    write_atomic(&path, store)
}

/// Escrita atômica (temporário + rename): um desligamento no meio da escrita não
/// pode deixar um JSON truncado, porque é ele que diz onde mora a sessão.
fn write_atomic(path: &Path, store: &Store) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(store).map_err(std::io::Error::other)?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    // No Windows o rename falha se o destino existe.
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)
}

/// Pasta de dados da WebView do perfil ativo, ou `None` no modo legado (aí o
/// Tauri usa o default dele e nada muda de lugar).
pub fn data_dir(app: &tauri::AppHandle, store: &Store) -> Option<PathBuf> {
    let slug = store.active.as_deref()?;
    Some(base_dir(app)?.join("profiles").join(slug))
}

/// Lê a pasta do perfil ativo a partir do estado gerenciado. Usada na criação
/// das janelas, que acontece depois do `manage`.
pub fn managed_data_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    let state = app.try_state::<std::sync::Mutex<Store>>()?;
    let store = state.lock().ok()?;
    data_dir(app, &store)
}

pub fn managed_active_name(app: &tauri::AppHandle) -> Option<String> {
    let state = app.try_state::<std::sync::Mutex<Store>>()?;
    let store = state.lock().ok()?;
    store.active_name().map(str::to_string)
}

fn slugify(name: &str, taken: &[Profile]) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let base = slug.trim_matches('-').to_string();
    let base = if base.is_empty() {
        "perfil".to_string()
    } else {
        base
    };

    if !taken.iter().any(|p| p.slug == base) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if !taken.iter().any(|p| p.slug == candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn validate_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("O nome não pode ficar em branco.".to_string());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(format!("Use até {MAX_NAME_LEN} caracteres."));
    }
    if name.chars().any(char::is_control) {
        return Err("O nome tem caracteres inválidos.".to_string());
    }
    Ok(name.to_string())
}

/// Cria um perfil. Na primeira vez também batiza a sessão que já está aberta:
/// `current_name` nomeia o que existe hoje, `new_name` o que será criado.
///
/// Só grava o estado — mover a pasta legada e abrir o perfil novo é trabalho do
/// boot seguinte, porque a WebView atual ainda está com a pasta travada.
pub fn add(
    app: &tauri::AppHandle,
    current_name: Option<String>,
    new_name: String,
) -> Result<(), String> {
    let mut store = load(app);
    let first_time = store.profiles.is_empty();

    if first_time {
        let current = validate_name(
            current_name
                .as_deref()
                .ok_or("Falta o nome do perfil atual.")?,
        )?;
        let new = validate_name(&new_name)?;
        if current.eq_ignore_ascii_case(&new) {
            return Err("Os dois perfis não podem ter o mesmo nome.".to_string());
        }

        let current_slug = slugify(&current, &store.profiles);
        store.profiles.push(Profile {
            name: current,
            slug: current_slug.clone(),
        });
        let new_slug = slugify(&new, &store.profiles);
        store.profiles.push(Profile {
            name: new,
            slug: new_slug.clone(),
        });
        store.active = Some(new_slug);
        // A sessão de hoje passa a pertencer ao primeiro perfil.
        store.pending_adopt = Some(current_slug);
    } else {
        let new = validate_name(&new_name)?;
        if store
            .profiles
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(&new))
        {
            return Err("Já existe um perfil com esse nome.".to_string());
        }
        let new_slug = slugify(&new, &store.profiles);
        store.profiles.push(Profile {
            name: new,
            slug: new_slug.clone(),
        });
        store.active = Some(new_slug);
    }

    save(app, &store).map_err(|err| format!("Falha ao gravar profiles.json: {err}"))
}

/// Cria o primeiro perfil a partir da tela de boas-vindas.
///
/// Diferente de `add`, aqui não há segundo nome a pedir: o perfil criado é o
/// dono da sessão que já está aberta, então ele é ao mesmo tempo o ativo e o
/// que vai adotar a pasta legada no boot seguinte. Numa instalação nova não há
/// pasta pra adotar e a rotina de adoção simplesmente segue em frente.
pub fn create_first(app: &tauri::AppHandle, name: String) -> Result<(), String> {
    let mut store = load(app);
    create_first_in(&mut store, &name)?;
    save(app, &store).map_err(|err| format!("Falha ao gravar profiles.json: {err}"))
}

fn create_first_in(store: &mut Store, name: &str) -> Result<(), String> {
    if !store.profiles.is_empty() {
        return Err("Já existem perfis criados.".to_string());
    }

    let name = validate_name(name)?;
    let slug = slugify(&name, &store.profiles);
    store.profiles.push(Profile {
        name,
        slug: slug.clone(),
    });
    store.active = Some(slug.clone());
    store.pending_adopt = Some(slug);
    store.onboarding_done = true;
    Ok(())
}

/// Registra que a tela de boas-vindas já foi respondida, sem criar perfil.
pub fn mark_onboarding_done(app: &tauri::AppHandle) -> Result<(), String> {
    let mut store = load(app);
    store.onboarding_done = true;
    save(app, &store).map_err(|err| format!("Falha ao gravar profiles.json: {err}"))
}

/// Troca o perfil ativo. Quem chama reinicia o app em seguida.
pub fn set_active(app: &tauri::AppHandle, slug: &str) -> Result<(), String> {
    let mut store = load(app);
    if !store.profiles.iter().any(|p| p.slug == slug) {
        return Err("Perfil inexistente.".to_string());
    }
    store.active = Some(slug.to_string());
    save(app, &store).map_err(|err| format!("Falha ao gravar profiles.json: {err}"))
}

/// Move a pasta legada pra dentro do primeiro perfil. Roda no boot, antes de
/// qualquer janela existir.
///
/// Falhar aqui significaria abrir o perfil novo com a sessão antiga órfã na
/// pasta legada — inaceitável. Então o fracasso reverte tudo pro modo legado: o
/// app abre exatamente como antes, ainda logado, e o usuário tenta de novo.
pub fn adopt_pending(app: &tauri::AppHandle, store: &mut Store) -> Result<(), String> {
    let Some(slug) = store.pending_adopt.clone() else {
        return Ok(());
    };
    let Some(base) = base_dir(app) else {
        return Ok(());
    };

    let legacy = base.join("EBWebView");
    let dest_parent = base.join("profiles").join(&slug);
    let dest = dest_parent.join("EBWebView");

    let mut attempt = move_legacy(&legacy, &dest_parent, &dest);
    // Retry curto: antivírus e o Indexador do Windows podem segurar um handle na
    // pasta por alguns instantes depois que o processo anterior morre.
    for _ in 0..4 {
        if attempt.is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
        attempt = move_legacy(&legacy, &dest_parent, &dest);
    }

    match attempt {
        Ok(()) => {
            store.pending_adopt = None;
            let _ = save(app, store);
            Ok(())
        }
        Err(err) => {
            *store = Store::default();
            let _ = save(app, store);
            Err(format!(
                "Não foi possível separar a sessão atual em um perfil ({err}). \
                 Nada foi perdido: o app abriu como antes. Tente de novo."
            ))
        }
    }
}

fn move_legacy(legacy: &Path, dest_parent: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest_parent)?;
    if dest.exists() {
        // Já adotada numa tentativa anterior; nada a fazer.
        return Ok(());
    }
    if !legacy.exists() {
        // Instalação nova, sem sessão pra adotar.
        return Ok(());
    }
    std::fs::rename(legacy, dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, slug: &str) -> Profile {
        Profile {
            name: name.to_string(),
            slug: slug.to_string(),
        }
    }

    #[test]
    fn slug_normaliza_e_desambigua() {
        assert_eq!(slugify("Levi Raniere", &[]), "levi-raniere");
        assert_eq!(slugify("  Dra. Ana  ", &[]), "dra-ana");
        assert_eq!(slugify("!!!", &[]), "perfil");
        let taken = vec![profile("Ana", "ana")];
        assert_eq!(slugify("ana", &taken), "ana-2");
    }

    #[test]
    fn nome_invalido_e_recusado() {
        assert!(validate_name("  ").is_err());
        assert!(validate_name(&"a".repeat(MAX_NAME_LEN + 1)).is_err());
        assert_eq!(validate_name("  Levi  ").unwrap(), "Levi");
    }

    #[test]
    fn primeiro_perfil_adota_a_sessao_atual() {
        let mut store = Store::default();
        create_first_in(&mut store, "  Levi  ").unwrap();

        assert_eq!(store.profiles.len(), 1);
        assert_eq!(store.profiles[0].name, "Levi");
        assert_eq!(store.active.as_deref(), Some("levi"));
        // O perfil criado tem que herdar a sessao que ja esta aberta, senao ela
        // fica orfa na pasta legada e o usuario cai num QR code.
        assert_eq!(store.pending_adopt.as_deref(), Some("levi"));
        assert!(store.onboarding_done);
    }

    #[test]
    fn primeiro_perfil_recusa_quando_ja_existe_algum() {
        let mut store = Store {
            profiles: vec![profile("Ana", "ana")],
            ..Store::default()
        };
        assert!(create_first_in(&mut store, "Levi").is_err());
        assert_eq!(store.profiles.len(), 1);
    }

    #[test]
    fn profiles_json_de_versao_anterior_continua_carregando() {
        // Arquivo escrito pela 1.1.0, sem o campo de boas-vindas.
        let raw = r#"{"profiles":[{"name":"Ana","slug":"ana"}],"active":"ana"}"#;
        let store: Store = serde_json::from_str(raw).unwrap();

        assert_eq!(store.active.as_deref(), Some("ana"));
        // O campo ausente vira `false` em vez de quebrar o parse. Quem ja tem
        // perfil nao ve a tela mesmo assim: o gate no boot tambem exige que a
        // lista de perfis esteja vazia.
        assert!(!store.onboarding_done);
    }
}
