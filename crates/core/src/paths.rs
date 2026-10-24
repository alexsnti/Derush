//! Emplacements partagés par l'app, le CLI et le serveur MCP.
//!
//! - modèles de transcription : `<données>/models/`
//! - projet ouvert dans l'app : `<données>/current-project`, pour que le serveur MCP
//!   travaille sur le bon fichier sans qu'on ait à lui donner le chemin.

use std::path::{Path, PathBuf};

/// `~/Library/Application Support/Derush` sur Mac, `~/.local/share/derush` sous Linux.
/// Surchargeable avec `DERUSH_HOME`.
pub fn data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("DERUSH_HOME") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| ".".into());
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Derush")
    } else if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(x).join("derush")
    } else {
        home.join(".local/share/derush")
    }
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

fn current_file() -> PathBuf {
    data_dir().join("current-project")
}

/// Mémorise le projet ouvert dans l'app.
pub fn set_current_project(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir())?;
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    std::fs::write(current_file(), abs.to_string_lossy().as_bytes())
}

/// Projet ouvert dans l'app, s'il existe encore.
pub fn current_project() -> Option<PathBuf> {
    let p = PathBuf::from(std::fs::read_to_string(current_file()).ok()?.trim());
    p.exists().then_some(p)
}
