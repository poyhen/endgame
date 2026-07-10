use std::io::{BufRead, Write};

use grammers_client::{Client, SignInError};

pub async fn ensure_authorized(client: &Client, api_hash: &str) -> anyhow::Result<()> {
    if client.is_authorized().await? {
        return Ok(());
    }

    println!("Signing in...");
    let phone = prompt("Enter your phone number (international format): ")?;
    let token = client.request_login_code(&phone, api_hash).await?;
    let code = prompt("Enter the code you received: ")?;
    match client.sign_in(&token, &code).await {
        Err(SignInError::PasswordRequired(password_token)) => {
            let hint = password_token.hint().unwrap_or("");
            let password = prompt(&format!("Enter the password (hint {hint}): "))?;
            client
                .check_password(password_token, password.trim())
                .await?;
        }
        Ok(_) => {}
        Err(error) => return Err(error.into()),
    }
    println!("Signed in!");
    Ok(())
}

fn prompt(message: &str) -> anyhow::Result<String> {
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(message.as_bytes())?;
    stdout.flush()?;

    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();
    let mut line = String::new();
    stdin.read_line(&mut line)?;
    Ok(line.trim().to_string())
}
