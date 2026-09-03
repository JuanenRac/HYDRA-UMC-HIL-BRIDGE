<p align="center">
  <img src="images/HYDRA_UMC_BANNER.svg" alt="HYDRA-UMC-HIL-BRIDGE banner" width="100%">
</p>

# 🌉 HYDRA-UMC-HIL-BRIDGE

<p align="center"><a href="README.md">🇺🇸 English</a> | <a href="README_spa.md">🇪🇸 Español</a> | 🇫🇷 <b>Français</b> | <a href="README_ita.md">🇮🇹 Italiano</a> | <a href="README_deu.md">🇩🇪 Deutsch</a> | <a href="README_zho.md">🇨🇳 简体中文</a> | <a href="README_jpn.md">🇯🇵 日本語</a></p>

### ⚡ Interface Hardware-in-the-Loop pour la synchronisation réel vs virtuel

<p align="left">
  <img src="https://img.shields.io/badge/Licence-GPL%203.0-blue.svg" alt="GPL 3.0">
  <img src="https://img.shields.io/badge/Protocole-WebSocket%20%2F%20gRPC-yellow.svg" alt="Protocol">
  <img src="https://img.shields.io/badge/Fonction-Sync%20latence%20zéro-green.svg" alt="Sync">
  <img src="https://img.shields.io/badge/%C3%89tape-%C3%89tabli%20v0-brightgreen.svg" alt="Étape établi v0">
</p>

---

## 1. 🛠️ APERÇU TECHNIQUE

**HYDRA-UMC-HIL-BRIDGE** est l'artère de communication qui active la fonctionnalité Hardware-in-the-Loop (HIL). Il synchronise l'état entre les contrôleurs physiques et le moteur du jumeau numérique (Digital Twin) en temps réel.

Il permet aux développeurs d'envoyer des commandes depuis n'importe quelle interface (App, Suite, Studio) au simulateur comme s'il s'agissait d'un robot physique, et inversement, il peut refléter les mouvements d'un robot physique dans le monde virtuel pour une supervision à distance et un suivi (shadowing) du jumeau numérique.

