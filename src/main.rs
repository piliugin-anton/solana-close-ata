//! CLI to close empty Solana Associated Token Accounts (ATAs) and reclaim rent.
//!
//! Scans both SPL Token and Token-2022 programs for accounts owned by the
//! provided keypair, prints a summary, and (unless --dry-run) batches close
//! instructions into transactions.

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use solana_account_decoder::UiAccountData;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;
use std::io::{self, Write};
use std::str::FromStr;

const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;
/// Per-signature fee. Used only for a rough pre-flight balance check.
const FEE_LAMPORTS_PER_TX: u64 = 5_000;

/// Close empty Solana Associated Token Accounts and reclaim rent.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Owner secret key, base58-encoded 64-byte form (Phantom export format).
    secret_key: String,

    /// List what would be closed without sending any transactions.
    #[arg(long)]
    dry_run: bool,

    /// Burn any remaining token balance before closing.
    /// Destructive — always run with --dry-run first.
    #[arg(long)]
    force: bool,

    /// RPC endpoint. Falls back to SOLANA_RPC_URL, then to mainnet.
    #[arg(
        long,
        env = "SOLANA_RPC_URL",
        default_value = "https://api.mainnet.solana.com"
    )]
    rpc: String,

    /// Skip the interactive confirmation prompt.
    #[arg(short = 'y', long)]
    yes: bool,

    /// Maximum close instructions per transaction. Each --force burn counts
    /// against the same budget, so keep this conservative when burning.
    #[arg(long, default_value_t = 12)]
    batch_size: usize,
}

#[derive(Debug)]
struct AtaInfo {
    address: Pubkey,
    mint: Pubkey,
    amount: u64,
    decimals: u8,
    ui_amount: String,
    rent_lamports: u64,
    token_program: Pubkey,
    is_frozen: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let keypair = parse_keypair(&args.secret_key)?;
    let owner = keypair.pubkey();

    let client =
        RpcClient::new_with_commitment(args.rpc.clone(), CommitmentConfig::confirmed());

    eprintln!("Owner: {}", owner);
    eprintln!("RPC:   {}", args.rpc);
    eprintln!();

    let mut atas = Vec::new();
    for program_id in [spl_token::id(), spl_token_2022::id()] {
        let found = fetch_token_accounts(&client, &owner, program_id)
            .with_context(|| format!("scanning program {}", program_id))?;
        atas.extend(found);
    }

    if atas.is_empty() {
        eprintln!("No token accounts found for this owner.");
        return Ok(());
    }

    print_table(&atas);

    let (closable, skipped): (Vec<&AtaInfo>, Vec<&AtaInfo>) = atas
        .iter()
        .partition(|a| !a.is_frozen && (a.amount == 0 || args.force));

    if !skipped.is_empty() {
        eprintln!(
            "\n{} account(s) will be skipped (frozen, or non-zero balance without --force):",
            skipped.len()
        );
        for a in &skipped {
            let reason = if a.is_frozen {
                "frozen"
            } else {
                "non-zero balance"
            };
            eprintln!(
                "  {}  [{}]  mint {}  bal {}",
                a.address, reason, a.mint, a.ui_amount
            );
        }
    }

    if closable.is_empty() {
        eprintln!("\nNothing to close.");
        return Ok(());
    }

    let total_rent: u64 = closable.iter().map(|a| a.rent_lamports).sum();
    let burn_count = closable.iter().filter(|a| a.amount > 0).count();

    eprintln!(
        "\n{} account(s) to close, {} balance(s) to burn, ~{:.9} SOL to reclaim.",
        closable.len(),
        burn_count,
        total_rent as f64 / LAMPORTS_PER_SOL
    );

    if args.dry_run {
        eprintln!("\n(dry-run) No transactions sent.");
        return Ok(());
    }

    // Pre-flight fee check.
    let batch_size = args.batch_size.max(1);
    let txs_needed = closable.len().div_ceil(batch_size);
    let min_fee = (txs_needed as u64) * FEE_LAMPORTS_PER_TX;
    let sol_balance = client
        .get_balance(&owner)
        .context("failed to fetch owner SOL balance")?;
    if sol_balance < min_fee {
        bail!(
            "Insufficient SOL for fees: have {} lamports, need at least {} for {} transaction(s)",
            sol_balance,
            min_fee,
            txs_needed
        );
    }

    if !args.yes && !confirm()? {
        eprintln!("Aborted.");
        return Ok(());
    }

    close_accounts(&client, &keypair, &closable, args.force, batch_size)
}

fn parse_keypair(s: &str) -> Result<Keypair> {
    let bytes = bs58::decode(s.trim())
        .into_vec()
        .context("secret key is not valid base58")?;
    if bytes.len() != 64 {
        bail!(
            "expected 64-byte secret key (Phantom export format), got {} bytes",
            bytes.len()
        );
    }
    Keypair::try_from(bytes.as_slice()).map_err(|e| anyhow!("invalid keypair bytes: {}", e))
}

