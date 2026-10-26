//! Setup web-search, daily-briefing and voice-note sections. Split from
//! setup.rs (moves only).

use super::*;

impl Setup {
    /// Optional web search (engine `web_search`): provider + key into the
    /// engine `.env` tier the worker reads.
    pub(super) fn step_search(&mut self) -> Result<(), PromptError> {
        let options = vec![
            "Tavily — free key, 1,000 searches a month (tavily.com)".to_string(),
            "Brave Search API (brave.com/search/api)".to_string(),
            "Not now".to_string(),
        ];
        let provider =
            match prompt::choice("Let Ka search the web (news, prices, docs)?", &options, 2)? {
                0 => "tavily",
                1 => "brave",
                _ => return Ok(()),
            };
        let env = ka_agent_core::Search {
            provider: provider.to_string(),
            base_url: None,
        }
        .key_env();
        let key = if self.engine_env().contains_key(&env) {
            println!("✓ {env} already saved");
            None
        } else if let Some(v) = env_lookup(&env)
            && prompt::yes_no(&format!("Use {env} from your environment?"), true)?
        {
            Some(v)
        } else {
            let k = prompt::secret(&env)?;
            if k.trim().is_empty() {
                println!("no key — web search stays off");
                return Ok(());
            }
            Some(k.trim().to_string())
        };
        self.search = Some((provider.to_string(), key));
        Ok(())
    }

    /// Optional voice-note transcription (OpenAI, Groq or a local server).
    pub(super) fn step_voice(&mut self) -> Result<(), PromptError> {
        let options = vec![
            "OpenAI Whisper (OPENAI_API_KEY)".to_string(),
            "Groq Whisper — fast, generous free tier (GROQ_API_KEY)".to_string(),
            "A Whisper server on this machine/network".to_string(),
            "Not now".to_string(),
        ];
        let voice = match prompt::choice("Understand voice notes from your phone?", &options, 3)? {
            0 => ka_agent_core::Voice {
                provider: "openai".into(),
                ..ka_agent_core::Voice::default()
            },
            1 => ka_agent_core::Voice {
                provider: "groq".into(),
                ..ka_agent_core::Voice::default()
            },
            2 => ka_agent_core::Voice {
                provider: "local".into(),
                base_url: Some(prompt::text(
                    "Server base URL (OpenAI-compatible)",
                    Some("http://127.0.0.1:8000/v1"),
                )?),
                model: None,
            },
            _ => return Ok(()),
        };
        let key = match voice.key_env() {
            None => None,
            Some(env) if self.engine_env().contains_key(env) => {
                println!("✓ {env} already saved");
                None
            }
            Some(env) => match env_lookup(env) {
                Some(v) if prompt::yes_no(&format!("Use {env} from your environment?"), true)? => {
                    Some(v)
                }
                _ => {
                    let k = prompt::secret(env)?;
                    if k.trim().is_empty() {
                        println!("no key — voice notes stay off");
                        return Ok(());
                    }
                    Some(k.trim().to_string())
                }
            },
        };
        self.voice = Some((voice, key));
        Ok(())
    }

    /// Optional daily briefing (one morning message, not a heartbeat).
    pub(super) fn step_briefing(&mut self) -> Result<(), PromptError> {
        let options = vec![
            "Yes, at 07:30".to_string(),
            "Yes, at another time".to_string(),
            "Not now".to_string(),
        ];
        self.briefing = match prompt::choice(
            "A short briefing every morning (calendar, reminders, what waits for you)?",
            &options,
            2,
        )? {
            0 => Some("07:30".to_string()),
            1 => loop {
                let t = prompt::text("Time (HH:MM, 24h)", Some("08:00"))?;
                if ka_agent_core::when::parse_hhmm(&t).is_some() {
                    break Some(t);
                }
                println!("✗ use HH:MM, e.g. 07:45");
            },
            _ => None,
        };
        Ok(())
    }
}
