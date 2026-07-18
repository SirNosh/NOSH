# Security policy

## Supported versions

Security fixes are applied to the latest tagged `0.x` release until a stable support policy is published.

## Reporting

Do not open a public issue for a suspected vulnerability involving credential exposure, remote-command authorization, cryptography, Project isolation, or protected Git paths. Use GitHub's private vulnerability-reporting feature for `SirNosh/NOSH`. Include the affected commit, reproduction, impact, and whether any relay or provider credentials may have been exposed. Do not include live secrets.

## Release posture

Remote control is release-blocked by any plaintext relay exposure, signature/revocation/replay bypass, arbitrary command execution, cross-Project access, or high/critical finding. GitHub release artifacts publish hashes, CycloneDX SBOM, dependency notices, and build provenance. Verify the release digest before running `scripts/install.ps1`.

NOSH never needs provider credentials: Pi owns provider authentication. Diagnostics report only credential presence, never values.
