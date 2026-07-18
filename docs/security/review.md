# Security review

## Implemented controls

- Loopback HTTP binds to `127.0.0.1`; bearer comparison is timing-safe and browser WebSocket authentication uses a subprotocol rather than a URL token.
- Pi remains the provider credential boundary. No provider SDK or provider-secret field exists in the PWA, relay, operational records, or NOSH remote vault.
- Remote devices use Ed25519 signatures, X25519 sealed key wrapping, XChaCha20-Poly1305 payloads, Argon2id local vaults, one-use pairing and relay challenges, per-device permissions, expiry, revocation, optimistic versions, and persistent idempotency receipts.
- Remote command policies bind type, target, permission, and payload. The relay frame parser rejects unknown/plaintext fields and applies size/cache bounds. There is no generic process or shell command.
- Project roots are canonicalized; static paths, paper figures, backup paths, and worktree operations reject escapes. Evaluated Git commits and accepted/negative evidence are immutable or retention-protected.
- CSP blocks third-party scripts, frames, objects, and unexpected connection targets. Release assets use a frozen lockfile, hashes, SBOM, license notices, and build attestation.

## Required independent review before a public remote release

An independent cryptography review must examine nonce lifecycle, account-key rotation/recovery, libsodium browser bundling, relay challenge canonicalization, replay state after daemon crash, and browser-vault/XSS exposure. A Windows security review must verify ACL inheritance, child environment secrecy, service lifecycle, and process-tree controls. Any high or critical finding blocks release.

The current implementation records a remote acceptance intent before an external side effect and a completion/failure afterward. Crash reconciliation of an accepted-but-incomplete remote intent is not yet proven by a recovery test; treat that as a release blocker for unattended remote use.
