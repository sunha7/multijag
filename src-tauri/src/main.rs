#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod jagex;
mod store;

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use jagex::Character;
use store::{master_key, seal, unseal, Account, Secrets, Settings, Store};

/// GitHub repository the in-app update check looks at (latest published release).
const REPO: &str = "sunha7/multijag";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    current: String,
    latest: String,
    url: String,
    available: bool,
}

struct AppState {
    store: Mutex<Store>,
    key: [u8; 32],
    dir: PathBuf,
    running: Mutex<HashMap<u32, RunningView>>,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RunningView {
    pid: u32,
    account_id: String,
    character_id: String,
    name: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AccountView {
    id: String,
    name: String,
    imported: bool,
    characters: Vec<Character>,
}

// What the UI sees: never tokens.
#[derive(Serialize, Clone)]
struct View {
    version: &'static str,
    settings: Settings,
    accounts: Vec<AccountView>,
    running: Vec<RunningView>,
}

fn view(st: &AppState) -> View {
    let store = st.store.lock().unwrap();
    View {
        version: env!("CARGO_PKG_VERSION"),
        settings: store.settings.clone(),
        accounts: store
            .accounts
            .iter()
            .map(|a| AccountView { id: a.id.clone(), name: a.name.clone(), imported: a.imported, characters: a.characters.clone() })
            .collect(),
        running: st.running.lock().unwrap().values().cloned().collect(),
    }
}

fn emit(app: &AppHandle) {
    let st = app.state::<AppState>();
    let _ = app.emit("state", view(&st));
}

fn webview_dir(st: &AppState, partition: &str) -> PathBuf {
    st.dir.join("webviews").join(partition)
}

fn store_id(partition: &str) -> [u8; 16] {
    uuid::Uuid::parse_str(partition).map(|u| *u.as_bytes()).unwrap_or([0; 16])
}

fn remove_webview_data(st: &AppState, partition: &str) {
    let _ = fs::remove_dir_all(webview_dir(st, partition));
}

fn with_account<T>(st: &AppState, id: &str, f: impl FnOnce(&mut Account) -> T) -> Result<T, String> {
    let mut store = st.store.lock().unwrap();
    let acct = store.accounts.iter_mut().find(|a| a.id == id).ok_or("Unknown account")?;
    let out = f(acct);
    store.save(&st.dir);
    Ok(out)
}

fn read_secrets(st: &AppState, id: &str) -> Result<Secrets, String> {
    let store = st.store.lock().unwrap();
    let acct = store.accounts.iter().find(|a| a.id == id).ok_or("Unknown account")?;
    Ok(acct.secrets.as_deref().and_then(|b| unseal(&st.key, b)).unwrap_or_default())
}

fn write_secrets(st: &AppState, id: &str, secrets: &Secrets) -> Result<(), String> {
    with_account(st, id, |a| a.secrets = Some(seal(&st.key, secrets)))
}

async fn ensure_tokens(app: &AppHandle, id: &str) -> Result<Secrets, String> {
    let st = app.state::<AppState>();
    let mut s = read_secrets(&st, id)?;
    let t = s.tokens.clone().ok_or("Account has no tokens; remove and re-add it")?;
    if jagex::now_ms() > t.expires_at - 60_000 {
        s.tokens = Some(jagex::refresh(&t).await?);
        write_secrets(&st, id, &s)?;
    }
    Ok(s)
}

async fn ensure_session(app: &AppHandle, id: &str) -> Result<String, String> {
    let st = app.state::<AppState>();
    if let Some(sid) = read_secrets(&st, id)?.session_id {
        if jagex::is_session_valid(&sid).await {
            return Ok(sid);
        }
    }
    let mut s = ensure_tokens(app, id).await?;
    let partition = with_account(&st, id, |a| a.partition.clone())?;
    let id_token = s.tokens.as_ref().map(|t| t.id_token.clone()).unwrap_or_default();
    let sid = jagex::create_game_session(app, webview_dir(&st, &partition), store_id(&partition), &id_token).await?;
    s.session_id = Some(sid.clone());
    write_secrets(&st, id, &s)?;
    Ok(sid)
}

#[tauri::command]
fn get_state(app: AppHandle) -> View {
    view(&app.state::<AppState>())
}

#[tauri::command]
async fn add_account(app: AppHandle) -> Result<(), String> {
    let st = app.state::<AppState>();
    // Fresh webview data per sign-in: separate cookies = separate simultaneous logins.
    let partition = uuid::Uuid::new_v4().to_string();
    let result = async {
        let tokens = jagex::login(&app, webview_dir(&st, &partition), store_id(&partition)).await?;
        let sub = jagex::decode_jwt(&tokens.id_token)?["sub"].as_str().ok_or("id_token has no sub")?.to_string();
        let name = jagex::get_account_name(&tokens).await;
        let secrets = Secrets { tokens: Some(tokens), session_id: None, ..Default::default() };
        {
            let mut store = st.store.lock().unwrap();
            let sealed = Some(seal(&st.key, &secrets));
            if let Some(existing) = store.accounts.iter_mut().find(|a| a.id == sub) {
                // Re-login of an already-added account: replace its tokens and webview data.
                remove_webview_data(&st, &existing.partition);
                existing.partition = partition.clone();
                existing.name = name;
                existing.secrets = sealed;
            } else {
                store.accounts.push(Account {
                    id: sub.clone(),
                    name,
                    partition: partition.clone(),
                    characters: vec![],
                    secrets: sealed,
                    imported: false,
                });
            }
            store.save(&st.dir);
        }
        emit(&app);
        Ok::<_, String>(sub)
    }
    .await;

    match result {
        Ok(sub) => refresh_characters(app, sub).await,
        Err(e) => {
            let used = st.store.lock().unwrap().accounts.iter().any(|a| a.partition == partition);
            if !used {
                remove_webview_data(&st, &partition);
            }
            Err(e)
        }
    }
}

#[tauri::command]
async fn refresh_characters(app: AppHandle, id: String) -> Result<(), String> {
    if with_account(&app.state::<AppState>(), &id, |a| a.imported)? {
        return Ok(()); // imported files describe exactly one character
    }
    let sid = ensure_session(&app, &id).await?;
    let mut characters = jagex::get_characters(&sid).await?;
    let st = app.state::<AppState>();
    with_account(&st, &id, |a| {
        // Keep the user's custom order; characters not seen before go last.
        let pos = |c: &Character| a.characters.iter().position(|o| o.account_id == c.account_id).unwrap_or(usize::MAX);
        characters.sort_by_key(pos);
        for c in &mut characters {
            c.hidden = a.characters.iter().any(|o| o.account_id == c.account_id && o.hidden);
            // Keep per-character launch options across refreshes.
            c.args = a.characters.iter().find(|o| o.account_id == c.account_id).map(|o| o.args.clone()).unwrap_or_default();
        }
        a.characters = characters;
    })?;
    emit(&app);
    Ok(())
}

#[tauri::command]
fn remove_account(app: AppHandle, id: String) {
    let st = app.state::<AppState>();
    {
        let mut store = st.store.lock().unwrap();
        if let Some(pos) = store.accounts.iter().position(|a| a.id == id) {
            let acct = store.accounts.remove(pos);
            remove_webview_data(&st, &acct.partition);
            store.save(&st.dir);
        }
    }
    emit(&app);
}

#[tauri::command]
async fn launch(app: AppHandle, id: String, character_id: String) -> Result<(), String> {
    let st = app.state::<AppState>();
    let (character, exe, extra_args) = {
        let store = st.store.lock().unwrap();
        let acct = store.accounts.iter().find(|a| a.id == id).ok_or("Unknown account")?;
        let character = acct
            .characters
            .iter()
            .find(|c| c.account_id == character_id)
            .cloned()
            .ok_or("Unknown character; try refreshing")?;
        let exe = store.settings.paths.get(&store.settings.client).cloned().unwrap_or_default();
        let client = &store.settings.client;
        let extra = pick_args(character.args.get(client), store.settings.args.get(client));
        (character, exe, split_args(&extra))
    };
    if exe.is_empty() || !Path::new(&exe).exists() {
        return Err(format!("Game client not found{}. Set it under Client paths.", if exe.is_empty() { String::new() } else { format!(": {exe}") }));
    }

    let (sid, cred_env) = if with_account(&st, &id, |a| a.imported)? {
        // Imported characters carry a fixed session: use it while valid, never try to renew.
        let env = read_secrets(&st, &id)?.imported.get(&character_id).cloned().unwrap_or_default();
        let sid = env.get("JX_SESSION_ID").cloned().unwrap_or_default();
        if sid.is_empty() || !jagex::is_session_valid(&sid).await {
            return Err("This imported session has expired. Remove this character and import a fresh credentials.properties file.".into());
        }
        (sid, env)
    } else {
        (ensure_session(&app, &id).await?, HashMap::new())
    };
    let mut cmd = Command::new(&exe);
    cmd.args(&extra_args)
        .env("JX_SESSION_ID", sid)
        .env("JX_CHARACTER_ID", &character.account_id)
        .env_remove("JX_ACCESS_TOKEN")
        .env_remove("JX_REFRESH_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match &character.display_name {
        Some(n) => cmd.env("JX_DISPLAY_NAME", n),
        None => cmd.env_remove("JX_DISPLAY_NAME"),
    };
    // Imported credentials: hand over exactly what the file contained (session, plus tokens if any).
    for (k, v) in &cred_env {
        cmd.env(k, v);
    }
    if let Some(dir) = Path::new(&exe).parent() {
        cmd.current_dir(dir);
    }
    let mut child = cmd.spawn().map_err(|e| format!("Could not start {exe}: {e}"))?;

    let pid = child.id();
    st.running.lock().unwrap().insert(
        pid,
        RunningView {
            pid,
            account_id: id,
            character_id,
            name: character.display_name.unwrap_or(character.account_id),
        },
    );
    emit(&app);

    let app2 = app.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        app2.state::<AppState>().running.lock().unwrap().remove(&pid);
        emit(&app2);
    });
    Ok(())
}

