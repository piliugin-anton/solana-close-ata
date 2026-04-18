# Solana ATA close CLI

CLI tool to close empty Solana Associated Token Accounts (ATAs) and reclaim their rent. Scans both SPL Token and Token-2022 programs.

## Build

```sh
cargo build --release
```

## Usage

The RPC endpoint defaults to `https://api.mainnet-beta.solana.com`. Override with the `SOLANA_RPC_URL` env var or `--rpc <URL>` (CLI > env > default).

```sh
export SOLANA_RPC_URL="https://your-rpc-provider.example.com"
```

### Dry-run (recommended first step)

Lists every token account owned by the keypair with mint, balance, rent, and program, but sends no transactions:

```sh
./target/release/close-ata <BASE58_SECRET_KEY> --dry-run
```

### Close empty ATAs

```sh
./target/release/close-ata <BASE58_SECRET_KEY>
```

You'll be prompted to confirm before any transaction is sent. Use `-y` to skip.

### Close all ATAs, burning non-zero balances

```sh
./target/release/close-ata <BASE58_SECRET_KEY> --force --dry-run  # verify first
./target/release/close-ata <BASE58_SECRET_KEY> --force
```

### Flags

| Flag | Description |
|---|---|
| `--dry-run` | List what would be closed; send nothing. |
| `--force` | Burn remaining token balances before closing. Destructive. |
| `--rpc <URL>` | RPC endpoint. Defaults to `$SOLANA_RPC_URL`, then to mainnet-beta. |
| `-y`, `--yes` | Skip the confirmation prompt. |
| `--batch-size <N>` | Close instructions per transaction. Default: 12. |

## Notes

- **Secret key format**: base58-encoded 64-byte expanded secret (what Phantom exports). 32-byte seeds are not accepted.
- **Shell history**: the secret key is a positional argument. Either prefix with a space (if `HISTCONTROL=ignorespace`), run with `HISTFILE=/dev/null`, or read from a pipe into a wrapper. Avoid leaving it in `~/.bash_history`.
- **Rent reclaim**: each empty SPL Token ATA holds ~0.00203928 SOL. Token-2022 accounts with extensions hold more; the actual lamports are read from each account.
- **Frozen accounts** are always skipped. `--force` does not override this — frozen accounts cannot be closed on-chain until thawed by the mint's freeze authority.
- **Token-2022 edge cases**: accounts with unharvested transfer-fee withholdings or certain extensions may fail to close. The tool surfaces the RPC error and moves on; manual harvest is out of scope.
- **Batching**: default 12 closes per tx is well within compute limits. With `--force`, each non-empty account adds a burn ix to the same batch — lower `--batch-size` if you hit tx size or CU limits.

## Safety

`--force` burns tokens irreversibly. Always dry-run first and read the skip list.