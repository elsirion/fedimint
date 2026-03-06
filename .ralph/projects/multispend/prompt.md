# Multispend Module: Account System with k-of-n Schnorr Locks

## Goal
Implement a new opt-in Fedimint module `multispend` that provides:
1. Consensus-tracked account balances keyed by deterministic account policy.
2. Spending authorized by naive k-of-n individual Schnorr signatures (no aggregated threshold signature scheme).
3. A per-server, non-consensus encrypted bulletin board for signer coordination.

## Scope
1. Create and wire these crates:
- `modules/fedimint-multispend-common`
- `modules/fedimint-multispend-server`
- `modules/fedimint-multispend-client`
- `modules/fedimint-multispend-tests`
2. Keep module opt-in only. Do not register it in `fedimintd` default modules.
3. Follow existing Fedimint patterns from dummy module crates and current trait signatures on this branch.

## Non-Goals
- No distributed key generation or threshold-crypto protocol.
- No signer key rotation or account policy updates.
- No cross-federation interoperability.
- No fee model beyond zero input/output fees for now.

## Hard Requirements
1. No `unwrap()` in non-test code; use `expect()` with clear reason when unavoidable.
2. Use structured logging (`tracing` fields) for meaningful events and validation failures.
3. Use deterministic validation only in consensus paths.
4. Define strict bounds:
- `1 <= threshold <= n`
- `n <= 32`
- No duplicate signer keys
- `amount > 0`
- Bulletin payload max size: 64 KiB
5. If current Fedimint trait signatures differ from this prompt, adapt to branch APIs while preserving required behavior.

## Cryptography and Canonicalization
1. Use secp256k1 Schnorr with BIP340-compatible keys (`XOnlyPublicKey` where feasible on this branch).
2. Canonical signer order is ascending lexicographic order of serialized key bytes.
3. Account ID derivation:
- `account_id = sha256("multispend/v1/account" || threshold_u16_be || n_u16_be || concatenated_sorted_pubkeys_bytes)`
4. Reject account creation/deposit if provided signer list is not canonical or contains duplicates.
5. Signature message:
- Use Fedimint’s canonical input signing message for transaction authorization on this branch.
- Add module domain separation by signing `sha256("multispend/v1/spend" || canonical_fedimint_message)` when constructing/verifying multispend signatures.

## Data Model
1. `MultispendOutput`:
- `account_id`
- `amount`
- `threshold`
- `pubkeys`
2. `MultispendInput`:
- `account_id`
- `amount`
- `signatures: Vec<(PubKey, Signature)>`
3. Errors:
- Input errors: `InsufficientBalance`, `InvalidSignature`, `AccountNotFound`, `InsufficientSignatures`, `DuplicateSignerInInput`
- Output errors: `ThresholdExceedsSigners`, `EmptySignerSet`, `AccountMismatch`
4. Config:
- `bulletin_board_ttl_secs` default `86400`
- Add `bulletin_max_payload_bytes` default `65536` (consensus/client config)

## Server-Side Consensus Behavior
1. `process_output`:
- Recompute `account_id` from `(threshold, pubkeys)` and require match.
- On first deposit, create account with policy and initial balance.
- On later deposits, require exact policy match and add balance.
- Record deposit audit entry.
2. `verify_input`:
- Validate structural constraints and signer uniqueness.
- Load account policy.
- Verify at least `k` valid signatures from unique registered signers.
- Reject unknown signers and invalid signatures.
3. `process_input`:
- Check account exists and has sufficient balance.
- Subtract amount atomically.
- Record withdrawal audit entry.
4. `audit`:
- Report deposits and withdrawals using Fedimint audit conventions consistent with existing modules.
5. `consensus_proposal` / `process_consensus_item`:
- No custom consensus items for v1 (empty/no-op behavior).