#[tauri::command]
fn reorder_accounts(app: AppHandle, ids: Vec<String>) {
    let st = app.state::<AppState>();
    {
        let mut store = st.store.lock().unwrap();
        store.accounts.sort_by_key(|a| ids.iter().position(|i| *i == a.id).unwrap_or(usize::MAX));
        store.save(&st.dir);
    }
    emit(&app);
}

#[tauri::command]
fn reorder_characters(app: AppHandle, id: String, ids: Vec<String>) -> Result<(), String> {
    let st = app.state::<AppState>();
    with_account(&st, &id, |a| {
        a.characters.sort_by_key(|c| ids.iter().position(|i| *i == c.account_id).unwrap_or(usize::MAX));
    })?;
    emit(&app);
    Ok(())
}

#[tauri::command]
fn set_hidden(app: AppHandle, id: String, character_id: String, hidden: bool) -> Result<(), String> {
    let st = app.state::<AppState>();
    with_account(&st, &id, |a| {
        if let Some(c) = a.characters.iter_mut().find(|c| c.account_id == character_id) {
            c.hidden = hidden;
        }
    })?;
    emit(&app);
    Ok(())
}

fn parse_version(v: &str) -> Vec<u64> {
    v.trim_start_matches(['v', 'V']).split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect()
}

