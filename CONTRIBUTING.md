# Contributing to HYDRA-UMC-HIL-BRIDGE 🦾

We welcome contributions to the hardware-in-the-loop bridge of the HYDRA-UMC platform.

## Technology Stack
- **Languages**: Node.js 20+, C++20.
- **Protocols**: gRPC (Local High-speed), WebSocket (Remote Mirroring).
- **Serialization**: Protocol Buffers.
- **Real-time**: Shared Memory (IPC) for ultra-low latency.

## Guidelines
1. **Sync Latency**: Ensure that the bidirectional state synchronization maintains a round-trip time below 5ms.
2. **Protocol Safety**: All gRPC service definitions must be strictly typed and backward compatible.
3. **Safety Interlocks**: Any changes to the simulation-to-real command logic must ensure that virtual safety interlocks cannot be bypassed.
4. **Testing**: Validate the bridge with both the `HYDRA-UMC-TWIN` (virtual) and `HYDRA-UMC` core (physical) simultaneously.
