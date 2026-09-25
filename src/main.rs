//Assistente vocale in italiano - conversione in linguaggio RUST
//2026 - Masiero Riccardo - tecnomas.engineering@gmail.com

mod config;
mod tts;
mod radio;
mod system;
mod intent;
mod ui;
mod vocalrecon;
mod audio_filter;
mod ai;
mod speaker_id;

use vocalrecon::listen_for_command;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use anyhow::Result;



fn select_thread_count() -> i32 {
    std::thread::available_parallelism()
    .map(|n| n.get() as i32)
    .unwrap_or(4)
    .min(8)
}

/// Controlla la presenza dei file di modello attesi (whisper, piper,
/// speaker-id) e stampa un avviso chiaro per ciascuno mancante. Non blocca
/// l'avvio: whisper/piper restano obbligatori a runtime e falliranno comunque
/// più avanti con errore proprio, ma qui l'utente capisce subito la causa
/// invece di un errore generico; speaker-id resta opzionale e si disattiva
/// silenziosamente se il modello/db manca (vedi speaker_id::init).
fn check_assets(whisper_path: &str, piper_model: Option<&str>, speaker_model: &str) {
    if !std::path::Path::new(whisper_path).exists() {
        eprintln!("⚠️  Modello Whisper non trovato: {}", whisper_path);
        eprintln!("    Copialo in ~/.local/share/assistente-rs/whisper/");
    }

    match piper_model {
        Some(path) if !std::path::Path::new(path).exists() => {
            eprintln!("⚠️  Voce Piper non trovata: {}", path);
            eprintln!("    Copiala in ~/.local/share/assistente-rs/piper/");
        }
        _ => {}
    }

    if speaker_model.is_empty() {
        println!("ℹ️  ASSISTENTE_SPEAKER_ID_MODEL non impostata: riconoscimento del parlante disattivo.");
    } else if !std::path::Path::new(speaker_model).exists() {
        eprintln!("⚠️  Modello speaker-id non trovato: {}", speaker_model);
        eprintln!("    Copialo in ~/.local/share/assistente-rs/speaker-id/");
    } else {
        let home = std::env::var("HOME").unwrap_or_default();
        let db_path = format!("{}/.local/share/assistente-rs/speaker-id/speakers.json", home);
        if !std::path::Path::new(&db_path).exists() {
            println!("ℹ️  Nessuna voce registrata ({}): nessuno verrà riconosciuto finché non fai l'enrollment.", db_path);
        }
    }
}


