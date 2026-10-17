# Security review status

The terminal-first release has not passed an independent security review. Previous browser/relay reviews are historical evidence, not approval of this runtime.

Review the controls and open gates in [the threat model](../../THREAT_MODEL.md) and [release gates](../testing/release-gates.md). Required focus includes local Origin/session authentication, per-user credential storage, TUI child-environment secrecy, terminal escape injection, typed agent authority, cross-Project isolation, Jobs and process trees, backup/restore, and supply-chain integrity.

Remote pairing, relay cryptography, browser vaults, PWA assets, and managed shells are removed features. Do not interpret their removal as a passed security test. Record commit, platform, tools, findings, remediation, and independent retest evidence for the remaining product.