## Bulletin Board (Per-Server API, Non-Consensus)
1. Storage model:
- Key by `(account_id, message_id)` where `message_id` is monotonic per-account counter.
- Store `sender_pubkey`, `recipient_pubkey`, `ciphertext`, `created_at_secs`.
2. API endpoints:
- `GET /account/:id` returns `account_id`, `balance`, `threshold`, `pubkeys_len`.
- `POST /bulletin/post` body: `account_id`, `sender_pubkey`, `recipient_pubkey`, `ciphertext`.
- `GET /bulletin/fetch` query: `account_id`, `recipient_pubkey`, `since_id`, `limit`.
- `POST /bulletin/cleanup` prunes expired messages.
3. Behavior:
- Server treats ciphertext as opaque bytes.
- Enforce max payload size.
- Return messages ordered by `message_id`.
- Expire messages older than TTL using server time.
4. Client expectation for federation use:
- Bulletin board is per-server and non-consensus; client should query multiple peers and merge by `(peer_id, message_id)`.

## Encryption Format for Bulletin Payloads
1. Use secp256k1 ECDH with ephemeral sender key and recipient static key.
2. Derive AEAD key via HKDF-SHA256 with info string `"multispend/bulletin/v1"`.
3. Encrypt with ChaCha20-Poly1305 using `fedimint-aead`.
4. Ciphertext envelope bytes:
- `version (1 byte)`
- `ephemeral_pubkey (33 bytes compressed)`
- `nonce (12 bytes)`
- `ciphertext_with_tag (remaining bytes)`

## Crate-by-Crate Tasks
1. `fedimint-multispend-common`:
- Define module constants, core types, errors, config, `compute_account_id`, plugin macro wiring, `CommonModuleInit`.
2. `fedimint-multispend-server`:
- Define DB records/lookup keys for accounts, audit records, bulletin messages, bulletin counters.
- Implement `ServerModuleInit` config generation and client-config export.
- Implement `ServerModule` handlers and API endpoints above.
3. `fedimint-multispend-client`:
- Define client DB for locally tracked account metadata.
- Implement deposit/withdraw state machines.
- Implement high-level API:
  - `create_account`
  - `deposit`
  - `propose_spend`
  - `sign_proposal`
  - `submit_spend`
  - `post_bulletin_message`
  - `fetch_bulletin_messages`
  - `get_account_info`
4. `fedimint-multispend-tests`:
- Add unit and integration tests listed below.
5. Workspace wiring:
- Add all four crates to workspace members and workspace deps.

## Required Tests
1. Unit tests:
- `compute_account_id` determinism.
- Canonical sorting and duplicate-key rejection.
- Signature verification accepts exactly valid k-of-n and rejects malformed cases.
2. Integration tests:
- Create 2-of-3 account, deposit funds, withdraw with 2 valid signatures succeeds.
- Withdraw with 1 signature fails with `InsufficientSignatures`.
- Withdraw with invalid signature fails with `InvalidSignature`.
- Withdraw with duplicate signer signatures fails with `DuplicateSignerInInput`.
- Deposit with mismatched `(account_id, threshold, pubkeys)` fails with `AccountMismatch`.
- Bulletin board post/fetch round-trip succeeds.
- Bulletin cleanup removes expired messages.

## Verification Commands
1. `cargo check -p fedimint-multispend-common -p fedimint-multispend-server -p fedimint-multispend-client -p fedimint-multispend-tests`
2. `cargo test -p fedimint-multispend-tests`
3. `just format`
4. `cargo clippy -p fedimint-multispend-common -p fedimint-multispend-server -p fedimint-multispend-client -p fedimint-multispend-tests -- -D warnings`

## Acceptance Criteria
1. All crates compile and tests pass with commands above.
2. Module behavior is deterministic in consensus-critical paths.
3. Account policy validation and signature checks are strict and reproducible.
4. Bulletin board APIs are functional, bounded, and documented in code comments/types.
5. No default-module registration in `fedimintd`.