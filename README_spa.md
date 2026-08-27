<p align="center">
  <img src="images/HYDRA_UMC_BANNER.svg" alt="HYDRA-UMC-HIL-BRIDGE banner" width="100%">
</p>

# 🌉 HYDRA-UMC-HIL-BRIDGE

<p align="center"><a href="README.md">🇺🇸 English</a> | 🇪🇸 <b>Español</b> | <a href="README_fra.md">🇫🇷 Français</a> | <a href="README_ita.md">🇮🇹 Italiano</a> | <a href="README_deu.md">🇩🇪 Deutsch</a> | <a href="README_zho.md">🇨🇳 简体中文</a> | <a href="README_jpn.md">🇯🇵 日本語</a></p>

### ⚡ Interfaz Hardware-in-the-Loop para Sincronización Real vs Virtual

<p align="left">
  <img src="https://img.shields.io/badge/Licencia-GPL%203.0-blue.svg" alt="GPL 3.0">
  <img src="https://img.shields.io/badge/Protocolo-WebSocket%20%2F%20gRPC-yellow.svg" alt="Protocol">
  <img src="https://img.shields.io/badge/Función-Sync%20Latencia%20Cero-green.svg" alt="Sync">
  <img src="https://img.shields.io/badge/Etapa-Establecido%20v0-brightgreen.svg" alt="Etapa establecido v0">
</p>

---

## 1. 🛠️ VISIÓN GENERAL TÉCNICA

**HYDRA-UMC-HIL-BRIDGE** es la arteria de comunicación que permite la funcionalidad Hardware-in-the-Loop (HIL). Sincroniza el estado entre los controladores físicos y el motor del Digital Twin en tiempo real.

Permite a los desarrolladores enviar comandos desde cualquier interfaz (App, Suite, Studio) al simulador como si fuera un robot físico y, a la inversa, puede reflejar los movimientos de un robot físico en el mundo virtual para supervisión remota y seguimiento (shadowing) del gemelo digital.

### Características Clave:
* 🛡️ **Enclavamiento de Seguridad (v0):** el subcomando real `route` bloquea un comando destinado a hardware real siempre que un reporte de riesgo del gemelo indique una colisión inminente - ver "Comprobación de honestidad" abajo para lo que funciona hoy exactamente.
* 🌉 **Puente Bidireccional (v0, sin transporte todavía):** existe encaminamiento real basado en modo (Real/Simulación) y reflejo (mirroring) real e incondicional; todavía no hay ninguna conexión gRPC/WebSocket real hacia un HYDRA-UMC-TWIN o controlador HYDRA-UMC de verdad.
* ⚡ **Mirroring de Latencia Cero (parcial):** el subcomando real `mirror` refleja hoy un comando hacia un receptor en memoria; "latencia cero" sobre una conexión de red real sigue siendo trabajo futuro.
* 📡 **Protocolo Unificado (planeado):** usa gRPC para sincronización local de alta velocidad y WebSockets para monitorización remota - todavía no hay ningún transporte de red conectado, a propósito (ver `Cargo.toml`).
* 🧪 **Transporte a prueba de fallos, testeable sin hardware (v0):** `CommandSink::send()` devuelve un `Result` real, y un `SimulatedTransport` puede modelar un enlace con timeout o desconectado; el puente reporta un resultado `TransportFailure` diferenciado en vez de afirmar jamás que un comando fue entregado cuando no lo fue - se puede probar vía `--transport-latency-ms`/`--transport-timeout-ms`/`--transport-disconnected` en `route`.

**Comprobación de honestidad - qué funciona hoy de verdad:** `route --mode real|simulation --joint NOMBRE --position VALOR [--collision-risk] [--distance METROS] [--transport-latency-ms MS] [--transport-timeout-ms MS] [--transport-disconnected]` toma una decisión real de encaminamiento - el modo `simulation` siempre envía, el modo `real` está sujeto a un enclavamiento de seguridad real que bloquea el comando siempre que se indique `--collision-risk`. `mirror --joint NOMBRE --position VALOR` refleja un comando incondicionalmente. Ambos encaminan por defecto hacia un receptor en memoria (`RecordingSink`, no un controlador real ni una instancia real de HYDRA-UMC-TWIN), o hacia un `SimulatedTransport` que puede fallar de verdad (timeout/desconexión) cuando se pasan los flags de transporte de arriba - todavía no hay transporte gRPC/WebSocket. Ver [`CHANGELOG.md`](CHANGELOG.md) para lo entregado exactamente, y la Hoja de Ruta abajo para lo que sigue por delante.