fn fetch_token_accounts(
    client: &RpcClient,
    owner: &Pubkey,
    program_id: Pubkey,
) -> Result<Vec<AtaInfo>> {
    let keyed = client
        .get_token_accounts_by_owner(owner, TokenAccountsFilter::ProgramId(program_id))?;

    let mut out = Vec::with_capacity(keyed.len());
    for k in keyed {
        let address = Pubkey::from_str(&k.pubkey)?;
        let rent_lamports = k.account.lamports;

        // Extract fields from the JSON-parsed representation rather than
        // deserializing into UiTokenAccount — more forgiving of minor
        // solana-account-decoder version drift and Token-2022 extensions.
        let info = match &k.account.data {
            UiAccountData::Json(p) => &p.parsed["info"],
            _ => bail!("unexpected non-json account data for {}", address),
        };

        let mint: Pubkey = info["mint"]
            .as_str()
            .ok_or_else(|| anyhow!("missing mint on {}", address))?
            .parse()?;
        let amount: u64 = info["tokenAmount"]["amount"]
            .as_str()
            .ok_or_else(|| anyhow!("missing tokenAmount.amount on {}", address))?
            .parse()
            .context("parsing tokenAmount.amount")?;
        let decimals = info["tokenAmount"]["decimals"]
            .as_u64()
            .ok_or_else(|| anyhow!("missing decimals on {}", address))? as u8;
        let ui_amount = info["tokenAmount"]["uiAmountString"]
            .as_str()
            .unwrap_or("0")
            .to_string();
        let is_frozen = info["state"].as_str() == Some("frozen");

        out.push(AtaInfo {
            address,
            mint,
            amount,
            decimals,
            ui_amount,
            rent_lamports,
            token_program: program_id,
            is_frozen,
        });
    }
    Ok(out)
}

fn print_table(atas: &[AtaInfo]) {
    eprintln!("Found {} token account(s):\n", atas.len());
    eprintln!(
        "{:<44}  {:<44}  {:>24}  {:>14}  PROGRAM",
        "ATA",
        "MINT",
        "BALANCE",
        "RENT (SOL)",
    );
    eprintln!("{}", "-".repeat(140));
    for a in atas {
        let program_tag = if a.token_program == spl_token::id() {
            "spl-token"
        } else {
            "token-2022"
        };
        let flags = if a.is_frozen { " [FROZEN]" } else { "" };
        eprintln!(
            "{:<44}  {:<44}  {:>24}  {:>14.9}  {}{}",
            a.address,
            a.mint,
            a.ui_amount,
            a.rent_lamports as f64 / LAMPORTS_PER_SOL,
            program_tag,
            flags
        );
    }
}

fn confirm() -> Result<bool> {
    print!("\nProceed? [y/N] ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(matches!(input.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn close_accounts(
    client: &RpcClient,
    payer: &Keypair,
    atas: &[&AtaInfo],
    force: bool,
    batch_size: usize,
) -> Result<()> {
    let mut successes = 0usize;
    let mut failures = 0usize;

    for (i, chunk) in atas.chunks(batch_size).enumerate() {
        let mut ixs: Vec<Instruction> = Vec::with_capacity(chunk.len() * 2);
        for ata in chunk {
            if force && ata.amount > 0 {
                ixs.push(build_burn_ix(ata, &payer.pubkey())?);
            }
            ixs.push(build_close_ix(ata, &payer.pubkey())?);
        }

        let recent_blockhash = client
            .get_latest_blockhash()
            .context("failed to fetch recent blockhash")?;
        let msg = Message::new(&ixs, Some(&payer.pubkey()));
        let tx = Transaction::new(&[payer], msg, recent_blockhash);

        eprintln!("\nBatch {} — {} account(s):", i + 1, chunk.len());
        match client.send_and_confirm_transaction(&tx) {
            Ok(sig) => {
                eprintln!("  ✓ {}", sig);
                successes += chunk.len();
            }
            Err(e) => {
                eprintln!("  ✗ {}", e);
                failures += chunk.len();
            }
        }
    }

    eprintln!("\nDone. Closed: {}, failed: {}", successes, failures);
    if failures > 0 {
        bail!("{} account(s) failed to close", failures);
    }
    Ok(())
}

fn build_close_ix(ata: &AtaInfo, owner: &Pubkey) -> Result<Instruction> {
    // Destination and authority are both the owner: rent is reclaimed to the
    // signer, and the signer is the account's close authority.
    let ix = if ata.token_program == spl_token::id() {
        spl_token::instruction::close_account(
            &spl_token::id(),
            &ata.address,
            owner,
            owner,
            &[],
        )?
    } else {
        spl_token_2022::instruction::close_account(
            &spl_token_2022::id(),
            &ata.address,
            owner,
            owner,
            &[],
        )?
    };
    Ok(ix)
}

fn build_burn_ix(ata: &AtaInfo, owner: &Pubkey) -> Result<Instruction> {
    let ix = if ata.token_program == spl_token::id() {
        spl_token::instruction::burn_checked(
            &spl_token::id(),
            &ata.address,
            &ata.mint,
            owner,
            &[],
            ata.amount,
            ata.decimals,
        )?
    } else {
        spl_token_2022::instruction::burn_checked(
            &spl_token_2022::id(),
            &ata.address,
            &ata.mint,
            owner,
            &[],
            ata.amount,
            ata.decimals,
        )?
    };
    Ok(ix)
}