#[tauri::command]
async fn check_update() -> Result<UpdateInfo, String> {
    let res = reqwest::Client::new()
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("User-Agent", "MultiJag")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("Could not reach GitHub: {e}"))?;
    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Err("No published release found yet (or the repository is private)".into());
    }
    if !res.status().is_success() {
        return Err(format!("GitHub returned HTTP {}", res.status()));
    }
    let body: serde_json::Value = res.json().await.map_err(|e| e.to_string())?;
    let latest = body["tag_name"].as_str().ok_or("Release has no tag")?.to_string();
    let current = env!("CARGO_PKG_VERSION").to_string();
    Ok(UpdateInfo {
        available: parse_version(&latest) > parse_version(&current),
        url: body["html_url"].as_str().unwrap_or_default().to_string(),
        latest,
        current,
    })
}

#[tauri::command]
fn open_link(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    // Only ever open our own repo and Jagex's account / legal pages.
    let allowed = [
        format!("https://github.com/{REPO}/"),
        "https://account.jagex.com/".to_string(),
        "https://legal.jagex.com/".to_string(),
    ];
    let lower = url.to_ascii_lowercase();
    if !allowed.iter().any(|p| lower.starts_with(&p.to_ascii_lowercase())) {
        return Err("Refusing to open an unexpected link".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_client(app: AppHandle, client: String) {
    let st = app.state::<AppState>();
    {
        let mut store = st.store.lock().unwrap();
        store.settings.client = client;
        store.save(&st.dir);
    }
    emit(&app);
}

/// Splits a launch-options string into arguments: whitespace separated, with "double" or
/// 'single' quotes grouping words that contain spaces (e.g. --profile "My Profile").
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            None => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// A character's own options win when set (non-blank); otherwise the client's global options.
fn pick_args(own: Option<&String>, global: Option<&String>) -> String {
    own.filter(|s| !s.trim().is_empty()).or(global).cloned().unwrap_or_default()
}

/// Values we keep from a RuneLite credentials.properties file.
struct ImportedCreds {
    character_id: String,
    display_name: Option<String>,
    env: HashMap<String, String>,
}

/// Parses a Java .properties-style credentials file (`KEY=VALUE`, `#` comments). Only the JX_*
/// keys RuneLite uses are read, and blank values are dropped.
fn parse_credentials(text: &str) -> Result<ImportedCreds, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let session = map.get("JX_SESSION_ID").filter(|v| !v.is_empty()).ok_or("missing JX_SESSION_ID")?.clone();
    let character_id = map.get("JX_CHARACTER_ID").filter(|v| !v.is_empty()).ok_or("missing JX_CHARACTER_ID")?.clone();
    let display_name = map.get("JX_DISPLAY_NAME").filter(|v| !v.is_empty()).cloned();
    let mut env = HashMap::from([("JX_SESSION_ID".to_string(), session)]);
    for k in ["JX_ACCESS_TOKEN", "JX_REFRESH_TOKEN"] {
        if let Some(v) = map.get(k).filter(|v| !v.is_empty()) {
            env.insert(k.to_string(), v.clone());
        }
    }
    Ok(ImportedCreds { character_id, display_name, env })
}

const IMPORTED_ID: &str = "imported";

/// Adds characters to the single "Imported credentials" group (creating it if needed). Each
/// character's JX_* values are kept in the group's encrypted secrets. Re-importing a character
/// swaps in the fresh session but keeps its hidden flag and launch options.
fn add_imported(store: &mut Store, key: &[u8; 32], items: Vec<(Character, HashMap<String, String>)>) {
    let pos = match store.accounts.iter().position(|a| a.id == IMPORTED_ID) {
        Some(p) => p,
        None => {
            store.accounts.push(Account {
                id: IMPORTED_ID.into(),
                name: "Imported credentials".into(),
                partition: String::new(),
                characters: vec![],
                secrets: None,
                imported: true,
            });
            store.accounts.len() - 1
        }
    };
    let acct = &mut store.accounts[pos];
    let mut secrets = acct.secrets.as_deref().and_then(|b| unseal(key, b)).unwrap_or_default();
    for (ch, env) in items {
        secrets.imported.insert(ch.account_id.clone(), env);
        match acct.characters.iter_mut().find(|c| c.account_id == ch.account_id) {
            Some(existing) => {
                if ch.display_name.is_some() {
                    existing.display_name = ch.display_name;
                }
            }
            None => acct.characters.push(ch),
        }
    }
    acct.secrets = Some(seal(key, &secrets));
}

/// Folds the old one-account-per-file imports (id `cred-…`) into the single group.
/// Returns true if anything changed. Accounts that can't be decrypted are left alone.
fn migrate_legacy_imported(store: &mut Store, key: &[u8; 32]) -> bool {
    let mut items = Vec::new();
    let mut migrated = Vec::new();
    for a in store.accounts.iter().filter(|a| a.imported && a.id != IMPORTED_ID) {
        let Some(secrets) = a.secrets.as_deref().and_then(|b| unseal(key, b)) else { continue };
        if secrets.cred_env.is_empty() {
            continue;
        }
        for c in &a.characters {
            items.push((c.clone(), secrets.cred_env.clone()));
        }
        migrated.push(a.id.clone());
    }
    if migrated.is_empty() {
        return false;
    }
    store.accounts.retain(|a| !migrated.contains(&a.id));
    add_imported(store, key, items);
    true
}

#[tauri::command]
fn remove_imported_character(app: AppHandle, character_id: String) {
    let st = app.state::<AppState>();
    {
        let mut store = st.store.lock().unwrap();
        if let Some(pos) = store.accounts.iter().position(|a| a.id == IMPORTED_ID) {
            let acct = &mut store.accounts[pos];
            acct.characters.retain(|c| c.account_id != character_id);
            if acct.characters.is_empty() {
                store.accounts.remove(pos);
            } else {
                let mut secrets = acct.secrets.as_deref().and_then(|b| unseal(&st.key, b)).unwrap_or_default();
                secrets.imported.remove(&character_id);
                acct.secrets = Some(seal(&st.key, &secrets));
            }
            store.save(&st.dir);
        }
    }
    emit(&app);
}

#[tauri::command]
async fn import_credentials(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_title("Select credentials.properties file(s) to import").pick_files(move |p| {
        let _ = tx.send(p);
    });
    let Some(picked) = rx.await.map_err(|e| e.to_string())? else { return Ok(()) };

    let st = app.state::<AppState>();
    let mut failures = Vec::new();
    for fp in picked {
        let path = fp.into_path().map_err(|e| e.to_string())?;
        let file = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let parsed = match fs::metadata(&path) {
            Ok(m) if m.len() > 64 * 1024 => Err("file is too large to be a credentials file".to_string()),
            Ok(_) => fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| parse_credentials(&t)),
            Err(e) => Err(e.to_string()),
        };
        match parsed {
            Err(e) => failures.push(format!("{file}: {e}")),
            Ok(c) => {
                let character = Character {
                    account_id: c.character_id,
                    display_name: c.display_name,
                    hidden: false,
                    args: HashMap::new(),
                    extra: Default::default(),
                };
                let mut store = st.store.lock().unwrap();
                add_imported(&mut store, &st.key, vec![(character, c.env)]);
                store.save(&st.dir);
            }
        }
    }
    emit(&app);
    if failures.is_empty() { Ok(()) } else { Err(format!("Could not import: {}", failures.join("; "))) }
}

#[tauri::command]
fn set_character_args(app: AppHandle, id: String, character_id: String, client: String, args: String) -> Result<(), String> {
    let st = app.state::<AppState>();
    with_account(&st, &id, |a| {
        if let Some(c) = a.characters.iter_mut().find(|c| c.account_id == character_id) {
            let args = args.trim().to_string();
            if args.is_empty() {
                c.args.remove(&client);
            } else {
                c.args.insert(client, args);
            }
        }
    })?;
    emit(&app);
    Ok(())
}

#[tauri::command]
fn set_args(app: AppHandle, client: String, args: String) {
    let st = app.state::<AppState>();
    {
        let mut store = st.store.lock().unwrap();
        store.settings.args.insert(client, args.trim().to_string());
        store.save(&st.dir);
    }
    emit(&app);
}

#[tauri::command]
async fn browse_client(app: AppHandle, client: String) -> Result<(), String> {
    use tauri_plugin_dialog::DialogExt;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_title("Select the game client executable").pick_file(move |p| {
        let _ = tx.send(p);
    });
    if let Some(picked) = rx.await.map_err(|e| e.to_string())? {
        let path = picked.into_path().map_err(|e| e.to_string())?;
        let st = app.state::<AppState>();
        {
            let mut store = st.store.lock().unwrap();
            store.settings.paths.insert(client, path.to_string_lossy().into_owned());
            store.save(&st.dir);
        }
        emit(&app);
    }
    Ok(())
}

// Debug builds only: MULTIJAG_CAPTURE_TEST=http://127.0.0.1:PORT runs the redirect-capture
// logic against a local fake server (/302jagex, /jsjagex, /302local) and exits.
#[cfg(debug_assertions)]
fn capture_selftest(app: AppHandle, base: String) {
    tauri::async_runtime::spawn(async move {
        let cases = [("/302jagex", 0), ("/jsjagex", 0), ("/302local", 1)];
        let preds = jagex::selftest_predicates();
        let dir = std::env::temp_dir().join("multijag-capture-test");
        for (path, p) in cases {
            let url = url::Url::parse(&format!("{base}{path}")).unwrap();
            let r = jagex::capture_redirect(&app, dir.clone(), [7; 16], url, "test", preds[p].1).await;
            println!("RESULT {path} -> {r:?}");
        }
        app.exit(0);
    });
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            fs::create_dir_all(&dir)?;
            let key = master_key(&dir);
            let mut store = Store::load(&dir);
            if migrate_legacy_imported(&mut store, &key) {
                store.save(&dir);
            }
            app.manage(AppState { key, store: Mutex::new(store), dir, running: Mutex::new(HashMap::new()) });
            #[cfg(debug_assertions)]
            if let Ok(base) = std::env::var("MULTIJAG_CAPTURE_TEST") {
                capture_selftest(app.handle().clone(), base);
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            add_account,
            remove_account,
            refresh_characters,
            launch,
            reorder_accounts,
            reorder_characters,
            set_hidden,
            check_update,
            open_link,
            set_client,
            set_args,
            import_credentials,
            remove_imported_character,
            set_character_args,
            browse_client
        ])
        .run(tauri::generate_context!())
        .expect("error while running MultiJag");
}