---

## 2. 🔄 FLUJO DE SINCRONIZACIÓN HIL

La división por modo en `BRIDGE` y la compuerta del enclavamiento de
seguridad en la ruta `Modo Real` son reales hoy (`bridge.rs`/
`interlock.rs`), encaminando hacia un receptor en memoria en vez de un
proceso real en vivo. Todo lo que toca un proceso real `APP`, `TWIN` o
`CORE` mediante una conexión real sigue siendo trabajo futuro.

```mermaid
flowchart LR
    APP["Interfaz de Control - planeado"] --> BRIDGE["HIL-BRIDGE - encaminamiento real v0"]
    BRIDGE -- Modo Simulación --> TWIN["HYDRA-UMC-TWIN - planeado"]
    BRIDGE -- "Modo Real (con enclavamiento - real v0)" --> CORE["Núcleo HYDRA-UMC (STM32) - planeado"]
    CORE -- Feedback --> BRIDGE
    TWIN -- Feedback --> BRIDGE
    BRIDGE --> APP
```

---

## 3. 🧱 ARQUITECTURA Y DECISIONES DE DISEÑO

* **Por qué este puente no tiene carpetas `hardware/`/`firmware/`/`os/`.** Software puro - conecta hardware ya existente (apps reales, firmware real de HYDRA-UMC) con el gemelo digital, sin placa propia.
* **Por qué es hermano, no un submódulo, de HYDRA-UMC-TWIN.** Un puente hardware-in-the-loop necesita seguir funcionando (y seguir dejando fluir los comandos de un dispositivo real) con independencia de lo que esté haciendo en ese instante el propio bucle de renderizado/física de HYDRA-UMC-TWIN - un proceso separado significa que un reinicio del gemelo no descarta un comando real en curso.
* **Cómo encaja en el resto del ecosistema.** Permite que HYDRA-UMC-SUITE, HYDRA-UMC-ANDROID-CONTROL y HYDRA-UMC-IOS-CONTROL controlen HYDRA-UMC-TWIN como si fuera una célula real respaldada por HYDRA-UMC-SERVER - la misma superficie de control, un objetivo simulado.
* **Por qué el enclavamiento confía en la propia bandera `collision_imminent` del gemelo en vez de derivar una a partir de un umbral de distancia.** El gemelo ya ha hecho el razonamiento geométrico/físico real en el momento en que reporta el riesgo - cuestionar esa conclusión aquí con un umbral de distancia independiente solo sería una segunda opinión de seguridad, posiblemente inconsistente. Ver la documentación propia del módulo `interlock.rs` para cómo esto refleja el límite detect-vs-enforce de HYDRA-UMC-SAFETY-ZONES.
* **Por qué el modo `Simulation` nunca está sujeto al enclavamiento.** Todo el sentido de encaminar un comando hacia el gemelo en vez de hardware real es poder ver una colisión predicha desarrollarse de forma segura - bloquearla ahí anularía la propia función que existe para respaldar.
* **Por qué `CommandSink` es un trait con solo una implementación en memoria `RecordingSink` hoy.** Todavía no existe ningún transporte gRPC/WebSocket real (ver el propio comentario de `Cargo.toml`) - `RecordingSink` es honesto al respecto: registra lo que se le pidió enviar sin transmitir a ningún sitio, el mismo razonamiento que `NullEStopRequester` en HYDRA-UMC-SAFETY-ZONES.
* **Por qué `CommandSink::send()` devuelve un `Result` en vez de `()`.** Un transporte real puede fallar en la entrega (timeout, desconexión) con independencia de si el enclavamiento permitió el comando - si `send()` no puede fallar, el puente no tiene forma de evitar afirmar éxito ante un comando que nunca llegó de verdad. `SimulatedTransport` existe justamente para que ese camino de fallo sea real y testeable hoy, sin esperar a que exista un transporte real.
* **Por qué `TransportFailure` es un `RouteOutcome` separado de `BlockedByInterlock`.** Son dos tipos distintos de "no ocurrió": uno es un rechazo deliberado de seguridad (el enclavamiento decidió no reenviarlo), el otro es una entrega de mejor esfuerzo que simplemente no se completó. Fusionarlos en un fallo genérico ocultaría qué capa de seguridad detuvo realmente el comando - útil para depurar y para cualquier política futura que los trate de forma distinta (reintentar un fallo de transporte es razonable; reintentar tras un bloqueo del enclavamiento no lo es).