fn main() -> Result<()> {
    dotenv::from_path(config::env_path()).ok();
    let config = config::load_config()?;
    let messages = config::load_messages()?;
    let messages_json = serde_json::to_value(&messages)?;

    let sleep_time = Duration::from_secs(config.sleep_time);
    let botname    = config.botname.clone();

    let model_path = vocalrecon::select_whisper_model(&config);
    let threads = select_thread_count();

    let speaker_model = config.speaker_id_model.clone().unwrap_or_default();
    check_assets(&model_path, config.piper_model.as_deref(), &speaker_model);

    std::env::set_var("ASSISTENTE_WHISPER_MODEL", &model_path);
    std::env::set_var("ASSISTENTE_WHISPER_THREADS", threads.to_string());

    tts::init_piper_paths(config.piper_bin.clone(), config.piper_model.clone());

    // Speaker identification: opzionale, attiva solo se è impostato il path
    // del modello ONNX. Nessun impatto se la variabile non è definita.
    if !speaker_model.is_empty() {
        speaker_id::init(&speaker_model, &messages_json);
    }


    audio_filter::init_audio_filter();   // <-- prima di warmup_audio
    std::thread::sleep(Duration::from_millis(400)); // lascia assestare PipeWire dopo il cambio di routing

    let stt_mode = vocalrecon::detect_stt_mode();
    vocalrecon::warmup_audio();

    // Whisper si precarica all'avvio SOLO se si parte offline (pronto subito,
    // senza ritardo alla prima frase). Se si parte online (Groq/Google),
    // whisper non occupa RAM finché non serve davvero: si carica al volo,
    // lazy tramite get_context() (OnceLock), la prima volta che la
    // connessione cade durante l'uso e transcribe_whisper() viene chiamata.
    // Se la connessione torna, si torna a usare Groq/Google senza scaricare
    // whisper dalla RAM (resta caricato per il resto della sessione).
    if !vocalrecon::use_stt_online() {
        vocalrecon::warmup_whisper();
    }

    let api_key = std::env::var("API_KEY_GROQ").unwrap_or_default();
    let yt_key  = std::env::var("API_KEY_YOUTUBE").unwrap_or_default();

    let tts_mode = tts::detect_tts_mode(vocalrecon::use_stt_online());

    std::env::set_var("RUST_LOG", "cpal=warn");
    env_logger::init();


    // Necessario su Linux/KDE per QtCore.Settings e rendering corretto
    // Equivalente Python: os.environ["QT_QPA_PLATFORM"] = "xcb"
    if std::env::var("QT_QPA_PLATFORM").is_err() {
        std::env::set_var("QT_QPA_PLATFORM", "xcb");
    }
    std::env::set_var("QT_XCB_GL_INTEGRATION", "xcb_egl");

    // Modalità --note TESTO: apre solo la finestra note e termina
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--note" {
        let testo = args[2..].join(" ");
        ui::avvia_finestra_note(&testo);
        return Ok(());
    }


    // ── Genera listaprogrammi e bookmarks in background ───────────────────
    thread::spawn(|| {
        if let Err(e) = system::generate_programs_list("data/listaprogrammi") {
            eprintln!("⚠️  Errore generazione listaprogrammi: {}", e);
        }
        if let Err(e) = system::generate_bookmarks_list("data/bookmarks") {
            eprintln!("⚠️  Errore generazione bookmarks: {}", e);
        }
        println!("✅ Lista programmi e bookmarks aggiornati.");
    });

    // ── Canali di comunicazione ───────────────────────────────────────────
    // tx_cmd/rx_cmd ora portano anche il buffer audio della frase (per
    // speaker_id::identify_speaker), non solo il testo trascritto.
    let (tx_cmd, rx_cmd)       = mpsc::channel::<(String, Option<Vec<f32>>)>();
    let (tx_output, rx_output) = mpsc::channel::<String>();
    let attivo_flag            = Arc::new(Mutex::new(false));

    vocalrecon::start_connection_monitor(tx_output.clone());

    tts::init_ui_output(tx_output.clone(), botname.clone());


    // ── Messaggi diagnostici: quale motore STT/TTS è in uso ────────────────
    let _ = tx_output.send(format!("🎙️ Riconoscimento vocale: {}", stt_mode));
    let _ = tx_output.send(format!("🔊 Sintesi vocale: {}", tts_mode));

    // Messaggio di avvio completato (testo da messages_it.json)
    let msg_avviato = messages_json["other_messages"]["bot_started"]
    .as_str().unwrap_or("[missing: other_messages.bot_started]")
    .replace("{botname}", &botname);
    println!("🤖 {}", msg_avviato);
    let _ = tx_output.send(format!("🤖 {}", msg_avviato));

    // Messaggi iniziali verso la GUI (testi da messages_it.json)
    let _ = tx_output.send(
        messages_json["other_messages"]["waiting_wakeword"]
        .as_str().unwrap_or("In ascolto...")
        .replace("{botname}", &botname)
    );

    // Cloni per i thread
    let tx_cmd_stt   = tx_cmd.clone();
    let tx_output_lp = tx_output.clone();
    let tx_output_stt = tx_output.clone();
    let attivo_lp    = attivo_flag.clone();
    let api_key_stt  = api_key.clone();


    // ── Thread STT: ascolto microfono ─────────────────────────────────────
    thread::spawn(move || {
        loop {
            match listen_for_command(&api_key_stt, &tx_output_stt) {
                Ok((text, buffer_audio)) if !text.is_empty() => {
                    if tts::is_speaking() {
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }

                    //println!("🎤 Riconosciuto: '{}'", text); // Disattivato ma da riattivare se serve per test
                    //let _ = tx_output_stt.send(format!("🎤 Riconosciuto: {}", text)); disattivato per l'interfaccia UI '
                    let _ = tx_cmd_stt.send((text, buffer_audio));
                }
                Err(e) => eprintln!("⚠️  STT errore: {}", e),
                  _ => {}
            }
            thread::sleep(Duration::from_millis(50));
        }
    });

    // ── Thread loop intent ────────────────────────────────────────────────
    let config_clone   = config.clone();
    let messages_clone = messages_json.clone();
    let messages_ui    = messages_json.clone();
    let api_key_intent = api_key.clone();
    let yt_key_intent  = yt_key.clone();

    thread::spawn(move || {
        let mut intent_handler = intent::IntentHandler::new(
            config_clone.wakeword.clone(),
                                                            config_clone.botname.clone(),
                                                            messages_clone,
                                                            std::path::PathBuf::from("data/listaprogrammi"),
                                                            std::path::PathBuf::from("data/bookmarks"),
                                                            std::path::PathBuf::from("data/stations.csv"),
                                                            tx_output_lp.clone(),
        );

        let api_key = api_key_intent;
        let yt_key  = yt_key_intent;
        let mut last_activity = Instant::now();

        loop {
            // Controlla inattività → stand-by
            if intent_handler.active
                && !intent_handler.awaiting_shutdown
                && !intent_handler.awaiting_reboot
                && last_activity.elapsed() >= sleep_time
                {
                    intent_handler.active              = false;
                    *attivo_lp.lock().unwrap()         = false;
                    let msg_testo = messages_ui["other_messages"]["standby_message"]
                    .as_str().unwrap_or("[missing: other_messages.standby_message]")
                    .replace("{botname}", &config_clone.botname);
                    let msg = format!("💤 {}", msg_testo);
                    println!("{}", msg);
                    let _ = tx_output_lp.send(msg);
                    let _ = tx_output_lp.send(
                        messages_ui["other_messages"]["waiting_wakeword"]
                        .as_str()
                        .unwrap_or("In ascolto...")
                        .replace("{botname}", &config_clone.botname)
                    );
                }

                match rx_cmd.recv_timeout(Duration::from_millis(500)) {
                    Ok((received, buffer_audio)) => {
                        if !intent_handler.active {
                            let received_lower = received.to_lowercase();
                            let wakeword_lower = config_clone.wakeword.to_lowercase();

                            if received_lower.contains(&wakeword_lower) {

                                // Aggiorna SUBITO la UI
                                *attivo_lp.lock().unwrap() = true;

                                println!("🤖 {} attivato", config_clone.botname);
                            }
                        }

                        // Aspetta fine TTS + cooldown anti-echo
                        while tts::is_speaking() {
                            thread::sleep(Duration::from_millis(50));
                        }
                        thread::sleep(Duration::from_millis(400));

                        let era_attivo = intent_handler.active;
                        let elapsed_da_ultima_attivita = last_activity.elapsed().as_secs_f64();

                        // Comando da GUI
                        if received.starts_with("__GUI__") {
                            intent_handler.active = true;
                            *attivo_lp.lock().unwrap() = true;
                        }

                        // Gestione effettiva del comando
                        let _ = intent_handler.comrecon(&received, &api_key, &yt_key, buffer_audio.as_deref());

                        // Sincronizza lo stato finale con la UI
                        *attivo_lp.lock().unwrap() = intent_handler.active;

                        if intent_handler.active || era_attivo {
                            last_activity = Instant::now();
                        }

                        if intent_handler.active {
                            let msg_testo = messages_ui["other_messages"]["active_message"]
                            .as_str().unwrap_or("[missing: other_messages.active_message]")
                            .replace("{botname}", &config_clone.botname)
                            .replace("{elapsed:.0}", &format!("{:.0}", elapsed_da_ultima_attivita))
                            .replace("{sleep_time}", &sleep_time.as_secs().to_string());
                            let msg = format!("✅ {}", msg_testo);
                            println!("{}", msg);
                        }
                    }

                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        eprintln!("❌ Canale STT disconnesso.");
                        break;
                    }
                }
        }
    });

    // ── GUI Qt/QML tramite qmetaobject (blocca il thread principale) ─────────
    let config_json_path = config::config_path("config.json");

    ui::avvia_gui(
        &config.layout,
        config_json_path.to_str().unwrap_or("config/config.json"),
                  &botname,
                  tx_cmd,
                  rx_output,
                  attivo_flag,
    );

    Ok(())
}
