mod accounts;
mod api;
mod app;
mod formatting;
mod ime;
mod links;
mod media;
mod metrics;
mod settings;
mod stream;
mod ui;

use anyhow::{Context, Result};
use std::io::{self, IsTerminal, Write};

fn prompt(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush()?;
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        anyhow::bail!("Input ended before login was completed");
    }
    Ok(input.trim().to_owned())
}

fn authorization_link(url: &str, interactive: bool) -> Result<String> {
    if !links::valid_url(url) {
        anyhow::bail!("Invalid authorization URL");
    }
    if interactive {
        // The displayed label stays short, while the terminal receives the
        // complete destination as an OSC 8 hyperlink without line breaks.
        Ok(format!(
            "\x1b]8;;{url}\x1b\\Open authorization page\x1b]8;;\x1b\\"
        ))
    } else {
        Ok(url.to_owned())
    }
}

fn login() -> Result<(api::Config, api::Account)> {
    println!("Mastodon login: register LazyDon on your server, then authorize it in your browser.");
    let instance = prompt("Server (e.g. mastodon.social): ")?;
    let auth = api::register_app(&instance)?;
    println!(
        "\n{}",
        authorization_link(&auth.authorize_url, io::stdout().is_terminal())?
    );
    match links::open(&auth.authorize_url) {
        Ok(()) => println!("Opening the authorization page in your browser."),
        Err(e) => eprintln!("Could not open browser: {e:#}"),
    }
    println!("Enter :open to reopen, :copy to copy the full URL, or :show to print it.");
    let code = loop {
        let input = prompt("Authorization code: ")?;
        match input.as_str() {
            ":open" => match links::open(&auth.authorize_url) {
                Ok(()) => println!("Opening the authorization page."),
                Err(e) => eprintln!("Could not open browser: {e:#}"),
            },
            ":copy" => match links::copy_to_clipboard(&auth.authorize_url) {
                Ok(()) => println!("Full authorization URL copied."),
                Err(e) => eprintln!("Could not copy URL: {e:#}"),
            },
            ":show" => println!(
                "Authorization URL (one line; it may wrap visually):\n{}",
                auth.authorize_url
            ),
            "" => println!("Enter the authorization code, :open, :copy, or :show."),
            _ => break input,
        }
    };
    let config = api::exchange_code(&auth, &code)?;
    let account = api::Mastodon::new(config.clone())?
        .verify_credentials()
        .context("Could not verify the account")?;
    println!("Logged in as @{}", account.acct);
    Ok((config, account))
}

#[cfg(test)]
mod login_tests {
    use super::authorization_link;

    #[test]
    fn interactive_link_keeps_a_long_url_intact_in_the_hyperlink_destination() {
        let url = format!(
            "https://example.org/oauth/authorize?client_id={}&scope=read+write",
            "x".repeat(300)
        );
        let link = authorization_link(&url, true).unwrap();
        assert!(link.contains(&format!("\x1b]8;;{url}\x1b\\")));
        assert!(!link.contains('\n'));
        assert_eq!(authorization_link(&url, false).unwrap(), url);
        assert!(authorization_link("https://example.org/\x1b]8;;evil", true).is_err());
    }
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
