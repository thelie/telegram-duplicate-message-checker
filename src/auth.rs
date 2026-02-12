use anyhow::{bail, Result};
use grammers_client::{Client, SignInError};
use tracing::info;

/// If the client is not yet authorized, run the interactive sign-in flow.
pub async fn ensure_authorized(
    client: &Client,
    api_hash: &str,
    phone_number: Option<&str>,
) -> Result<()> {
    if client.is_authorized().await? {
        info!("Already authorized");
        return Ok(());
    }

    info!("Not authorized, starting authentication flow");

    let phone = match phone_number {
        Some(p) => p.to_string(),
        None => prompt("Enter your phone number (e.g. +1234567890): ")?,
    };

    let token = client.request_login_code(&phone, api_hash).await?;
    let code = prompt("Enter the code you received: ")?;

    match client.sign_in(&token, &code).await {
        Ok(user) => {
            info!(
                "Signed in as {}",
                user.first_name().unwrap_or("(unknown)")
            );
            Ok(())
        }
        Err(SignInError::PasswordRequired(password_token)) => {
            let hint = password_token.hint().unwrap_or("none");
            info!("2FA required (hint: {})", hint);
            let password = prompt("Enter your 2FA password: ")?;
            client
                .check_password(password_token, password.trim())
                .await?;
            info!("Successfully signed in with 2FA");
            Ok(())
        }
        Err(e) => bail!("Sign in failed: {}", e),
    }
}

fn prompt(msg: &str) -> Result<String> {
    use std::io::{self, Write};
    print!("{}", msg);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}
