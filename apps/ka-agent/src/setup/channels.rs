//! Setup Telegram and Matrix pairing sections. Split from setup.rs
//! (moves only).

use super::*;

impl Setup {
    /// `None` = no channel; the channel name that was configured.
    pub(super) async fn step_channels(&mut self) -> Result<Option<&'static str>, PromptError> {
        let options = vec![
            "Telegram".to_string(),
            "Matrix".to_string(),
            "Not now".to_string(),
        ];
        match prompt::choice("Talk to Ka Agent from your phone?", &options, 2)? {
            0 => {
                self.setup_telegram().await?;
                Ok(Some("telegram"))
            }
            1 => {
                self.setup_matrix().await?;
                Ok(Some("matrix"))
            }
            _ => Ok(None),
        }
    }

    pub(super) async fn setup_telegram(&mut self) -> Result<(), PromptError> {
        println!("In Telegram, open @BotFather → /newbot → copy the token it gives you.");
        loop {
            let token = prompt::secret("Bot token")?;
            let base = self
                .doc
                .get("telegram")
                .and_then(|t| t.get("api_base"))
                .and_then(|v| v.as_str())
                .unwrap_or("https://api.telegram.org")
                .to_string();
            match ka_agent_channels::telegram::telegram_get_me(&base, &token).await {
                Ok(username) => {
                    println!("✓ connected to @{username}");
                    self.telegram_token = Some(token);
                    self.telegram_user = Some(username);
                    return Ok(());
                }
                Err(e) => println!("✗ {e} — try again"),
            }
        }
    }

    pub(super) async fn setup_matrix(&mut self) -> Result<(), PromptError> {
        loop {
            let homeserver = prompt::text("Homeserver", Some("https://matrix.org"))?;
            let user = prompt::text("Bot account (e.g. @ka-bot:matrix.org)", None)?;
            let password = prompt::secret("Bot password (used once to log in; never stored)")?;
            match ka_agent_channels::matrix::login(&homeserver, &user, &password).await {
                Ok(_token) => {
                    self.matrix = Some((homeserver, user));
                    break;
                }
                Err(e) => {
                    println!("✗ login failed: {e}");
                    let fallback = vec![
                        "Enter the password again".to_string(),
                        "Paste an access token instead".to_string(),
                    ];
                    if prompt::choice("", &fallback, 0)? == 1 {
                        let _token = prompt::secret("Access token")?;
                        self.matrix = Some((homeserver, user));
                        break;
                    }
                }
            }
        }
        let operator = prompt::text("Your own Matrix id (e.g. @you:matrix.org)", None)?;
        self.matrix_operator = Some(operator);
        if let Some((_, bot)) = &self.matrix {
            println!("Invite {bot} to a room or DM it — it joins automatically.");
        }
        Ok(())
    }
}
