//! Serveur MCP de Derush (transport stdio, JSON-RPC 2.0, un message par ligne).
//!
//! Claude travaille sur le même fichier projet que l'app : chaque outil relit le projet,
//! le modifie, le réenregistre, et l'app recharge le fichier dès qu'il change.
//! Par défaut, le projet est celui ouvert en dernier dans l'app.

mod tools;

use std::io::{BufRead, Write};

use serde_json::{json, Value};

const SERVER_NAME: &str = "derush";
const DEFAULT_PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "\
Derush est le logiciel de montage YouTube de l'utilisatrice. Tu modifies son projet ouvert dans l'app.

Notions :
- Les rushes (src-0, src-1…) sont mis bout à bout ; la vidéo montée est la suite des parties gardées.
- Deux temps coexistent : le temps d'un rush (« src-0 1:02.3 ») et le temps de la vidéo montée (« montage 0:48.1 »). \
Les coupes se font en temps de rush ; titres et images peuvent se placer avec l'un ou l'autre.
- La transcription (get_transcript, search_transcript) donne les deux temps pour chaque phrase : \
pars d'elle pour retrouver un passage plutôt que de deviner des instants.

Bonnes pratiques :
- Commence par get_project. Si rien n'est transcrit, propose transcribe.
- Après une modification, résume ce que tu as changé ; l'utilisatrice vérifie dans l'app avant d'exporter.
- export_video et transcribe tournent en tâche de fond : suis-les avec job_status.
- Pour trouver les fichiers, utilise list_media (sans dossier : survol des dossiers habituels).
- Tu ne vois pas la vidéo et n'entends pas le son : appuie-toi sur la transcription, regarde avec get_frames \
avant de placer un titre ou une image, et propose un preview à l'utilisatrice avant export_video.";

fn send(msg: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{msg}");
    let _ = out.flush();
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn handle(ctx: &tools::Ctx, msg: &Value) -> Option<Value> {
    // Les notifications (sans id) et les réponses du client n'appellent pas de réponse.
    let id = msg.get("id")?.clone();
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        return None;
    };
    let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or(DEFAULT_PROTOCOL),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "title": "Derush", "version": env!("CARGO_PKG_VERSION") },
            "instructions": INSTRUCTIONS,
        }),
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools::definitions() }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match tools::call(ctx, name, &args) {
                Ok(content) => json!({ "content": content }),
                // Erreur d'outil : renvoyée à Claude comme résultat, pour qu'il puisse corriger.
                Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
            }
        }
        _ => return Some(error(id, -32601, &format!("méthode inconnue : {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn main() {
    let mut project = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--project" => project = args.next().map(Into::into),
            "--help" | "-h" => {
                eprintln!("derush-mcp [--project fichier.derush.json]\nSans --project : projet ouvert en dernier dans Derush.");
                return;
            }
            _ => {}
        }
    }
    let ctx = tools::Ctx::new(project);
    eprintln!("derush-mcp prêt");
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let out: Vec<Value> = batch.iter().filter_map(|m| handle(&ctx, m)).collect();
                if !out.is_empty() {
                    send(&Value::Array(out));
                }
            }
            Ok(msg) => {
                if let Some(r) = handle(&ctx, &msg) {
                    send(&r);
                }
            }
            Err(e) => send(&error(Value::Null, -32700, &format!("JSON invalide : {e}"))),
        }
    }
}
