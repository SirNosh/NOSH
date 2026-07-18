# Encrypted remote control

Deploy `apps/relay` to your own Cloudflare account and set a strong `RELAY_ADMIN_TOKEN` secret. Publish only the static `apps/web/dist` PWA through the tag-gated Pages workflow.

Configure Windows without placing the relay admin token in Project files:

```powershell
nosh remote setup --relay-url=https://relay.example --channel=<random-16+-chars> --admin-token=<secret>
nosh stop
nosh start
nosh remote status
nosh remote pair
```

Scan the one-time terminal QR from `nosh remote pair`, or enter its relay URL, channel, capability, and a device-local vault password in the PWA. Confirm the short verification code on Windows, then run the printed approval command. The QR contains only the expiring pairing capability and public routing data. The browser unwraps the current account-key version only after approval. Revoke a lost device with `nosh remote revoke <device-id>`; NOSH rotates the data key and sends each remaining device a durable rekey envelope sealed to that device's X25519 public key.

Each WebSocket connection proves possession of its Ed25519 key with a one-use relay challenge. Command metadata and payload receive account-key encryption before entering the relay; the signed inner envelope binds account, Project, target, semantic operation, permission, expected version, issue/expiry time, and idempotency key. The relay cannot issue commands or read Project content.

If Windows or the relay is unavailable, the PWA is read-only and must not claim a Pause/Stop succeeded. If noshd was interrupted after accepting a command, Settings lists it until a local user inspects the target and records whether the effect occurred. Physical cross-network acceptance must be repeated for each deployment.