---

## 📂 ESTRUCTURA DE DIRECTORIOS

Puente puramente software, sin diseño de hardware propio; por eso este
proyecto no lleva carpetas `hardware/`, `firmware/` ni `os/`, conforme a la política de estructura del repositorio.

```text
HYDRA-UMC-HIL-BRIDGE/
├── src/
│   ├── protocol.rs       # Tipos reales JointCommand/Mode
│   ├── interlock.rs      # Decisión real de enclavamiento de seguridad
│   ├── bridge.rs         # Encaminamiento real basado en modo + mirroring + CommandSink/SimulatedTransport
│   └── main.rs           # Entry point + subcomandos reales `route`/`mirror`
├── docs/                # Documentación y guías de integración
├── build/               # Notas/artefactos de build (la salida real de cargo vive en target/, en .gitignore)
├── images/              # Medios y diagramas
├── scripts/             # Scripts de utilidad
├── tools/
│   ├── build_test.py    # Comprobación de compilación sin versionado
│   └── ci_validate.py   # Validación de manifiesto/CHANGELOG/docs usada por CI
├── Cargo.toml           # Metadatos del paquete, dependencias, version cuentakilometros
├── bump_version.py      # Bump de version tipo cuentakilometros (usado por build.sh/.bat)
├── build.sh / build.bat # Bump de version, `cargo test`, luego `cargo build --release`
├── build-test.sh / build-test.bat # Comprobación de compilación sin versionado
└── run.sh / run.bat     # Ejecuta el binario release compilado (reenvía argumentos)
```

---

## 🏗️ BUILD Y RUN

