# Multispend Module: Account System with Threshold Signature Locks

## Context

Build a new fedimint module called `multispend` that provides an account-based system with naive k-of-n threshold signature locks. Unlike the e-cash mint module (which uses blinded bearer tokens), multispend accounts are explicitly identified by their cosigner set and hold a tracked balance within the federation. Spending requires collecting k individual Schnorr signatures from the n registered cosigners.

To coordinate multi-party signing, the module includes a built-in encrypted bulletin board: a per-server API where signers can post and retrieve encrypted messages (partial signatures, proposals, etc.) without needing out-of-band communication.

## Design

### Account Model

- **Account ID**: SHA256 hash of `(threshold, sorted_pubkeys[])` — deterministic, derived from the spending policy
- **Threshold**: `k` (number of signatures required)
- **Cosigners**: `n` secp256k1 public keys (sorted canonically)
- **Balance**: `Amount` tracked per account in server DB

### Transaction Types

| Type | Purpose | Key Fields |
|------|---------|------------|
| **Output** (deposit) | Fund an account | `account_id`, `threshold`, `pubkeys[]`, `amount` |
| **Input** (withdraw) | Spend from an account | `account_id`, `amount`, `signatures[]` (k of n Schnorr sigs over the tx) |

- `process_output`: Creates or adds to an account's balance. Stores the account metadata (threshold, pubkeys) on first deposit.
- `verify_input`: Verifies that at least `k` valid Schnorr signatures are present from registered cosigners.
- `process_input`: Deducts the amount from the account balance.
- The message signed is the transaction hash (from `InputMeta.pub_key` / the fedimint transaction signing flow). We use the first pubkey in the signature set as the `pub_key` returned in `InputMeta`, but the actual authorization check happens in `process_input` where we verify all k signatures.

### Bulletin Board (Encrypted Message Broker)

Per-server API endpoints (not consensus). Each server stores messages in its local DB independently.

- **Post message**: Client submits `(recipient_account_id, sender_pubkey, encrypted_payload, timestamp)`
- **Fetch messages**: Client queries by `(account_id, since_timestamp)` to get new messages
- **Encryption**: ECIES with secp256k1 — encrypt to each recipient's pubkey using ephemeral ECDH + ChaCha20-Poly1305 (via the existing `fedimint-aead` crate or `secp256k1` ECDH + manual AEAD)
- **Expiry**: Messages auto-expire after a configurable TTL (default: 24 hours) to prevent unbounded storage growth
- Messages are opaque blobs to the server — it cannot read them

### Signing Flow (Client-Side)

1. **Proposer** creates a spend proposal (amount, destination) and posts it to the bulletin board encrypted to all cosigners
2. **Cosigners** poll the bulletin board, decrypt proposals, and if they approve, sign the transaction and post their signature back
3. **Proposer** collects k signatures, assembles the fedimint transaction input, and submits it

## File Structure

```
modules/
├── fedimint-multispend-common/
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs          # Module types: Input, Output, ConsensusItem, errors
│       └── config.rs       # MultispendConfig, MultispendClientConfig
│
├── fedimint-multispend-server/
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs          # ServerModule + ServerModuleInit implementation
│       └── db.rs           # Server DB keys (accounts, balances, bulletin board)
│
└── fedimint-multispend-client/
    ├── Cargo.toml
    └── src/
        ├── lib.rs          # ClientModule + ClientModuleInit, high-level API
        ├── db.rs           # Client DB keys
        └── states.rs       # Client state machines (deposit/withdraw tracking)
```

## Implementation Steps

### Step 1: Scaffold the common crate (`fedimint-multispend-common`)

Create `modules/fedimint-multispend-common/` modeled on `fedimint-dummy-common`.

**`src/lib.rs`** — define:
- `KIND = "multispend"`, `MODULE_CONSENSUS_VERSION = (0, 0)`
- `MultispendInput { account_id: sha256::Hash, amount: Amount, signatures: Vec<(PublicKey, schnorr::Signature)> }`
- `MultispendOutput { account_id: sha256::Hash, amount: Amount, threshold: u64, pubkeys: Vec<PublicKey> }`
- `MultispendOutputOutcome` (unit struct)
- `MultispendConsensusItem` (unit struct — unused initially)
- `MultispendInputError { InsufficientBalance, InvalidSignature, AccountNotFound, InsufficientSignatures, DuplicateSignerInInput }`
- `MultispendOutputError { ThresholdExceedsSigners, EmptySignerSet, AccountMismatch }`
- `MultispendModuleTypes` + `plugin_types_trait_impl_common!`
- `MultispendCommonInit` implementing `CommonModuleInit`
- Helper: `compute_account_id(threshold, &sorted_pubkeys) -> sha256::Hash`

**`src/config.rs`** — define:
- `MultispendConfig { private: MultispendConfigPrivate, consensus: MultispendConfigConsensus }`
- `MultispendConfigConsensus { bulletin_board_ttl_secs: u64 }`
- `MultispendConfigPrivate` (empty initially)
- `MultispendClientConfig { bulletin_board_ttl_secs: u64 }`
- `plugin_types_trait_impl_config!`

**`Cargo.toml`**: deps on `fedimint-core`, `serde`, `thiserror`, `bitcoin_hashes`

### Step 2: Scaffold the server crate (`fedimint-multispend-server`)

Create `modules/fedimint-multispend-server/` modeled on `fedimint-dummy-server`.