### Caractéristiques principales :
* 🛡️ **Verrouillage de sécurité (v0) :** le vrai sous-commande `route` bloque une commande destinée au matériel réel dès qu'un rapport de risque du jumeau signale une collision imminente - voir « Vérification d'honnêteté » ci-dessous pour ce qui fonctionne exactement aujourd'hui.
* 🌉 **Pont bidirectionnel (v0, sans transport pour l'instant) :** un vrai acheminement basé sur le mode (Réel/Simulation) et une vraie mise en miroir inconditionnelle existent ; il n'y a encore aucune vraie connexion gRPC/WebSocket vers un vrai HYDRA-UMC-TWIN ou contrôleur HYDRA-UMC.
* ⚡ **Mise en miroir sans latence (partiel) :** le vrai sous-commande `mirror` reflète aujourd'hui une commande vers un récepteur en mémoire ; la « latence zéro » sur une vraie connexion réseau reste du travail futur.
* 📡 **Protocole unifié (prévu) :** utilise gRPC pour une synchronisation locale haute vitesse et les WebSockets pour la surveillance à distance - aucun transport réseau n'est encore branché, volontairement (voir `Cargo.toml`).
* 🧪 **Transport à sécurité intrinsèque, testable sans matériel (v0) :** `CommandSink::send()` renvoie désormais un vrai `Result`, et un `SimulatedTransport` peut simuler une liaison en timeout ou déconnectée ; le pont rapporte un résultat `TransportFailure` distinct plutôt que de jamais prétendre qu'une commande a été livrée alors qu'elle ne l'a pas été - exerçable via `--transport-latency-ms`/`--transport-timeout-ms`/`--transport-disconnected` sur `route`.

**Vérification d'honnêteté - ce qui fonctionne réellement aujourd'hui :** `route --mode real|simulation --joint NOM --position VALEUR [--collision-risk] [--distance MÈTRES] [--transport-latency-ms MS] [--transport-timeout-ms MS] [--transport-disconnected]` prend une vraie décision d'acheminement - le mode `simulation` envoie toujours, le mode `real` est soumis à un vrai verrouillage de sécurité qui bloque la commande dès que `--collision-risk` est indiqué. `mirror --joint NOM --position VALEUR` reflète une commande inconditionnellement. Les deux acheminent par défaut vers un récepteur en mémoire (`RecordingSink`, pas un vrai contrôleur ni une vraie instance HYDRA-UMC-TWIN), ou vers un `SimulatedTransport` qui peut réellement échouer (timeout/déconnexion) quand les indicateurs de transport ci-dessus sont passés - il n'y a pas encore de transport gRPC/WebSocket. Voir [`CHANGELOG.md`](CHANGELOG.md) pour ce qui a été livré exactement, et la Roadmap ci-dessous pour ce qui reste à venir.

---

## 2. 🔄 FLUX DE SYNCHRONISATION HIL

La répartition par mode au niveau de `BRIDGE` et la porte de verrouillage
de sécurité sur le chemin `Mode Réel` sont réelles aujourd'hui
(`bridge.rs`/`interlock.rs`), acheminant vers un récepteur en mémoire
plutôt qu'un vrai processus en direct. Tout ce qui touche un vrai
processus `APP`, `TWIN` ou `CORE` via une vraie connexion reste du
travail futur.

```mermaid
flowchart LR
    APP["Interface de contrôle - prévu"] --> BRIDGE["HIL-BRIDGE - acheminement réel v0"]
    BRIDGE -- Mode Simulation --> TWIN["HYDRA-UMC-TWIN - prévu"]
    BRIDGE -- "Mode Réel (verrouillé - réel v0)" --> CORE["Cœur HYDRA-UMC (STM32) - prévu"]
    CORE -- Retour --> BRIDGE
    TWIN -- Retour --> BRIDGE
    BRIDGE --> APP
```

---

## 3. 🧱 ARCHITECTURE & DÉCISIONS DE CONCEPTION

* **Pourquoi ce pont n'a pas de dossiers `hardware/`/`firmware/`/`os/`.** Logiciel pur - il relie du matériel déjà existant (vraies applications, vrai firmware HYDRA-UMC) au jumeau numérique, sans carte propre.
* **Pourquoi c'est un frère, pas un sous-module, de HYDRA-UMC-TWIN.** Un pont hardware-in-the-loop doit continuer de fonctionner (et continuer de faire circuler les commandes d'un vrai appareil) indépendamment de ce que fait à cet instant la propre boucle rendu/physique de HYDRA-UMC-TWIN - un processus séparé signifie qu'un redémarrage du jumeau ne perd pas une vraie commande en cours.
* **Comment cela s'intègre dans le reste de l'écosystème.** Permet à HYDRA-UMC-SUITE, HYDRA-UMC-ANDROID-CONTROL et HYDRA-UMC-IOS-CONTROL de contrôler HYDRA-UMC-TWIN comme s'il s'agissait d'une vraie cellule adossée à HYDRA-UMC-SERVER - la même surface de contrôle, une cible simulée.
* **Pourquoi le verrouillage fait confiance au propre indicateur `collision_imminent` du jumeau plutôt que d'en dériver un à partir d'un seuil de distance.** Le jumeau a déjà effectué le vrai raisonnement géométrique/physique au moment où il signale un risque - remettre en question cette conclusion ici avec un seuil de distance indépendant ne serait qu'un second avis de sécurité, potentiellement incohérent. Voir la documentation propre du module `interlock.rs` pour la manière dont cela reflète la frontière détecter-vs-appliquer de HYDRA-UMC-SAFETY-ZONES.
* **Pourquoi le mode `Simulation` n'est jamais verrouillé.** Tout l'intérêt d'acheminer une commande vers le jumeau plutôt que vers le matériel réel est de pouvoir observer en toute sécurité une collision prédite se dérouler - la bloquer là annulerait la fonctionnalité même qu'elle est censée soutenir.
* **Pourquoi `CommandSink` est un trait avec seulement une implémentation `RecordingSink` en mémoire aujourd'hui.** Aucun vrai transport gRPC/WebSocket n'existe encore (voir le propre commentaire de `Cargo.toml`) - `RecordingSink` est honnête à ce sujet : il enregistre ce qu'on lui a demandé d'envoyer sans rien transmettre nulle part, le même raisonnement que `NullEStopRequester` dans HYDRA-UMC-SAFETY-ZONES.
* **Pourquoi `CommandSink::send()` renvoie un `Result` plutôt que `()`.** Un vrai transport peut échouer à livrer (timeout, déconnexion) indépendamment du fait que le verrouillage ait laissé passer la commande - si `send()` ne peut pas échouer, le pont n'a aucun moyen d'éviter de prétendre au succès pour une commande qui n'est jamais réellement arrivée. `SimulatedTransport` existe précisément pour que ce chemin d'échec soit réel et testable dès aujourd'hui, sans attendre qu'un vrai transport existe.
* **Pourquoi `TransportFailure` est un `RouteOutcome` distinct de `BlockedByInterlock`.** Ce sont deux types différents de « ne s'est pas produit » : l'un est un refus de sécurité délibéré (le verrouillage a décidé de ne pas transmettre), l'autre est une livraison au mieux qui ne s'est simplement pas terminée. Les fusionner en un échec générique masquerait quelle couche de sécurité a réellement arrêté la commande - utile pour le débogage et pour toute politique future qui les traiterait différemment (réessayer un échec de transport est raisonnable ; réessayer après un blocage du verrouillage ne l'est pas).

---

## 📂 STRUCTURE DES RÉPERTOIRES

Pont purement logiciel, sans conception matérielle propre ; ce projet ne
comporte donc pas de dossiers `hardware/`, `firmware/` ni `os/`, conformément à la politique de structure du dépôt.

```text
HYDRA-UMC-HIL-BRIDGE/
├── src/
│   ├── protocol.rs       # Vrais types JointCommand/Mode
│   ├── interlock.rs      # Vraie décision de verrouillage de sécurité
│   ├── bridge.rs         # Vrai acheminement basé sur le mode + mise en miroir + CommandSink/SimulatedTransport
│   ├── server.rs         # Surface JSON/HTTP simple (tiny_http, bloquant, sans runtime async)
│   └── main.rs           # Point d'entrée + vrais sous-commandes `route`/`mirror`
├── docs/                # Documentation et guides d'intégration
├── build/               # Notes/artefacts de build (la sortie réelle de cargo vit dans target/, ignoré par git)
├── images/              # Médias et diagrammes
├── systemd/
│   └── hydra-umc-hil-bridge.service # Unité systemd de l'API locale route/mirror sur la CM5
├── tools/
│   ├── build_test.py    # Vérification de build sans versionnage
│   └── ci_validate.py   # Validation manifeste/CHANGELOG/docs utilisée par CI
├── Cargo.toml           # Métadonnées du paquet, dépendances, version compteur kilométrique
├── bump_version.py      # Incrément de version native type compteur kilométrique (utilisé par build.sh/.bat)
├── bump_manifest_version.py # Synchronise la version de hydra-umc.project.json avec la version native (--sync)
├── build.sh / build.bat # Incrémente la version, `cargo test`, puis `cargo build --release`
├── build-test.sh / build-test.bat # Vérification de build sans versionnage
└── run.sh / run.bat     # Exécute le binaire release compilé (relaie les arguments)
```

---

## 🏗️ BUILD ET RUN

Nécessite la chaîne d'outils Rust (`cargo`/`rustc`, à installer via [rustup](https://rustup.rs)) et Python 3.10+ (uniquement pour `bump_version.py`).

```bash
# Linux / macOS
./build.sh   # incrément de version compteur kilométrique, `cargo test` (23 tests), puis `cargo build --release`
./run.sh     # exécute target/release/hydra-umc-hil-bridge, affiche nom + version + rôle
```

```bat
:: Windows
build.bat
run.bat
```

`build.sh`/`build.bat` incrémentent la version du propre `Cargo.toml` de ce projet selon la règle "compteur kilométrique" de l'écosystème (PATCH+1, avec retenue vers MINOR au-delà de 9), exécutent la vraie suite de tests, puis construisent un binaire release.

Les vrais sous-commandes `route` et `mirror` :

```bash
./run.sh route --mode real --joint shoulder --position 0.5
# SENT: routed to real HYDRA-UMC controller

./run.sh route --mode real --joint shoulder --position 0.5 --collision-risk --distance 0.02
# BLOCKED: safety interlock refused to forward to real hardware (twin reports imminent collision at 0.020m)

./run.sh route --mode simulation --joint shoulder --position 0.5 --collision-risk --distance 0.02
# SENT: routed to HYDRA-UMC-TWIN (simulation)

./run.sh mirror --joint elbow --position -0.3
# MIRRORED: joint 'elbow' = -0.300000 shadowed into the twin

./run.sh route --mode real --joint shoulder --position 0.5 --transport-timeout-ms 100 --transport-latency-ms 500
# TRANSPORT FAILURE: command was not confirmed delivered (transport timed out after 100ms)

./run.sh route --mode real --joint shoulder --position 0.5 --transport-disconnected
# TRANSPORT FAILURE: command was not confirmed delivered (transport is disconnected)
```

`route` se termine avec `0` (envoyé), `1` (bloqué par le verrouillage de sécurité - un vrai résultat significatif, pas une erreur), `2` (entrée invalide), ou `3` (échec de transport - le verrouillage a laissé passer la commande, mais la livraison n'a pas été confirmée). `mirror` se termine avec `0`, `2` ou `3`.

`Cargo.toml` ne comporte volontairement aucune crate externe pour l'instant - voir le commentaire dans le fichier pour ce qui sera ajouté quand le vrai travail de transport gRPC/WebSocket commencera.

---

## 🚀 FEUILLE DE ROUTE
* **Phase 1 :** Synchronisation du jumeau numérique avec la télémétrie matérielle en temps réel et latence inférieure à 10 ms.
* **Phase 2 :** Intégration de Physics Replica avec des simulateurs de classe industrielle (Isaac Sim) et prise en charge des corps déformables.
* **Phase 3 :** Modèles de récupération automatisés de Node Healing pour un basculement décentralisé et détection précoce de la dégradation des capteurs.
* **Phase 4 :** Synchronisation HIL multi-contrôleurs (Swarm HIL) et prise en charge de la génération de données synthétiques photoréalistes.

---

## 🔗 Projets Liés

Ce projet fait partie de l'écosystème robotique HYDRA-UMC du même auteur (JuanenRac / Electro Hobby 3D). Bon à savoir, car une demande pourrait en réalité concerner l'un de ceux-ci plutôt que ce dépôt.

**Projet Parent**
- **[HYDRA-UMC-TWIN](https://github.com/JuanenRac/HYDRA-UMC-TWIN)** — hub d'intégration pour le moteur de jumeau numérique, avec un vrai contrat de synchronisation par compatibilité de version ; le parent dont ce dépôt est un service de simulation spécifique, au sein de son propre moteur de jumeau numérique.

**Projets Frères** — les autres services de simulation du propre moteur de jumeau numérique de HYDRA-UMC-TWIN
- **[HYDRA-UMC-PHYSICS-REPLICA](https://github.com/JuanenRac/HYDRA-UMC-PHYSICS-REPLICA)** — vraie cinématique directe et validation des limites articulaires sur un vrai sous-ensemble URDF.
- **[HYDRA-UMC-SYNTHETIC-DATA-GEN](https://github.com/JuanenRac/HYDRA-UMC-SYNTHETIC-DATA-GEN)** — vrai générateur procédural de scènes 2D avec export d'annotations YOLO/COCO.

**Directement Liés**
- **[HYDRA-UMC-STUDIO](https://github.com/JuanenRac/HYDRA-UMC-STUDIO)** — tableau de bord de contrôle web avec visualisation 3D multi-robot en temps réel — l'une des 3 interfaces client pouvant envoyer des commandes via ce bridge comme s'il s'agissait d'un robot physique, une fois qu'un vrai transport existe.
- **[HYDRA-UMC-SUITE](https://github.com/JuanenRac/HYDRA-UMC-SUITE)** — centre de commande d'essaim de bureau (PySide6) pour plusieurs serveurs à la fois, empaqueté en exécutable autonome — l'une des 3 interfaces client pouvant envoyer des commandes via ce bridge comme s'il s'agissait d'un robot physique, une fois qu'un vrai transport existe.
- **[HYDRA-UMC-ANDROID-CONTROL](https://github.com/JuanenRac/HYDRA-UMC-ANDROID-CONTROL)** — application de contrôle Android native avec connexion biométrique et un compagnon Wear OS jumelé — l'une des 3 interfaces client pouvant envoyer des commandes via ce bridge comme s'il s'agissait d'un robot physique, une fois qu'un vrai transport existe.
- **[HYDRA-UMC-IOS-CONTROL](https://github.com/JuanenRac/HYDRA-UMC-IOS-CONTROL)** — application de contrôle iOS/iPadOS (Flutter) avec synchronisation WebSocket en temps réel — l'une des 3 interfaces client pouvant envoyer des commandes via ce bridge comme s'il s'agissait d'un robot physique, une fois qu'un vrai transport existe.
- **[HYDRA-UMC-SERVER](https://github.com/JuanenRac/HYDRA-UMC-SERVER)** — le vrai backend headless (REST/WebSocket) auquel parle réellement chaque client de contrôle — le backend derrière ces 3 interfaces client, le vrai contrôleur que le propre `route --mode real` de ce bridge cible finalement une fois qu'un transport existe.

**Fait Également Partie de l'Écosystème**

*Matériel & Plateforme de Base*
- **[HYDRA-UMC](https://github.com/JuanenRac/HYDRA-UMC)** — la carte mère physique du bras robotique : hôte CM5 + coprocesseur STM32H745 double cœur, coordonnant jusqu'à 8 bras-outils via CAN-OTA/SPI-OTA.
- **[HYDRA-UMC-OS](https://github.com/JuanenRac/HYDRA-UMC-OS)** — couche produit reproductible sur Raspberry Pi OS pour le CM5 : agent en lecture seule, config/profils validés, provisionnement WiFi de premier contact.
- **[HYDRA-UMC-SDK](https://github.com/JuanenRac/HYDRA-UMC-SDK)** — le contrat JSON-Schema partagé et la barrière de sécurité contre laquelle chaque bridge valide ses commandes.

*Backend Central & Clients*
- **[HYDRA-UMC-DSI](https://github.com/JuanenRac/HYDRA-UMC-DSI)** — interface tactile native pour l'écran tactile DSI 7" embarqué, intégrée directement sur le CM5.
- **[HYDRA-UMC-EDITOR-URDF](https://github.com/JuanenRac/HYDRA-UMC-EDITOR-URDF)** — créateur/éditeur graphique de bureau pour URDF qui envoie les modèles terminés vers le propre catalogue de STUDIO.
- **[HYDRA-UMC-BRIDGE-AMR](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-AMR)** — frontière de coordination pour les flottes AGV/AMR via un éditeur MQTT VDA 5050 réel.
- **[HYDRA-UMC-BRIDGE-CNC](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-CNC)** — coordinateur haut niveau pour cellules CNC avec accès réel au statut/octets de contrôle GRBL.
- **[HYDRA-UMC-BRIDGE-DROIDS](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-DROIDS)** — frontière de coordination pour droïdes à pattes/humanoïdes, avec un véritable émetteur de commandes Boston Dynamics Spot.
- **[HYDRA-UMC-BRIDGE-LASER](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-LASER)** — coordinateur de sécurité pour cellules laser lisant 3 vraies sécurités GPIO de clé/enceinte/verrouillage.
- **[HYDRA-UMC-BRIDGE-OPENPNP](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-OPENPNP)** — coordinateur haut niveau sûr pour le flux de cartes du pick-and-place OpenPnP.
- **[HYDRA-UMC-BRIDGE-PRINTER3D](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-PRINTER3D)** — frontière de coordination sûre pour imprimantes 3D Moonraker/Klipper, avec de vraies commandes de tâche contrôlées.
- **[HYDRA-UMC-BRIDGE-ROS2](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-ROS2)** — coordinateur de sécurité avec un vrai transport ROS 2 rclpy à importation paresseuse.
- **[HYDRA-UMC-BRIDGE-UAV](https://github.com/JuanenRac/HYDRA-UMC-BRIDGE-UAV)** — frontière de coordination pour UAV équipés de caméra, avec un véritable émetteur de commandes MAVLink.

*Plateforme d'Outils URTC*
- **[URTC](https://github.com/JuanenRac/URTC)** — firmware pour la carte physique Universal Robot Tool Controller, plus de 25 profils d'outil sur bus CAN.
- **[URTC-FLASHER](https://github.com/JuanenRac/URTC-FLASHER)** — outil de bureau à interface graphique pour flasher les cartes URTC, CAN-OTA plus SWD/JTAG puce complète.
- **[URTC-TESTER](https://github.com/JuanenRac/URTC-TESTER)** — outil de bureau de diagnostic CAN-bus en direct pour cartes URTC, un panneau par profil d'outil.
- **[URTC-WEB-STUDIO](https://github.com/JuanenRac/URTC-WEB-STUDIO)** — alternative basée navigateur à URTC-TESTER via la Web Serial API, sans installation locale.

*Nœud IA de Vision (Hailo-8)*
- **[HYDRA-UMC-VISION-NODE](https://github.com/JuanenRac/HYDRA-UMC-VISION-NODE)** — hub d'intégration pour le pipeline de vision Hailo-8, avec une vraie vérification de disponibilité matérielle par étape.
- **[HYDRA-UMC-DETECTION-HEF](https://github.com/JuanenRac/HYDRA-UMC-DETECTION-HEF)** — registre réel de modèles compilés avec vérification de chargement sécurisé par architecture Hailo/checksum.
- **[HYDRA-UMC-VISION-STREAMER](https://github.com/JuanenRac/HYDRA-UMC-VISION-STREAMER)** — générateur réel de pipeline GStreamer + config MediaMTX, avec une vraie frontière d'intégration HailoRT.
- **[HYDRA-UMC-VISUAL-SERVOING-API](https://github.com/JuanenRac/HYDRA-UMC-VISUAL-SERVOING-API)** — vraie loi de correction Position-Based Visual Servoing, verrouillée sur l'état de zone en amont.
- **[HYDRA-UMC-SAFETY-ZONES](https://github.com/JuanenRac/HYDRA-UMC-SAFETY-ZONES)** — vraie vérification de violation de zone et demande d'E-STOP, avec application de la fraîcheur de calibration.

*Nœud IA Cognitif (Hailo-10)*
- **[HYDRA-UMC-COGNITIVE-NODE](https://github.com/JuanenRac/HYDRA-UMC-COGNITIVE-NODE)** — hub d'intégration pour le pipeline cognitif Hailo-10 (orchestration LLM/VLA/voix).
- **[HYDRA-UMC-VLA-ENGINE](https://github.com/JuanenRac/HYDRA-UMC-VLA-ENGINE)** — vrai encodage/décodage de jetons d'action et génération de trajectoire pour un modèle Vision-Language-Action.
- **[HYDRA-UMC-VOICE-UI](https://github.com/JuanenRac/HYDRA-UMC-VOICE-UI)** — vrai front-end vocal (VAD + analyseur d'intention) avec un relais Watch borné et soumis à confirmation.
- **[HYDRA-UMC-SEMANTIC-PLANNER](https://github.com/JuanenRac/HYDRA-UMC-SEMANTIC-PLANNER)** — vraie décomposition de tâches basée sur des règles et récupération sémantique d'erreurs sur les codes d'erreur MCU.
- **[HYDRA-UMC-DOCS-QA](https://github.com/JuanenRac/HYDRA-UMC-DOCS-QA)** — vraie recherche documentaire TF-IDF (bibliothèque standard uniquement) sur les propres documents Markdown de cet écosystème.

*Orchestration & Essaim*
- **[HYDRA-UMC-ORCHESTRATOR](https://github.com/JuanenRac/HYDRA-UMC-ORCHESTRATOR)** — hub d'intégration avec un vrai contrat de rapport de santé gRPC/Protobuf et une machine à états de mission.
- **[HYDRA-UMC-JOB-DISPATCHER](https://github.com/JuanenRac/HYDRA-UMC-JOB-DISPATCHER)** — vraie file de tâches basée sur la priorité avec déduplication, via une vraie API HTTP.
- **[HYDRA-UMC-NODE-HEALING](https://github.com/JuanenRac/HYDRA-UMC-NODE-HEALING)** — vrai chien de garde de santé de flotte basé sur gRPC, avec retry/backoff et détection d'incohérence d'identité.
- **[HYDRA-UMC-PATH-PLANNER-3D](https://github.com/JuanenRac/HYDRA-UMC-PATH-PLANNER-3D)** — vrai planificateur de trajectoire 3D basé sur RRT, avec vraie validation des collisions obstacle/espace de travail.
- **[HYDRA-UMC-SWARM-SYNC](https://github.com/JuanenRac/HYDRA-UMC-SWARM-SYNC)** — vraie synchronisation d'état CRDT LWW-Element-Map, testée par propriétés pour la convergence multi-cellule.

*Données & Analytique*
- **[HYDRA-UMC-DATALAKE](https://github.com/JuanenRac/HYDRA-UMC-DATALAKE)** — vrai magasin de séries temporelles basé sur sqlite3, avec une vraie API HTTP d'ingestion/requête.
- **[HYDRA-UMC-ANOMALY-DETECTOR](https://github.com/JuanenRac/HYDRA-UMC-ANOMALY-DETECTOR)** — vrai détecteur d'anomalies FFT + ligne de base statistique, avec surveillance de dérive.
- **[HYDRA-UMC-PRODUCTION-REPORTS](https://github.com/JuanenRac/HYDRA-UMC-PRODUCTION-REPORTS)** — vrai calcul OEE/disponibilité sur l'historique de DATALAKE, avec export CSV reproductible.
- **[HYDRA-UMC-TELEMETRY-COLLECTOR](https://github.com/JuanenRac/HYDRA-UMC-TELEMETRY-COLLECTOR)** — vrai pipeline d'ingestion CAN/WebSocket vers DATALAKE, avec déduplication par séquence.

*Passerelle Industrielle*
- **[HYDRA-UMC-GATEWAY-INDUSTRIAL](https://github.com/JuanenRac/HYDRA-UMC-GATEWAY-INDUSTRIAL)** — hub d'intégration relayant vers les protocoles industriels, avec une vraie couche de liste blanche de commandes/contre-pression.
- **[HYDRA-UMC-OPCUA-SERVER](https://github.com/JuanenRac/HYDRA-UMC-OPCUA-SERVER)** — vrai espace d'adressage OPC-UA, vérifié avec une vraie session client du protocole binaire.
- **[HYDRA-UMC-MQTT-BROKER](https://github.com/JuanenRac/HYDRA-UMC-MQTT-BROKER)** — vrai broker MQTT avec authentification par client optionnelle et ACL de sujets.
- **[HYDRA-UMC-MTCONNECT-ADAPTER](https://github.com/JuanenRac/HYDRA-UMC-MTCONNECT-ADAPTER)** — vrais points de terminaison XML MTConnect `/probe` et `/current`, avec sortie en mode dégradé.

*Outils Complémentaires & Opérations de l'Écosystème*
- **[HYDRA-UMC-DASHBOARD-AI](https://github.com/JuanenRac/HYDRA-UMC-DASHBOARD-AI)** — panneaux Smart Summaries et Anomaly Highlighting sur DATALAKE/ANOMALY-DETECTOR, avec un repli statistique honnête.
- **[HYDRA-UMC-TOOL-CLI](https://github.com/JuanenRac/HYDRA-UMC-TOOL-CLI)** — CLI de flotte avec un vrai contrat de codes de sortie stable, un vrai client en direct de la propre API de HYDRA-UMC-SERVER.
- **[HYDRA-UMC-WATCH](https://github.com/JuanenRac/HYDRA-UMC-WATCH)** — application compagnon WearOS avec de vraies alertes haptiques et un relais vocal vers le téléphone jumelé.
- **[URTC-SMART-RACK](https://github.com/JuanenRac/URTC-SMART-RACK)** — firmware pour un rack de montage de cartes avec décodage réel d'ID d'outil et logique de préchauffage Smart Idle.
- **[URTC-VISION-TOOL](https://github.com/JuanenRac/URTC-VISION-TOOL)** — firmware plus un vrai compagnon de vision Python pour une tête d'outil d'inspection thermique/RGB.
- **[HYDRA-UMC-UPDATER](https://github.com/JuanenRac/HYDRA-UMC-UPDATER)** — outil administratif de bureau qui découvre, clone et met à jour chaque dépôt de cet écosystème.


## 👤 AUTEUR
**JuanenRac** (Electro Hobby 3D)
📧 electrohobby3d@gmail.com
📺 [youtube.com/@electrohobby3d](https://youtube.com/@electrohobby3d)

## 📜 LICENCE
GPL-3.0 - Voir le fichier LICENSE pour plus de détails.
