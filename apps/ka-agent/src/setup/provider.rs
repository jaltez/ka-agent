//! Setup provider, key and model selection. Split from setup.rs
//! (moves only).

use super::*;

impl Setup {
    pub(super) async fn step_provider(&mut self) -> Result<(), PromptError> {
        if let Some(name) = self.args.provider.clone() {
            match catalog::provider(&name) {
                Some(p) => {
                    self.provider = Some(p);
                    return Ok(());
                }
                None => {
                    println!("unknown provider {name} — ka-agent models lists them");
                    std::process::exit(1);
                }
            }
        }
        let list = catalog::providers();
        // Probe keyless locals once (in parallel); the label says `running`.
        let locals: Vec<(String, bool)> = {
            let probes: Vec<_> = list
                .iter()
                .filter(|p| p.key_env.is_none())
                .map(|p| {
                    let p = p.clone();
                    async move {
                        (
                            p.name.to_string(),
                            matches!(
                                catalog::check_credentials(&p, None).await,
                                CredentialCheck::Ok { .. }
                            ),
                        )
                    }
                })
                .collect();
            futures_join(probes).await
        };
        let options: Vec<String> = list
            .iter()
            .map(|p| {
                let mut label = format!("{} — {}", p.name, p.note);
                if self.key_found(p) {
                    label.push_str(" (key found)");
                }
                if let Some((_, running)) = locals.iter().find(|(n, _)| *n == p.name)
                    && *running
                {
                    label.push_str(" (running)");
                }
                label
            })
            .collect();
        let default = list
            .iter()
            .position(|p| self.key_found(p))
            .or_else(|| {
                locals
                    .iter()
                    .position(|(_, r)| *r)
                    .map(|i| list.iter().position(|p| p.name == locals[i].0).unwrap_or(i))
            })
            .unwrap_or_else(|| list.iter().position(|p| p.name == "anthropic").unwrap_or(0));
        let pick = prompt::choice("Which provider should Ka Agent use?", &options, default)?;
        self.provider = list.get(pick).cloned();
        Ok(())
    }

    pub(super) async fn step_key(&mut self) -> Result<(), PromptError> {
        let Some(p) = self.provider.clone() else {
            return Ok(());
        };
        let Some(env) = p.key_env else {
            self.key_from = "none";
            return Ok(());
        };
        self.key = self.args.key_stdin.then(read_key_stdin).flatten();
        if self.key.is_some() {
            self.key_from = "stdin";
            return Ok(());
        }
        if let Some(saved) = self.engine_env().get(env)
            && prompt::yes_no(&format!("Use the saved {env}?"), true)?
        {
            self.key = Some(saved.clone());
            self.key_from = "saved key";
            return Ok(());
        }
        if let Some(v) = env_lookup(env)
            && prompt::yes_no(
                &format!(
                    "Import {env} from your environment? (the agent runs isolated and can't see your shell's variables)"
                ),
                true,
            )?
        {
            self.key = Some(v);
            self.key_from = "environment";
            return Ok(());
        }
        if let Some(url) = p.key_url {
            println!("Get a key at {url}");
        }
        let key = prompt::secret(&format!("Paste your {} API key (input hidden)", p.name))?;
        self.key = Some(key);
        self.key_from = "typed";
        Ok(())
    }

    /// Returns Err only on prompt failure; a rejected key loops.
    pub(super) async fn step_verify(&mut self) {
        if self.args.no_verify {
            return;
        }
        let Some(p) = self.provider.clone() else {
            return;
        };
        let Some(key) = self.key.clone() else { return };
        println!("checking the key with {}…", p.name);
        loop {
            match catalog::check_credentials(&p, Some(&key)).await {
                CredentialCheck::Ok { models } => {
                    self.live_models = models;
                    println!("✓ key works ({} models available)", self.live_models.len());
                    return;
                }
                CredentialCheck::NoListing { .. } => {
                    println!(
                        "✓ reached {} (it doesn't list models; the first message will confirm the key)",
                        p.name
                    );
                    return;
                }
                CredentialCheck::Rejected { status, message } => {
                    println!("✗ {} rejected the key: HTTP {status} {message}", p.name);
                }
                CredentialCheck::Unreachable { message } => {
                    println!("✗ couldn't reach {}: {message}", p.base_url);
                }
            }
            let options = vec![
                "Enter the key again".to_string(),
                "Keep it anyway".to_string(),
                "Choose another provider".to_string(),
            ];
            match prompt::choice("", &options, 0) {
                Ok(0) => {
                    match prompt::secret(&format!("Paste your {} API key (input hidden)", p.name)) {
                        Ok(k) => self.key = Some(k),
                        Err(e) => {
                            let _ = prompt_err(e);
                            std::process::exit(1);
                        }
                    }
                }
                Ok(1) => return,
                Ok(_) => {
                    self.provider = None;
                    self.key = None;
                    if self.step_provider().await.is_err() || self.step_key().await.is_err() {
                        std::process::exit(1);
                    }
                    if self.args.no_verify {
                        return;
                    }
                }
                Err(e) => {
                    let _ = prompt_err(e);
                    std::process::exit(1);
                }
            }
        }
    }

    pub(super) async fn step_model(&mut self) -> Result<(), PromptError> {
        if let Some(m) = self.args.model.clone() {
            match catalog::validate_selector(&m) {
                Ok(info) => {
                    self.model = Some(info.id);
                    return Ok(());
                }
                Err(e) => {
                    println!("{e}");
                    std::process::exit(1);
                }
            }
        }
        let Some(p) = self.provider.clone() else {
            return Ok(());
        };
        let mut options: Vec<String> = Vec::new();
        if let Some(d) = catalog::default_model(p.name) {
            options.push(d.to_string());
        }
        for m in catalog::catalog_models(p.name) {
            if !options.contains(&m) {
                options.push(m);
            }
        }
        for id in &self.live_models {
            let full = format!("{}/{}", p.name, id);
            if !options.contains(&full) {
                options.push(full);
            }
        }
        options.truncate(12);
        options.push("Type a model id".to_string());
        loop {
            let pick = prompt::choice("Which model?", &options, 0)?;
            if pick + 1 == options.len() {
                let typed = prompt::text(
                    "Model id (provider/model, e.g. anthropic/claude-sonnet-5)",
                    None,
                )?;
                match catalog::validate_selector(&typed) {
                    Ok(info) => {
                        self.model = Some(info.id);
                        return Ok(());
                    }
                    Err(e) => {
                        println!("{e}");
                        continue;
                    }
                }
            }
            let chosen = options[pick].clone();
            match catalog::validate_selector(&chosen) {
                Ok(info) => {
                    self.model = Some(info.id);
                    return Ok(());
                }
                Err(e) => println!("{e}"),
            }
        }
    }
}

/// Simple join of async probes (no futures crate: poll sequentially inside
/// one task — the probes are localhost-fast and bounded by the 8 s timeout).
async fn futures_join<F>(probes: Vec<F>) -> Vec<(String, bool)>
where
    F: Future<Output = (String, bool)>,
{
    let mut out = Vec::with_capacity(probes.len());
    for p in probes {
        out.push(p.await);
    }
    out
}
