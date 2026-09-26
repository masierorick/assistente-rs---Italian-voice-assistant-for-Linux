use anyhow::Result;
use serde::Deserialize;
use std::fs::File;
use std::io::BufReader;
use std::process::Command;
use std::path::Path;
use crate::audio_filter;

#[derive(Debug, Deserialize)]
struct Station {
    name: String,
    url: String,
}

fn load_stations(stations_json: &Path) -> Vec<Station> {
    File::open(stations_json)
        .ok()
        .and_then(|file| serde_json::from_reader(BufReader::new(file)).ok())
        .unwrap_or_default()
}

pub fn play_radio_stream(url: &str) -> Result<()> {
    // Ferma uno stream radio già in riproduzione, se presente, cosi'
    // non restano piu' stream sovrapposti e radio_reference punta
    // sempre a un solo processo attivo.
    stop_radio();

    let child = Command::new("ffplay")
        .args(["-nodisp", "-loglevel", "panic", url])
        .spawn()?;

    audio_filter::set_radio_reference(Some(child.id().to_string()));
    Ok(())
}

pub fn search_and_play(comando: &str, stations_json: &Path) {
    let comando_lower = comando.to_lowercase();
    for stazione in load_stations(stations_json) {
        if !stazione.name.is_empty()
            && !stazione.url.is_empty()
            && comando_lower.contains(&stazione.name.to_lowercase())
        {
            let _ = play_radio_stream(&stazione.url);
            if let Ok(messages) = crate::config::load_messages() {
                let msg = messages.other_messages["radio_station_opened"]
                    .as_str()
                    .unwrap_or("[missing: other_messages.radio_station_opened]")
                    .replace("{stazione}", &stazione.name);
                let _ = crate::tts::speak(&msg);
            }
            return;
        }
    }
    if let Ok(messages) = crate::config::load_messages() {
        let msg = messages.error_messages["radio_not_found"]
            .as_str()
            .unwrap_or("[missing: error_messages.radio_not_found]");
        let _ = crate::tts::speak(msg);
    }
}

pub fn stop_radio() {
    if let Some(pid_str) = audio_filter::get_radio_reference() {
        if let Ok(pid) = pid_str.parse::<u32>() {
            let _ = Command::new("kill").arg(pid.to_string()).status();
        }
        audio_filter::set_radio_reference(None);
        return;
    }
    // fallback se non c'è un riferimento salvato (es. radio avviata prima di questa modifica)
    let _ = Command::new("pkill").arg("ffplay").status();
}

/// Restituisce la lista delle stazioni come testo formattato
pub fn lista_stazioni(stations_json: &Path) -> String {
    let intestazione = crate::config::load_messages()
        .ok()
        .and_then(|m| m.other_messages["radio_list_header"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "[missing: other_messages.radio_list_header]".to_string());
    let mut testo = format!("{}\n", intestazione);
    for stazione in load_stations(stations_json) {
        testo.push_str(&stazione.name);
        testo.push('\n');
    }
    testo
}