**`src/db.rs`** — define DB keys:
- `AccountKey(sha256::Hash)` → `AccountInfo { threshold: u64, pubkeys: Vec<PublicKey>, balance: Amount }` (prefix `0x01`)
- `AccountAuditDepositKey(OutPoint)` → `Amount` (prefix `0x02`)
- `AccountAuditWithdrawKey(InPoint)` → `Amount` (prefix `0x03`)
- `BulletinMessageKey(sha256::Hash, u64)` → `BulletinMessage { sender: PublicKey, ciphertext: Vec<u8>, timestamp: u64 }` (prefix `0x10`, keyed by `(account_id, message_id)`)
- `BulletinMessageCounterKey(sha256::Hash)` → `u64` (prefix `0x11`, per-account message counter)

**`src/lib.rs`** — implement:
- `MultispendInit` (ServerModuleInit):
  - `trusted_dealer_gen`: generate simple configs with default TTL
  - `distributed_gen`: same (no DKG needed for this module)
  - `get_client_config`: extract client config
- `Multispend` (ServerModule):
  - `verify_input`: Check account exists, check k valid Schnorr sigs from registered pubkeys, check no duplicate signers
  - `process_input`: Deduct balance, fail if insufficient. Record audit.
  - `process_output`: On first deposit, create account with threshold+pubkeys. On subsequent deposits, verify threshold+pubkeys match, add to balance. Record audit.
  - `consensus_proposal`/`process_consensus_item`: return empty / bail (not used)
  - `audit`: assets = withdrawals, liabilities = deposits
  - `api_endpoints`:
    - `GET /account/:id` → returns account info (balance, threshold, pubkey count)
    - `POST /bulletin/post` → store encrypted message for an account
    - `GET /bulletin/fetch` → retrieve messages for an account since a timestamp
    - `POST /bulletin/cleanup` → (internal) prune expired messages

### Step 3: Scaffold the client crate (`fedimint-multispend-client`)

Create `modules/fedimint-multispend-client/` modeled on `fedimint-dummy-client`.

**`src/db.rs`**:
- `MultispendClientAccountKey(sha256::Hash)` → `ClientAccountInfo { threshold: u64, pubkeys: Vec<PublicKey>, our_keypair_index: usize }` (prefix `0x01`)

**`src/states.rs`**:
- `MultispendStateMachine` enum wrapping `DepositStateMachine` and `WithdrawStateMachine`
- `DepositStateMachine`: Created → Accepted/Rejected (await tx acceptance)
- `WithdrawStateMachine`: Created → Accepted/Refunded (await tx acceptance, refund on rejection)

**`src/lib.rs`**:
- `MultispendClientModule` struct holding `client_ctx`, `secp`, `notifier`
- `MultispendClientInit` implementing `ClientModuleInit`
- `ClientModule` impl with `input_fee`/`output_fee` returning zero fees
- High-level API methods:
  - `create_account(threshold, pubkeys) -> AccountId` — compute account ID, store locally
  - `deposit(account_id, amount) -> OperationId` — create a transaction with MultispendOutput
  - `propose_spend(account_id, amount) -> SpendProposal` — create and optionally post to bulletin board
  - `sign_proposal(proposal) -> Signature` — sign a spend proposal with our key
  - `submit_spend(account_id, amount, signatures) -> OperationId` — assemble and submit MultispendInput transaction
  - `post_bulletin_message(account_id, recipient_pubkey, payload)` — encrypt and post to bulletin board
  - `fetch_bulletin_messages(account_id, since)` — fetch and decrypt messages from bulletin board
  - `get_account_info(account_id)` — query federation for account balance/info

### Step 4: Wire into workspace

- Add all three crates to `Cargo.toml` workspace members list
- Add workspace dependency entries for `fedimint-multispend-{common,client,server}`
- Do NOT register in `fedimintd/src/lib.rs` default_modules() yet (keep it opt-in for now)

### Step 5: Tests

Create `modules/fedimint-multispend-tests/` with:
- Unit tests for `compute_account_id` determinism
- Unit tests for signature verification logic
- Integration test using `Fixtures`:
  - Create a 2-of-3 account
  - Deposit funds
  - Withdraw with 2 valid signatures (should succeed)
  - Withdraw with 1 signature (should fail)
  - Withdraw with invalid signature (should fail)
  - Bulletin board post/fetch round-trip

## Key Reference Files

| Purpose | Path |
|---------|------|
| Module trait (server) | `fedimint-server-core/src/lib.rs` |
| Module trait (client) | `fedimint-client-module/src/module/mod.rs` |
| ServerModuleInit trait | `fedimint-server-core/src/init.rs` |
| ClientModuleInit trait | `fedimint-client-module/src/module/init.rs` |
| ModuleCommon / macros | `fedimint-core/src/module/mod.rs`, `fedimint-core/src/macros.rs` |
| Dummy module (template) | `modules/fedimint-dummy-{common,server,client}/src/` |
| DB key macros | `fedimint-core/src/db/` (`impl_db_record!`, `impl_db_lookup!`) |
| Config macros | `fedimint-core/src/macros.rs` (`plugin_types_trait_impl_config!`) |
| AEAD encryption | `crypto/fedimint-aead/src/lib.rs` |
| Schnorr signing example | `fedimint-core/src/core/backup.rs` (BackupRequest::sign) |
| Test fixtures | `fedimint-testing/src/fixtures.rs` |
| Workspace Cargo.toml | `Cargo.toml` (root) |

## Verification

1. `cargo check -p fedimint-multispend-common -p fedimint-multispend-server -p fedimint-multispend-client` — compiles cleanly
2. `just format` — formatting passes
3. `just clippy` — no warnings
4. Run integration tests in `fedimint-multispend-tests` to verify deposit/withdraw/bulletin board flows
