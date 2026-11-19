<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# Encryption at rest — design

**Status: DESIGNED, not implemented.** Today the database is plain SQLite and
`SECURITY.md` says so. This note decides the order of work and leaves the open questions
marked **[A]**.

## What we are defending against

| Case | Covered today? | By what |
|---|---|---|
| A stolen, powered-off laptop | Yes, if the OS disk is encrypted | BitLocker, FileVault, LUKS — not us |
| Another user account on the same machine | Yes | Per-user profile ACLs, `0600` tokens |
| **A backup file** copied to a USB stick, a cloud folder, an e-mail | **No** | — this is the gap |
| Sync data passing through a server | Not applicable yet | `eon-office-spec/docs/sync.md`, transport section |
| Malware running as the user | No, and no design can | stated in `SECURITY.md` |

The disk case is already well served by the operating system, and duplicating it adds a
key to lose without adding protection. The **backup** is the file that leaves the
protected disk, so it comes first.

## Order of work

1. **Encrypted backups.** `backup.create { path, passphrase }` writes the online backup
   through an authenticated stream cipher: Argon2id (memory-hard) derives the key from the
   passphrase and a random salt; XChaCha20-Poly1305 in a chunked STREAM construction
   encrypts it; a small header records the format version, the KDF parameters and the
   salt. `restore` asks for the passphrase and refuses on any authentication failure.
   Unencrypted backups remain possible and are named as such.
2. **Database encryption, opt-in.** **[A]** Two candidates:
   - *SQLCipher-compatible page encryption*: the whole file, including indexes and the
     audit log; costs a C dependency with its own crypto and build complexity on three
     platforms.
   - *Application-level encryption of document bodies*: smaller dependency surface, but
     metadata (collection names, ids, clock stamps, search terms) stays in clear, and the
     search index would have to be encrypted separately or not at all.
   Decided by measuring both against the protocol tests, start-up time and the size of the
   supply-chain addition (madde 37).
3. **Key custody** for step 2: the database key lives in the OS keychain (Windows
   Credential Manager/DPAPI, macOS Keychain, Linux Secret Service) and a **recovery key** is
   shown once, to be written down. Losing both loses the data — said plainly in the
   interface before the person turns it on. No escrow by the project, ever.

## Rules that hold whichever way step 2 goes

- Encryption is never on by default where its failure mode is silent data loss; turning it
  on is a decision the person makes with the recovery key in hand.
- No custom primitives: an audited crate for each (Argon2id, XChaCha20-Poly1305), chosen
  with their licences and maintenance checked like every dependency.
- The audit log keeps working: a tamper-evident chain is still worth more than a hidden one.
