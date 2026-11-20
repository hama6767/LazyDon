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
fn login() -> Result<api::Config> {
    println!("Mastodon login: register LazyDon on your server, then authorize it in your browser.");
    let instance = prompt("Server (e.g. mastodon.social): ")?;
    let auth = api::register_app(&instance)?;
    println!("\nOpen this URL in your browser:\n{}\n", auth.authorize_url);
    let code = prompt("Authorization code: ")?;
    let config = api::exchange_code(&auth, &code)?;
    let account = api::Mastodon::new(config.clone())?
        .verify_credentials()
        .context("Could not verify the account")?;
    config.save()?;
    println!("Logged in as @{}", account.acct);
    Ok(config)
}
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("LazyDon — Vim-inspired Mastodon client\n\nUsage: lazydon [--login]\n\n--login  Register and authorize an account again\n-h, --help  Show this help");
        return Ok(());
    }
    if args.iter().any(|a| a != "--login") {
        anyhow::bail!("Unknown option. See --help.");
    }
    let config = if args.iter().any(|a| a == "--login") {
        login()?
    } else {
        match api::Config::load()? {
            Some(c) => c,
            None => login()?,
        }
    };
    let account = api::Mastodon::new(config.clone())?
        .verify_credentials()
        .context("Account verification failed; run with --login to authenticate again")?;
    let mut terminal = ratatui::init();
    let enabled = crossterm::execute!(io::stdout(), crossterm::event::EnableBracketedPaste);
    let result = enabled
        .context("Could not enable bracketed paste")
        .and_then(|_| app::run(&mut terminal, config, format!("@{}", account.acct)));
    let disabled = crossterm::execute!(io::stdout(), crossterm::event::DisableBracketedPaste);
    ratatui::restore();
    result?;
    disabled.context("Could not restore terminal paste mode")
}
