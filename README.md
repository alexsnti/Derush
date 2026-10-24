# Derush

Petit logiciel de montage pour vidéos YouTube face caméra : il repère les blancs et les prises ratées, on valide les coupes à l'écran (sur la timeline ou dans le texte), on ajoute titres, images, musique, intro et outro, et on exporte un MP4 prêt à publier, avec sous-titres et chapitres. Claude peut piloter le montage grâce au serveur MCP intégré.

Le principe : la timeline est une suite de segments gardés ou coupés, et l'habillage est accroché à des instants des rushes, donc il reste au bon endroit quand les coupes changent. Tout le projet tient dans un fichier JSON lisible (`*.derush.json`).

## Installation sur Mac (M1)

```sh
# Outils
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
brew install ffmpeg node

# Lancer en développement
cd app
npm install
npm run tauri dev
```

Pour une app installable (`Derush.app` + `.dmg`) :

```sh
cd app
npx tauri icon src-tauri/icons/icon.png   # génère les icônes Mac à partir du PNG
npm run tauri build
```

En développement, l'app utilise le ffmpeg de Homebrew. L'app distribuée embarque le sien (voir ci-dessous).

## Installer sur un autre Mac (sans Xcode)

Le Mac qui installe l'app n'a besoin de rien : ni Xcode, ni Homebrew, ni Rust. Seule la machine qui *construit* l'app en a besoin.

### 1. Construire le `.dmg`

**Avec GitHub Actions** (aucun Mac de compilation nécessaire) : pousser le projet sur un dépôt GitHub, puis *Actions › App Mac › Run workflow*. Au bout de 15 à 20 minutes, le `.dmg` est dans les artefacts de l'exécution (« Derush-mac »).

**Ou sur un Mac de compilation** (Rust, Node et `xcode-select --install`, pas besoin de Xcode complet) :

```sh
./scripts/build-mac.sh
# → target/release/bundle/dmg/Derush_0.1.0_aarch64.dmg
```