#[cfg(test)]
mod tests {
    use super::{add_imported, migrate_legacy_imported, parse_credentials, pick_args, split_args};
    use crate::jagex::Character;
    use crate::store::{seal, unseal, Account, Secrets, Settings, Store};
    use std::collections::HashMap;

    fn chr(id: &str, name: &str) -> Character {
        Character { account_id: id.into(), display_name: Some(name.into()), hidden: false, args: HashMap::new(), extra: Default::default() }
    }
    fn env(sid: &str) -> HashMap<String, String> {
        HashMap::from([("JX_SESSION_ID".to_string(), sid.to_string())])
    }
    fn empty_store() -> Store {
        Store { settings: Settings { client: "runelite".into(), paths: HashMap::new(), args: HashMap::new() }, accounts: vec![] }
    }

    #[test]
    fn imports_share_one_group_and_migrate() {
        let key = [9u8; 32];
        let mut st = empty_store();
        // Two legacy one-account-per-file imports plus a normal account.
        for (id, sid) in [("1", "s1"), ("2", "s2")] {
            let secrets = Secrets { cred_env: env(sid), ..Default::default() };
            st.accounts.push(Account { id: format!("cred-{id}"), name: id.into(), partition: String::new(), characters: vec![chr(id, id)], secrets: Some(seal(&key, &secrets)), imported: true });
        }
        st.accounts.push(Account { id: "normal".into(), name: "n".into(), partition: "p".into(), characters: vec![], secrets: None, imported: false });
        assert!(migrate_legacy_imported(&mut st, &key));
        assert_eq!(st.accounts.len(), 2);
        let g = st.accounts.iter().find(|a| a.id == "imported").unwrap();
        assert_eq!(g.characters.len(), 2);
        let s = unseal(&key, g.secrets.as_ref().unwrap()).unwrap();
        assert_eq!(s.imported["1"]["JX_SESSION_ID"], "s1");
        assert_eq!(s.imported["2"]["JX_SESSION_ID"], "s2");
        assert!(!migrate_legacy_imported(&mut st, &key)); // idempotent

        // Re-import swaps the session but keeps the character's settings.
        st.accounts.iter_mut().find(|a| a.id == "imported").unwrap().characters[0].hidden = true;
        add_imported(&mut st, &key, vec![(chr("1", "One"), env("fresh"))]);
        let g = st.accounts.iter().find(|a| a.id == "imported").unwrap();
        assert_eq!(g.characters.len(), 2);
        assert!(g.characters[0].hidden);
        assert_eq!(g.characters[0].display_name.as_deref(), Some("One"));
        assert_eq!(unseal(&key, g.secrets.as_ref().unwrap()).unwrap().imported["1"]["JX_SESSION_ID"], "fresh");
    }

