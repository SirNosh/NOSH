# Security policy

## Supported versions

Security fixes target the latest tagged `0.x` release until a stable policy is published.

## Reporting

Use GitHub private vulnerability reporting for `SirNosh/NOSH`. Do not post live secrets or public exploit details. Include the affected commit, reproduction, impact, and potential credential or research-data exposure.

## Release posture

The current product is local and terminal-first. Browser/PWA, relay, remote pairing, and embedded shell services have been removed. Never expose `noshd` beyond loopback. Same-user native clients are trusted by design: automatic trust requires a loopback socket, a loopback `Host` header, and either no `Origin` or an `Origin` matching that host. Other callers need a 15-minute session issued with the bootstrap capability.

Authorization bypass, arbitrary control-plane command execution, cross-Project/path escape, protected-branch bypass, secret exposure, or a high/critical finding blocks release. Verify release hashes and provenance before running installation scripts. Review the generated notices and SBOM, including Bun/OpenTUI runtime dependencies.

Pi owns provider authentication. Diagnostics must report only credential presence, never values. Independent application-security review remains required; see [THREAT_MODEL.md](THREAT_MODEL.md).
