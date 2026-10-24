//! Cœur de Derush : tout ce qui ne dépend pas de l'interface.
//!
//! - [`model`] : le projet (rushes, segments, calques), sérialisé en JSON.
//!   C'est la source de vérité partagée par l'UI, le CLI et, plus tard, le serveur MCP.
//! - [`media`] : appels à ffprobe / ffmpeg (sonde, décodage PCM).
//! - [`analysis`] : détection des silences sur le niveau sonore.
//! - [`retakes`] : détection des prises refaites à partir d'une transcription.
//! - [`render`] : construction du graphe ffmpeg et export MP4.

pub mod analysis;
pub mod media;
pub mod model;
pub mod paths;
pub mod render;
pub mod retakes;
pub mod text;
pub mod transcribe;

pub use model::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("ffmpeg/ffprobe introuvable ou en échec : {0}")]
    Tool(String),
    #[error("fichier illisible : {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON invalide : {0}")]
    Json(#[from] serde_json::Error),
    #[error("projet invalide : {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;
