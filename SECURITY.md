# Security Policy 🔒 (HYDRA-UMC-HIL-BRIDGE)

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.x.x  | ✅ Yes             |

## Reporting a Vulnerability

**CRITICAL: Do not report safety-critical vulnerabilities through public GitHub issues.**

In a HIL bridge, a security flaw can allow unauthorized remote control of physical hardware. If you discover a vulnerability affecting the **WebSocket authentication**, **gRPC command injection**, or **state buffer overflows**:

1. **Email**: Send a detailed report to `electrohobby3d@gmail.com`.
2. **Impact**: Describe if the bug allows bypassing safety interlocks, hijacking motor commands from a remote session, or crashing the synchronization artery.
3. **Response**: Initial acknowledgment within 48 hours.

We follow a coordinated disclosure policy to ensure hardware safety before public release.