    #[test]
    fn parses_credentials_files() {
        let c = parse_credentials("#Do not share\n#date\nJX_ACCESS_TOKEN=\nJX_CHARACTER_ID=42\nJX_DISPLAY_NAME=Some Name\nJX_REFRESH_TOKEN=\nJX_SESSION_ID=abc\n").unwrap();
        assert_eq!(c.character_id, "42");
        assert_eq!(c.display_name.as_deref(), Some("Some Name"));
        assert_eq!(c.env.len(), 1); // blank tokens are dropped
        assert_eq!(c.env["JX_SESSION_ID"], "abc");
        let c = parse_credentials("JX_CHARACTER_ID=1\r\nJX_SESSION_ID=x\r\nJX_ACCESS_TOKEN=t\r\n").unwrap();
        assert_eq!(c.env["JX_ACCESS_TOKEN"], "t");
        assert!(parse_credentials("JX_CHARACTER_ID=1").is_err());
        assert!(parse_credentials("JX_SESSION_ID=x").is_err());
    }

    #[test]
    fn character_args_override_global() {
        let own = "--profile Alt".to_string();
        let global = "--debug".to_string();
        let blank = "  ".to_string();
        assert_eq!(pick_args(Some(&own), Some(&global)), "--profile Alt");
        assert_eq!(pick_args(None, Some(&global)), "--debug");
        assert_eq!(pick_args(Some(&blank), Some(&global)), "--debug");
        assert_eq!(pick_args(Some(&own), None), "--profile Alt");
        assert_eq!(pick_args(None, None), "");
    }

    #[test]
    fn splits_args() {
        assert_eq!(split_args(""), Vec::<String>::new());
        assert_eq!(split_args("  --debug   -J-Xmx2g "), vec!["--debug", "-J-Xmx2g"]);
        assert_eq!(split_args(r#"--profile "My Alt" --safe-mode"#), vec!["--profile", "My Alt", "--safe-mode"]);
        assert_eq!(split_args("--profile 'a b'"), vec!["--profile", "a b"]);
        assert_eq!(split_args(r#"--x """#), vec!["--x", ""]);
    }
}