Le script télécharge un ffmpeg et un ffprobe statiques ([builds de Martin Riedl](https://ffmpeg.martin-riedl.de/)), vérifie qu'ils ne dépendent pas de Homebrew, compile le serveur MCP et embarque le tout dans `Derush.app`. Si ce ffmpeg n'a pas l'encodeur matériel Apple, l'export bascule tout seul en x264 (plus lent, même résultat).

### 2. Installer

1. Ouvrir le `.dmg` et glisser Derush dans Applications.
2. L'app n'est pas signée par un compte développeur Apple : au premier lancement, macOS la bloque. Le plus simple est d'ouvrir le Terminal une fois et de taper :
   ```sh
   xattr -dr com.apple.quarantine /Applications/Derush.app
   ```
   Autre solution : tenter d'ouvrir l'app, puis *Réglages Système › Confidentialité et sécurité › Ouvrir quand même*.
3. À la première transcription, l'app télécharge le modèle Whisper (≈ 550 Mo, une seule fois).

Pour éviter l'étape 2, il faut un compte Apple Developer (99 € par an) : l'app peut alors être signée et notariée, et s'ouvre comme n'importe quelle autre.

### 3. Brancher Claude Desktop

Le serveur MCP est dans l'app. Dans *Réglages › Développeur › Modifier la configuration* :

```json
{
  "mcpServers": {
    "derush": { "command": "/Applications/Derush.app/Contents/MacOS/derush-mcp" }
  }
}
```

Puis redémarrer Claude Desktop. Si les rushes sont dans Bureau, Documents ou Téléchargements, macOS demandera d'autoriser Claude à accéder à ces dossiers (pas pour Films ni Musique).

## Utilisation

1. **Importer des rushes** (ou les glisser sur la fenêtre) : l'analyse coupe les blancs.
2. **Vérifier** : lecture en mode « Monté » (les coupes sont sautées) ou « Rush brut ».
   - Double-clic sur une zone de la timeline : la garder ou la couper.
   - Glisser pour sélectionner, puis `C` pour couper ou `G` pour garder.
   - `Espace` : lecture · flèches : ±1 s (±5 s avec Maj) · `⌘S` : enregistrer.
3. **Texte** : transcription locale (Whisper, accéléré par la puce M1, rien n'est envoyé en ligne).
   - Le modèle se télécharge une fois depuis l'onglet (≈ 550 Mo).
   - « Chercher » les prises ratées : phrases recommencées juste après (on garde la dernière), « je recommence », « non attends »…
   - Clic sur une phrase pour y aller, ✕ pour la couper, ↺ pour la remettre.
4. **Habillage** : titres et images ajoutés à la position de lecture, musique de fond (baissée automatiquement sous la voix), intro et outro.
5. **Exporter pour YouTube** : MP4 H.264 (encodeur matériel de la puce Apple par défaut), son normalisé à −14 LUFS, petit fondu à chaque coupe. L'onglet Export génère aussi les sous-titres `.srt` et les chapitres (à partir des titres).

Une fois enregistré, le projet s'enregistre tout seul à chaque modification.

## Piloter le montage avec Claude (MCP)

Avec l'app installée, voir « Brancher Claude Desktop » ci-dessus. En développement, pointer plutôt sur `target/release/derush-mcp` (après `cargo build --release -p derush-mcp`).

Le serveur travaille sur **le projet ouvert en dernier dans Derush** (il faut l'avoir enregistré une fois). Ses modifications apparaissent dans l'app en une à deux secondes ; si des changements non enregistrés sont en cours dans l'app, elle demande quelle version garder.

### Tout faire depuis Claude

L'app n'est pas indispensable : Claude peut faire tout le montage, et l'app sert d'écran de vérification pour les retouches fines. Une séance type :

1. « Mes rushes du tournage d'hier sont dans Films. Prépare le montage. » → `list_media`, `create_project`, `transcribe`
2. « Coupe les prises ratées, mets un titre pour chaque tissu, mon logo en haut à droite au début, et la musique douce. » → `find_retakes`, `get_transcript`, `add_title`, `add_image`, `set_music`, en vérifiant avec `get_frames`
3. « Montre-moi. » → `preview` ouvre un aperçu basse résolution dans QuickTime
4. « À 2:13, remets la phrase sur la lisière. Puis exporte dans Films. » → `keep`, `export_video`, `write_subtitles`, `get_chapters`

Claude ne voit pas la vidéo en direct et n'entend pas le son : il s'appuie sur la transcription, regarde des images du montage quand il place un titre ou une image (`get_frames`), et il faut regarder l'aperçu avant l'export.

Autres exemples :
- « Coupe les prises ratées et dis-moi ce que tu as enlevé. »
- « Mets un titre "Le lin lavé" quand je commence à parler du lin, et un autre pour chaque tissu. »
- « Enlève le passage où je parle de la livraison. »
- « Donne-moi les chapitres YouTube et exporte la vidéo dans ~/Movies/haul.mp4. »

Outils exposés (23) :

| Usage | Outils |
|---|---|
| Fichiers | `list_media`, `create_project` |
| Lire le montage | `get_project`, `get_transcript`, `search_transcript`, `get_frames` |
| Couper | `cut`, `keep`, `find_retakes` |
| Habillage | `add_title`, `add_image`, `update_overlay`, `remove_overlay`, `set_music`, `set_intro`, `set_outro` |
| Sortie | `preview`, `export_video`, `write_subtitles`, `get_chapters`, `set_export` |
| Suivi | `transcribe`, `job_status` |

La transcription, l'export et les aperçus longs tournent en tâche de fond (suivis avec `job_status`), pour ne pas bloquer Claude.

## Structure

```
crates/core   Modèle, analyse audio, transcription, prises ratées, sous-titres, export ffmpeg (aucune dépendance à l'UI)
crates/cli    Outil en ligne de commande, pour régler l'analyse et déboguer l'export
crates/mcp    Serveur MCP pour Claude (stdio)
app/          App Tauri : src-tauri/ (commandes Rust) et src/ (interface TypeScript)
```

Le CLI est pratique pour caler les réglages sur de vrais rushes :

```sh
cargo run -p derush-cli -- analyze rush1.mov rush2.mov -o projet.derush.json --min-silence 0.5
cargo run --release -p derush-cli -- transcribe projet.derush.json --cut-retakes
cargo run -p derush-cli -- info projet.derush.json
cargo run -p derush-cli -- plan projet.derush.json      # graphe ffmpeg généré, sans encoder
cargo run -p derush-cli -- render projet.derush.json -o video.mp4
```

Tests : `cargo test` (le test de bout en bout génère un rush avec ffmpeg, l'analyse et l'exporte).
Test de la transcription sur un vrai fichier :

```sh
DERUSH_TEST_MODEL=~/Library/Application\ Support/Derush/models/ggml-large-v3-turbo-q5_0.bin \
DERUSH_TEST_AUDIO=rush.mov cargo test --release -p derush-core real_model -- --ignored --nocapture
```

## Feuille de route

- [x] Détection des blancs (niveau RMS, seuil adaptatif, marges) et validation à l'écran
- [x] Titres, images, musique avec ducking, intro/outro, loudness YouTube
- [x] Transcription Whisper locale (Metal), prises ratées, montage par le texte
- [x] Sous-titres `.srt` et chapitres YouTube
- [x] Serveur MCP, avec synchronisation en direct avec l'app
- [ ] VAD Silero à la place du seuil d'énergie (plus robuste avec une machine à coudre ou de la musique)
- [ ] Proxies 720p générés à l'import, pour une lecture fluide des rushes 4K/HEVC
- [x] App autonome (ffmpeg et serveur MCP embarqués), `.dmg` construit par GitHub Actions
- [ ] Incrustations vidéo (pas seulement des images)
