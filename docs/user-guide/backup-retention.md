# Backup and retention policy

Run `nosh backup <project>` before upgrades, schema migrations, or material graph/paper changes. Retain at least three recent backups and one monthly copy on separate storage. Test restoration into a disposable user profile.

Accepted evidence, negative evidence, signed remote-command records, Reviews, evaluated commits, paper sources, and their manifests are retention-protected. Cache and temporary artifact bytes may be removed after their manifests no longer reference them. Checkpoints are retained while their run or recovery window remains open. Large datasets/checkpoints are manifest references unless explicitly copied by the user.

Backups intentionally exclude provider credentials, relay admin tokens, device private keys, `.env` files, and arbitrary large artifact bytes. The remote vault is a separate encrypted secret and is not copied into a Project backup.
