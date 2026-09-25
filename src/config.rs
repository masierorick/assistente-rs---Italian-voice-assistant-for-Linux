use std::fs;
use std::path::PathBuf;
use std::process::Command;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use dirs;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub botname: String,
    pub wakeword: String,
    pub sleep_time: u64,
    pub deltavolume: u8,
    pub layout: String,
    #[serde(default)]
    pub musicplayer: String,
    #[serde(default)]
    pub browser: String,
    #[serde(default)]
    pub whisper_model: Option<String>,
    #[serde(default)]
    pub piper_bin: Option<String>,
    #[serde(default)]
    pub piper_model: Option<String>,
    #[serde(default)]
    pub speaker_id_model: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Messages {
    pub welcome_messages: Vec<String>,
    pub goodbye_messages: Vec<String>,
    pub error_messages: serde_json::Value,
    pub other_messages: serde_json::Value,
    pub commands: serde_json::Value,
    pub objects: serde_json::Value,
}

/// Restituisce il percorso del file di configurazione seguendo questa priorità:
/// 1. ~/.config/assistente-rs/<file>
/// 2. /usr/share/assistente/config/<file>
/// 3. ./config/<file> (solo come fallback per sviluppo)
pub fn config_path(file: &str) -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        let user_path = home.join(".config").join("assistente-rs").join(file);
        if user_path.exists() {
            return user_path;
        }
    }

    let system_path = PathBuf::from("/usr/share/assistente/config").join(file);
    if system_path.exists() {
        return system_path;
    }

    PathBuf::from("config").join(file)
}

pub fn env_path() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        let user_path = home.join(".config").join("assistente-rs").join(".env");
        if user_path.exists() {
            return user_path;
        }
    }
    let system_path = PathBuf::from("/usr/share/assistente/config").join(".env");
    if system_path.exists() {
        return system_path;
    }
    PathBuf::from("config").join(".env")
}

pub fn load_config() -> Result<Config> {
    let path = config_path("config.json");
    let data = fs::read_to_string(&path)?;
    let mut cfg: Config = serde_json::from_str(&data)?;

    cfg.whisper_model = cfg.whisper_model.map(|p| expand_tilde(&p));
    cfg.piper_bin = cfg.piper_bin.map(|p| expand_tilde(&p));
    cfg.piper_model = cfg.piper_model.map(|p| expand_tilde(&p));
    cfg.speaker_id_model = cfg.speaker_id_model.map(|p| expand_tilde(&p));

    // Se browser/musicplayer non sono impostati in config.json, li rileva
    // automaticamente dal sistema e li salva, cosi' la rilevazione avviene
    // una sola volta (al primo avvio) e non ad ogni load_config().
    let mut modificato = false;

    if cfg.browser.trim().is_empty() {
        if let Some(browser) = detect_default_browser() {
            println!("🌐 Browser predefinito rilevato: {}", browser);
            cfg.browser = browser;
            modificato = true;
        } else {
            eprintln!("⚠️  Impossibile rilevare automaticamente il browser predefinito: impostalo manualmente in config.json");
        }
    }

    if cfg.musicplayer.trim().is_empty() {
        if let Some(player) = detect_default_musicplayer() {
            println!("🎵 Music player predefinito rilevato: {}", player);
            cfg.musicplayer = player;
            modificato = true;
        } else {
            eprintln!("⚠️  Impossibile rilevare automaticamente il music player predefinito: impostalo manualmente in config.json");
        }
    }

    if modificato {
        salva_config(&path, &cfg);
    }

    Ok(cfg)
}

fn salva_config(path: &PathBuf, cfg: &Config) {
    match serde_json::to_string_pretty(cfg) {
        Ok(json) => {
            if let Err(e) = fs::write(path, json) {
                eprintln!("⚠️  Impossibile salvare config.json aggiornato: {}", e);
            }
        }
        Err(e) => eprintln!("⚠️  Impossibile serializzare config.json aggiornato: {}", e),
    }
}

/// Rileva il browser predefinito di sistema tramite xdg-settings e ne
/// ricava l'eseguibile leggendo il relativo file .desktop.
fn detect_default_browser() -> Option<String> {
    let output = Command::new("xdg-settings")
    .args(["get", "default-web-browser"])
    .output()
    .ok()?;

    let desktop_file = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if desktop_file.is_empty() {
        return None;
    }

    eseguibile_da_desktop_file(&desktop_file)
}

/// Non esiste un equivalente diretto di xdg-settings per i music player,
/// quindi si usa xdg-mime sui mime-type audio piu' comuni per risalire
/// all'app associata; in mancanza di risultato si ripiega su un elenco
/// di player noti, verificando quale sia installato nel PATH.
fn detect_default_musicplayer() -> Option<String> {
    for mime in ["audio/mpeg", "audio/x-flac", "audio/ogg", "audio/x-wav"] {
        if let Ok(output) = Command::new("xdg-mime")
            .args(["query", "default", mime])
            .output()
            {
                let desktop_file = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !desktop_file.is_empty() {
                    if let Some(eseguibile) = eseguibile_da_desktop_file(&desktop_file) {
                        return Some(eseguibile);
                    }
                }
            }
    }

    for candidato in [
        "vlc", "mpv", "audacious", "clementine", "rhythmbox",
        "elisa", "amarok", "strawberry", "cmus",
    ] {
        if comando_disponibile(candidato) {
            return Some(candidato.to_string());
        }
    }

    None
}

fn comando_disponibile(bin: &str) -> bool {
    Command::new("which")
    .arg(bin)
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// Data la stringa di un file .desktop (es. "firefox.desktop"), lo cerca
/// nelle cartelle standard XDG e ne estrae l'eseguibile dalla riga Exec=.
fn eseguibile_da_desktop_file(desktop_file: &str) -> Option<String> {
    let mut dirs_desktop = vec![
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
    ];
    if let Some(home) = dirs::home_dir() {
        dirs_desktop.push(home.join(".local/share/applications"));
    }

    for dir in dirs_desktop {
        let path = dir.join(desktop_file);
        if let Ok(contenuto) = fs::read_to_string(&path) {
            for riga in contenuto.lines() {
                if let Some(exec) = riga.strip_prefix("Exec=") {
                    let comando = exec.split_whitespace().next().unwrap_or("");
                    let eseguibile = comando.rsplit('/').next().unwrap_or(comando);
                    if !eseguibile.is_empty() {
                        return Some(eseguibile.to_string());
                    }
                }
            }
        }
    }

    None
}

pub fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest).to_string_lossy().to_string();
        }
    }
    path.to_string()
}

pub fn load_messages() -> Result<Messages> {
    let path = config_path("messages_it.json");
    let data = fs::read_to_string(path)?;
    Ok(serde_json::from_str::<Messages>(&data)?)
}
