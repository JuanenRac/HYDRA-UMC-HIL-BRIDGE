# The pass verdict of the twin before the real robot moves

HYDRA-UMC-TWIN runs a planned route in simulation (`POST /dryrun/trajectory`) and answers a **verdict** (`pass`, `fail` or `incomplete`), signed with its
Ed25519 key, bound to the exact samples of the trajectory, to the robot and to a time window. This bridge can refuse to move the real robot unless that
verdict says `pass`. The switch is called `require_twin_pass`. **It is off by default.** It comes in addition to the risk certificate of every real command
(`docs/INTERLOCK_CERTIFICATE.md`): with the switch on, both must hold.

## What a real command carries

`POST /route` in `real` mode adds the field `twin_pass`:

```json
{
  "mode": "real", "joint": "shoulder", "position": 0.25, "certificate": { ... },
  "twin_pass": {
    "verdict": { ... the answer of POST /dryrun/trajectory, as it came ... },
    "samples": [ {"t": 0.0, "joints": {"shoulder": 0.0}}, {"t": 0.1, "joints": {"shoulder": 0.25}} ]
  }
}
```

## What the bridge checks, in this order

1. the switch is on, a twin key (`--twin-public-key-file`) and the robot (`--twin-robot-id`) are set, and the request has a `twin_pass` with between 1 and 20000 samples;
2. `key_id` names a trusted key;
3. the SHA-256 of the canonical text of the samples, computed by the bridge from the samples that came with the request, is the one in the verdict (so the
   samples cannot be swapped for another trajectory);
4. the Ed25519 signature is valid for the signed text (`hydra-umc-dryrun-verdict-v1`, key, robot, SHA-256, verdict, issued, valid until, nonce);
5. the verdict is `pass`; `fail` and `incomplete` allow nothing;
6. it is for the robot named by `--twin-robot-id`, its nonce has 32 hexadecimal characters and it is inside its window (1 s of future skew allowed);
7. the command (same joint, same position) is a point of the trajectory that was not sent yet.

One verdict covers a whole trajectory, which is sent as many commands, so the verdict is not used up by the first one; each point of it is good for **one**
command. A point is marked when its command was really sent, not when it was refused. A refusal does not use up the certificate the command carries. The
verdicts remembered are bounded (the expired ones are forgotten first). The answer of a refused command says why, and the audit log records it like any other
blocked order.

## The switch

| Flag | Environment | Meaning |
|---|---|---|
| `--require-twin-pass` | `HYDRA_UMC_HIL_BRIDGE_REQUIRE_TWIN_PASS=1` | Turns the verdict on at start and **locks it**: it cannot be turned off while the bridge runs. Needs the key and the robot. |
| `--twin-robot-id NAME` | `HYDRA_UMC_HIL_BRIDGE_TWIN_ROBOT_ID` | The robot a verdict must be for. |
| `--twin-pass-file FILE` | | Where the value is kept (`{"require_twin_pass": true}`), read at start and written by a change from HYDRA-UMC-SERVER. |

`GET /settings/twin-pass` answers `requireTwinPass`, `locked`, `persisted`, `ready` (key and robot set), `robotIdSet`, `keysConfigured` and `canChange`.
`POST /settings/twin-pass` with `{"requireTwinPass": true|false, "confirm": true}` changes it. Only an administrator of HYDRA-UMC-SERVER can do it (a signed token with
the role `admin`; the bridge must have `--identity-key-file`), a confirmation is needed in both directions, a locked switch cannot be turned off (`409`), the
switch cannot be turned on while the key or the robot is missing (`409`) and a value that cannot be kept in `--twin-pass-file` is not changed (`409`). HYDRA-UMC-SERVER
carries the change from STUDIO and SUITE (`GET` and `POST /api/hil-bridge/settings`) and records it in its audit log. `GET /stats` adds `requireTwinPass` and
`blockedByTwinPass`.

This is a simulation with finite differences: it does not replace the limits of the controller or the stop of the firmware.
