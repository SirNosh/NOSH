# Remote protocol v1

The relay accepts only `{frameId, deviceId, kind, ciphertext}`. Durable Object sequence is routing metadata added after storage. Enrolled devices obtain a short-lived challenge and sign `nosh-relay-v1\n<channelId>\n<deviceId>\n<challenge>` before WebSocket upgrade.

`ciphertext` is an account-key XChaCha20-Poly1305 envelope over the full signed command or event. A command's inner authenticated header includes protocol/account/device/Project/target IDs, semantic type, fixed required permission, expected version, key version, issue/expiry time, nonce, ciphertext, and Ed25519 signature. `noshd` alone decrypts, validates, persists, dispatches, and emits an encrypted acknowledgement.

Relay cache is bounded to 5,000 recent ciphertext frames per channel while snapshots are retained separately. Clients persist a sequence cursor only after decrypting and durably applying that frame. A revocation closes the lost device's tagged sockets, prevents its new challenges, rotates the account key/version, and durably publishes a per-device rekey envelope sealed to each remaining X25519 public key before publishing the next snapshot. Accepted commands without a durable terminal receipt remain fail-closed until a local user records an inspected `applied` or `not_applied` resolution.
