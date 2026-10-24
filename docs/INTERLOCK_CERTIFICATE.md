<!-- =============================================================================
HYDRA-UMC-HIL-BRIDGE - Interlock certificate
Copyright (C) JuanenRac (Electro Hobby 3D) <electrohobby3d@gmail.com>
GPL-3.0-or-later - see LICENSE
============================================================================= -->

# The interlock certificate

HYDRA-UMC-HIL-BRIDGE does not send a command to real hardware on the word of whoever asks. In `real` mode it needs the risk report of THAT
command and it needs to know that HYDRA-UMC-TWIN made it. The report travels as a certificate: the report, bound to a joint and an exact
position, signed with the Ed25519 key of the twin. The bridge holds only the public half, so nobody who can reach the bridge can mint one.

## Who does what

- **The twin** (`POST /interlock/certify`, behind the token of `--certify-token-file`) signs `{"joint", "position", "collision_imminent", "distance_m"}`
  and answers the certificate. It exists only when the twin starts with `--signing-key-file` AND a certify token. `hydra-umc-twin keygen --key-file PATH`
  makes the key (64 hexadecimal characters, never over an existing file, mode 0600 on Unix) and prints the public key.
- **The bridge** (`serve --twin-public-key-file FILE`, one 64-hex public key per line, `#` for comments) verifies the certificate that comes in the `certificate`
  field of `POST /route` in real mode.

Until the physics engine exists, the twin signs the report of the caller that holds the certify token (the physics replica): a certificate is as good as the
source of the report. What the certificate does add is that the report was not written by the one who wants the robot to move.

## The certificate

```json
{
  "joint": "shoulder", "position": 0.5,
  "collision_imminent": false, "distance_m": 0.4,
  "issued_at_ms": 1700000000000,
  "nonce": "00112233445566778899aabbccddeeff",
  "key_id": "ea4a6c63e29c520a",
  "signature": "<128 hexadecimal characters>"
}
```

`key_id` is the first 8 bytes of the public key in hexadecimal. `nonce` is 32 hexadecimal characters from the system's randomness.
`distance_m` of infinity ("nothing in range") is signed as the largest finite number, since infinity has no text form.

## What is signed

Eight lines joined by `\n`, no trailing newline:

```text
hydra-umc-interlock-certificate-v1
<key_id>
<joint>
<position>             f64 in Rust's shortest round-trip form ({:?}: 0.5, 1e-7, 100.0)
<collision_imminent>   true | false
<distance_m>           the same form
<issued_at_ms>         integer
<nonce>
```

The signature is plain Ed25519 (RFC 8032) over those bytes. A fixed vector is checked by both repositories, and was reproduced with an independent
implementation of RFC 8032:

| | |
|---|---|
| seed | 32 bytes of `0x07` |
| public key | `ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c` |
| fields | `shoulder`, `0.5`, `false`, `0.4`, `1700000000000`, `00112233445566778899aabbccddeeff` |
| signature | `325a7b5fda1f54fc002b73616ea3e786eb9d7418d572460c11e21d7edde313f76d4ddc0d028c5a510885e7c3b3a923e055283d188ec6774654a41962e664850a` |

## What the bridge checks, in this order

1. a twin public key is configured (without one, every real command is blocked and the answer says why);
2. `key_id` names a trusted key, and the signature is valid for the fields;
3. the certificate is for THIS command: same joint and exactly the same position;
4. it is not dated in the future (1 s of skew allowed) and not older than `--certificate-max-age-ms` (default 2000, at most 60000);
5. its nonce was not used before (the last 8192 are remembered); only a certificate that passes everything above uses its nonce up;
6. only then the usual interlock decides on `collision_imminent` and `distance_m` (a genuine certificate that reports a collision still blocks).

`--allow-unsigned-risk` (or `HYDRA_UMC_HIL_BRIDGE_ALLOW_UNSIGNED_RISK=1`) is the bench setting: a `risk` written by the caller is believed again. It is off by default
and every start announces it. Simulation mode never needs a certificate.

## The audit

Every real order is recorded twice, `order` (it arrived: the caller named in `X-Hydra-Caller`, the address of the peer, whether the Bearer token was presented, the
certificate key and nonce) before the interlock is asked, and `outcome` (`sent_real`, `blocked_by_interlock`, `transport_failure`, with the reason). `--audit-log FILE`
(or `HYDRA_UMC_HIL_BRIDGE_AUDIT_LOG`) appends one JSON object per line; if the `order` record cannot be written the order is refused with 503 and nothing is
forwarded. `GET /audit` (behind the token) lists the last 200. The caller label is what the request says about itself: the proof is the token.
