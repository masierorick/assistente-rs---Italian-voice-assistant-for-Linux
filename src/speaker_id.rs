// Riconoscimento del parlante (speaker identification), offline via sherpa-onnx.
// Usa lo stesso db (~/.local/share/assistente-rs/speaker-id/speakers.json) del
// progetto standalone speaker-id: le voci registrate con quel tool sono già
// utilizzabili qui, nessun enrollment separato necessario.
//
// NB: sherpa-rs usa `eyre` internamente per i propri errori, non `anyhow`;
// li convertiamo qui al confine del modulo per restare coerenti col resto
// del progetto, che usa anyhow::Result ovunque.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sherpa_rs::speaker_id::{EmbeddingExtractor, ExtractorConfig};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const SAMPLE_RATE: u32 = 16_000;

static RECOGNIZER: OnceLock<Mutex<SpeakerRecognizer>> = OnceLock::new();
static MESSAGES: OnceLock<Value> = OnceLock::new();

/// Legge una chiave da other_messages di messages_it.json, con fallback se
/// init() non è ancora stato chiamato o la chiave manca dal json.
fn msg(key: &str, fallback: &str) -> String {
    MESSAGES
        .get()
        .and_then(|m| m["other_messages"][key].as_str())
        .unwrap_or(fallback)
        .to_string()
}

#[derive(Serialize, Deserialize, Default)]
struct SpeakerDb {
    speakers: HashMap<String, Vec<f32>>,
}

impl SpeakerDb {
    fn path() -> PathBuf {
        let mut p = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()));
        p.push(".local/share/assistente-rs/speaker-id/speakers.json");
        p
    }

    fn load() -> Self {
        let path = Self::path();
        match fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }
}

struct SpeakerRecognizer {
    extractor: EmbeddingExtractor,
}

impl SpeakerRecognizer {
    fn new(model_path: &str) -> Result<Self> {
        let config = ExtractorConfig {
            model: model_path.to_string(),
            provider: None,
            num_threads: Some(2),
            debug: false,
        };
        let extractor = EmbeddingExtractor::new(config)
            .map_err(|e| anyhow::anyhow!("inizializzazione modello speaker embedding fallita: {e}"))?;
        Ok(Self { extractor })
    }

    fn embed(&mut self, samples: &[f32]) -> Result<Vec<f32>> {
        self.extractor
            .compute_speaker_embedding(samples.to_vec(), SAMPLE_RATE)
            .map_err(|e| anyhow::anyhow!("calcolo embedding speaker fallito: {e}"))
    }

    fn identify(&mut self, samples: &[f32], threshold: f32) -> Option<String> {
        let emb = match self.embed(samples) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("⚠️ {}", msg("speaker_id_error", "speaker_id: {error}").replace("{error}", &e.to_string()));
                return None;
            }
        };
        let db = SpeakerDb::load();
        db.speakers
            .iter()
            .map(|(name, e)| (name.clone(), cosine_sim(&emb, e)))
            .filter(|(_, sim)| *sim >= threshold)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .map(|(name, _)| name)
    }
}

fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Inizializza il modello una sola volta all'avvio (chiamato da main.rs).
/// `messages`: contenuto di messages_it.json, usato per i log diagnostici
/// di questo modulo (chiavi other_messages.speaker_id_*), così cambiare
/// lingua non richiede toccare il codice.
/// Se il path è invalido o il modello non carica, stampa un errore e
/// identify_speaker() resterà silenziosamente disattivo (ritorna sempre None).
pub fn init(model_path: &str, messages: &Value) {
    let _ = MESSAGES.set(messages.clone());
    match SpeakerRecognizer::new(model_path) {
        Ok(rec) => {
            let _ = RECOGNIZER.set(Mutex::new(rec));
            let m = msg("speaker_id_active", "Speaker identification attiva ({model})")
                .replace("{model}", model_path);
            println!("🗣️ {}", m);
        }
        Err(e) => {
            let m = msg(
                "speaker_id_init_failed",
                "speaker_id::init fallito, riconoscimento parlante disattivo: {error}",
            )
            .replace("{error}", &e.to_string());
            eprintln!("⚠️ {}", m);
        }
    }
}

/// Vero se il modello è stato caricato con successo (init() riuscita).
/// Usato per evitare di proporre l'enrollment automatico quando
/// speaker-id non è configurato/attivo su questa macchina.
pub fn is_active() -> bool {
    RECOGNIZER.get().is_some()
}

/// Identifica il parlante dal buffer f32 mono a 16kHz della frase appena
/// captata dalla VAD. Ritorna None se non inizializzato, se nessuna voce
/// supera la soglia, o in caso di errore.
pub fn identify_speaker(samples: &[f32], threshold: f32) -> Option<String> {
    let mutex = RECOGNIZER.get()?;
    let mut rec = mutex.lock().ok()?;
    rec.identify(samples, threshold)
}

/// Registra una nuova voce nel db condiviso con il tool standalone
/// speaker-id, a partire dal buffer f32 mono a 16kHz di un comando già
/// pronunciato (non serve un enrollment dedicato separato).
pub fn enroll(name: &str, samples: &[f32]) -> Result<()> {
    let mutex = RECOGNIZER
        .get()
        .ok_or_else(|| anyhow::anyhow!("speaker_id non inizializzato"))?;
    let mut rec = mutex
        .lock()
        .map_err(|_| anyhow::anyhow!("lock speaker_id avvelenato"))?;
    let emb = rec.embed(samples)?;

    let mut db = SpeakerDb::load();
    db.speakers.insert(name.to_string(), emb);

    let path = SpeakerDb::path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(&db)?)?;
    Ok(())
}
