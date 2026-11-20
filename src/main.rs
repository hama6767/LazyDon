mod accounts;
mod api;
mod app;
mod formatting;
mod links;
mod media;
mod metrics;
mod settings;
mod stream;
mod ui;

use anyhow::{Context, Result};
use std::io::{self, Write};

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_owned())
}
fn login() -> Result<(api::Config, api::Account)> {
    println!("Mastodon login: register LazyDon on your server, then authorize it in your browser.");
    let instance = prompt("Server (e.g. mastodon.social): ")?;
    let auth = api::register_app(&instance)?;
    println!("\nOpen this URL in your browser:\n{}\n", auth.authorize_url);
    let code = prompt("Authorization code: ")?;
    let config = api::exchange_code(&auth, &code)?;
    let account = api::Mastodon::new(config.clone())?
        .verify_credentials()
        .context("Could not verify the account")?;
    println!("Logged in as @{}", account.acct);
    Ok((config, account))
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("LazyDon — Vim-inspired Mastodon client\n\nUsage: lazydon [--login]\n\n--login  Add or reauthorize an account\n-h, --help  Show this help");
        return Ok(());
    }
    if args.iter().any(|a| a != "--login") {
        anyhow::bail!("Unknown option. See --help.");
    }
    let mut store = accounts::AccountStore::load()?;
    if args.iter().any(|a| a == "--login") || store.accounts.is_empty() {
        let (config, account) = login()?;
        let mut candidate = store.clone();
        candidate.add_with_identity(config, account)?;
        candidate.save()?;
        if let Err(e) = candidate.cleanup_removed_legacy() {
            eprintln!("Old credential cleanup failed: {e:#}");
        }
        store = candidate;
    }
    let mut terminal = ratatui::init();
    let enabled = crossterm::execute!(io::stdout(), crossterm::event::EnableBracketedPaste);
    let result = enabled
        .context("Could not enable bracketed paste")
        .and_then(|_| app::run(&mut terminal, store));
    let disabled = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
    result?;
    disabled.context("Could not restore terminal paste mode")
}