Requiere el toolchain de Rust (`cargo`/`rustc`, instalar vía [rustup](https://rustup.rs)) y Python 3.10+ (solo para `bump_version.py`).

```bash
# Linux / macOS
./build.sh   # bump de version cuentakilometros, `cargo test` (14 tests), luego `cargo build --release`
./run.sh     # ejecuta target/release/hydra-umc-hil-bridge, imprime nombre + version + rol
```

```bat
:: Windows
build.bat
run.bat
```

`build.sh`/`build.bat` incrementan la version del propio `Cargo.toml` de este proyecto siguiendo la regla "cuentakilometros" del ecosistema (PATCH+1, con acarreo a MINOR al pasar de 9), ejecutan la suite de tests real, y luego construyen un binario release.

Los subcomandos reales `route` y `mirror`:

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

`route` sale con `0` (enviado), `1` (bloqueado por el enclavamiento de seguridad - un resultado real y significativo, no un error), `2` (entrada inválida), o `3` (fallo de transporte - el enclavamiento lo permitió, pero la entrega no se confirmó). `mirror` sale con `0`, `2` o `3`.

`Cargo.toml` no lleva crates externos todavía a propósito - ver el comentario dentro del archivo para lo que se añade cuando empiece el trabajo real de transporte gRPC/WebSocket.

---

## 🚀 HOJA DE RUTA
* **Fase 1:** Sincronización de Digital Twin con telemetría de hardware en tiempo real y latencia sub-10ms.
* **Fase 2:** Integración de Physics Replica con simuladores de grado industrial (Isaac Sim) y soporte para cuerpos deformables.
* **Fase 3:** Patrones de recuperación automatizados de Node Healing para failover descentralizado y detección temprana de degradación de sensores.
* **Fase 4:** Sincronización HIL multi-controlador (Swarm HIL) y soporte para generación de datos sintéticos fotorrealistas.

---

## 🔗 Proyectos Relacionados

Este proyecto forma parte de un ecosistema de robótica más amplio del mismo autor (JuanenRac / Electro Hobby 3D), que abarca firmware, software de control, nodos de IA y herramientas de flota. Vale la pena conocerlo, ya que una petición podría en realidad ser sobre uno de estos proyectos en vez de sobre este repositorio.

### Familia

**Padre:** **[HYDRA-UMC-TWIN](https://github.com/JuanenRac/HYDRA-UMC-TWIN)** — el padre de integración que este puente enlaza con hardware real.

**Hermanos:**
- **[HYDRA-UMC-PHYSICS-REPLICA](https://github.com/JuanenRac/HYDRA-UMC-PHYSICS-REPLICA)** — servicio de simulación hermano, mismo padre.
- **[HYDRA-UMC-SYNTHETIC-DATA-GEN](https://github.com/JuanenRac/HYDRA-UMC-SYNTHETIC-DATA-GEN)** — servicio de simulación hermano, mismo padre.

### Relación Directa (fuera de la familia)

- **[HYDRA-UMC-SERVER](https://github.com/JuanenRac/HYDRA-UMC-SERVER)** — objetivo del puente hardware-in-the-loop.
- **[HYDRA-UMC-SUITE](https://github.com/JuanenRac/HYDRA-UMC-SUITE)** — objetivo del puente hardware-in-the-loop.
- **[HYDRA-UMC-ANDROID-CONTROL](https://github.com/JuanenRac/HYDRA-UMC-ANDROID-CONTROL)** — objetivo del puente hardware-in-the-loop.
- **[HYDRA-UMC-IOS-CONTROL](https://github.com/JuanenRac/HYDRA-UMC-IOS-CONTROL)** — objetivo del puente hardware-in-the-loop.

### Resto del Ecosistema

**Plataforma HYDRA-UMC** — la célula de micro-fábrica multi-robot
- **[HYDRA-UMC](https://github.com/JuanenRac/HYDRA-UMC)** — la placa base CM5 + STM32H745 que orquesta hasta 8 brazos robóticos.
- **[HYDRA-UMC-SERVER](https://github.com/JuanenRac/HYDRA-UMC-SERVER)** — el backend Express/WebSocket con el que habla cada cliente de control.
- **[HYDRA-UMC-STUDIO](https://github.com/JuanenRac/HYDRA-UMC-STUDIO)** — panel de control web, visualización 3D multi-robot.
- **[HYDRA-UMC-ANDROID-CONTROL](https://github.com/JuanenRac/HYDRA-UMC-ANDROID-CONTROL)** — app de control Android por Wi-Fi/Bluetooth.
- **[HYDRA-UMC-IOS-CONTROL](https://github.com/JuanenRac/HYDRA-UMC-IOS-CONTROL)** — app de control iOS/iPadOS construida en Flutter.
- **[HYDRA-UMC-SUITE](https://github.com/JuanenRac/HYDRA-UMC-SUITE)** — centro de mando de enjambre de escritorio (Python/PySide6).
- **[HYDRA-UMC-EDITOR-URDF](https://github.com/JuanenRac/HYDRA-UMC-EDITOR-URDF)** — editor de modelos URDF de escritorio para el catálogo de robots.
- **[HYDRA-UMC-DSI](https://github.com/JuanenRac/HYDRA-UMC-DSI)** — interfaz táctil nativa para la pantalla DSI integrada.

**Plataforma URTC** — el controlador de cabezal de herramienta que lleva cada brazo HYDRA-UMC
- **[URTC](https://github.com/JuanenRac/URTC)** — controlador de cabezal de herramienta CAN, 25 perfiles de herramienta.
- **[URTC-FLASHER](https://github.com/JuanenRac/URTC-FLASHER)** — herramienta de escritorio de flasheo CAN-OTA + SWD/JTAG.
- **[URTC-TESTER](https://github.com/JuanenRac/URTC-TESTER)** — herramienta de escritorio de diagnóstico CAN en vivo.
- **[URTC-WEB-STUDIO](https://github.com/JuanenRac/URTC-WEB-STUDIO)** — alternativa basada en navegador vía Web Serial API.

**🎥 Vision AI Node (Hailo-8)**
- [HYDRA-UMC-VISION-NODE](https://github.com/JuanenRac/HYDRA-UMC-VISION-NODE)
- [HYDRA-UMC-VISION-STREAMER](https://github.com/JuanenRac/HYDRA-UMC-VISION-STREAMER)
- [HYDRA-UMC-DETECTION-HEF](https://github.com/JuanenRac/HYDRA-UMC-DETECTION-HEF)
- [HYDRA-UMC-SAFETY-ZONES](https://github.com/JuanenRac/HYDRA-UMC-SAFETY-ZONES)
- [HYDRA-UMC-VISUAL-SERVOING-API](https://github.com/JuanenRac/HYDRA-UMC-VISUAL-SERVOING-API)

**🧠 Cognitive AI Node (Hailo-10)**
- [HYDRA-UMC-COGNITIVE-NODE](https://github.com/JuanenRac/HYDRA-UMC-COGNITIVE-NODE)
- [HYDRA-UMC-VLA-ENGINE](https://github.com/JuanenRac/HYDRA-UMC-VLA-ENGINE)
- [HYDRA-UMC-VOICE-UI](https://github.com/JuanenRac/HYDRA-UMC-VOICE-UI)
- [HYDRA-UMC-SEMANTIC-PLANNER](https://github.com/JuanenRac/HYDRA-UMC-SEMANTIC-PLANNER)
- [HYDRA-UMC-DOCS-QA](https://github.com/JuanenRac/HYDRA-UMC-DOCS-QA)

**🐝 Orchestration & Swarm**
- [HYDRA-UMC-ORCHESTRATOR](https://github.com/JuanenRac/HYDRA-UMC-ORCHESTRATOR)
- [HYDRA-UMC-SWARM-SYNC](https://github.com/JuanenRac/HYDRA-UMC-SWARM-SYNC)
- [HYDRA-UMC-PATH-PLANNER-3D](https://github.com/JuanenRac/HYDRA-UMC-PATH-PLANNER-3D)
- [HYDRA-UMC-JOB-DISPATCHER](https://github.com/JuanenRac/HYDRA-UMC-JOB-DISPATCHER)
- [HYDRA-UMC-NODE-HEALING](https://github.com/JuanenRac/HYDRA-UMC-NODE-HEALING)

**📊 Data & Analytics**
- [HYDRA-UMC-DATALAKE](https://github.com/JuanenRac/HYDRA-UMC-DATALAKE)
- [HYDRA-UMC-TELEMETRY-COLLECTOR](https://github.com/JuanenRac/HYDRA-UMC-TELEMETRY-COLLECTOR)
- [HYDRA-UMC-ANOMALY-DETECTOR](https://github.com/JuanenRac/HYDRA-UMC-ANOMALY-DETECTOR)
- [HYDRA-UMC-PRODUCTION-REPORTS](https://github.com/JuanenRac/HYDRA-UMC-PRODUCTION-REPORTS)

**🏭 Industrial Gateway**
- [HYDRA-UMC-GATEWAY-INDUSTRIAL](https://github.com/JuanenRac/HYDRA-UMC-GATEWAY-INDUSTRIAL)
- [HYDRA-UMC-OPCUA-SERVER](https://github.com/JuanenRac/HYDRA-UMC-OPCUA-SERVER)
- [HYDRA-UMC-MQTT-BROKER](https://github.com/JuanenRac/HYDRA-UMC-MQTT-BROKER)
- [HYDRA-UMC-MTCONNECT-ADAPTER](https://github.com/JuanenRac/HYDRA-UMC-MTCONNECT-ADAPTER)

**🛠️ Complementary Tools**
- [URTC-SMART-RACK](https://github.com/JuanenRac/URTC-SMART-RACK)
- [URTC-VISION-TOOL](https://github.com/JuanenRac/URTC-VISION-TOOL)
- [HYDRA-UMC-WATCH](https://github.com/JuanenRac/HYDRA-UMC-WATCH)
- [HYDRA-UMC-TOOL-CLI](https://github.com/JuanenRac/HYDRA-UMC-TOOL-CLI)
- [HYDRA-UMC-DASHBOARD-AI](https://github.com/JuanenRac/HYDRA-UMC-DASHBOARD-AI)


## 👤 AUTOR
**JuanenRac** (Electro Hobby 3D)
📧 electrohobby3d@gmail.com

## 📜 LICENCIA
GPL-3.0 - Ver archivo LICENSE para más detalles.

## 🛠️ BUILD & RUN

Usa la comprobación de compilación sin versionado antes de una compilación de publicación:

| Acción | Windows | Linux / macOS |
|---|---|---|
| Comprobación de compilación (sin cambiar versión ni CHANGELOG) | `build-test.bat` | `./build-test.sh` |
| Ejecución / desarrollo (cuando exista) | `run*.bat` o `dev*.bat` | `./run*.sh` o `./dev*.sh` |

`build-test.bat` y `build-test.sh` compilan o validan el stack del proyecto sin incrementar `hydra-umc.project.json` ni modificar `CHANGELOG.md`. Solo pueden crear salidas normales del compilador. Los scripts existentes `build*.bat`, `build*.sh`, `run*` y `dev*` conservan su comportamiento específico de versión o ejecución; úsalos cuando necesites ese comportamiento.