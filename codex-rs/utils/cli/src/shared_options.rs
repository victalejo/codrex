//! Shared command-line flags used by both interactive and non-interactive Codex entry points.

use crate::SandboxModeCliArg;
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug, Default)]
pub struct SharedCliOptions {
    /// Optional image(s) to attach to the initial prompt.
    #[arg(
        long = "image",
        short = 'i',
        value_name = "FILE",
        value_delimiter = ',',
        num_args = 1..
    )]
    pub images: Vec<PathBuf>,

    /// Model the agent should use. Accepts:
    ///   - bare model name (uses the default provider): `gpt-5.1`
    ///   - `provider/model` syntax to select both at once: `minimax/MiniMax-M2.7`
    ///
    /// CLI flags take precedence over config.toml. The `provider/model`
    /// prefix and the explicit `--provider` flag are mutually exclusive.
    #[arg(long, short = 'm')]
    pub model: Option<String>,

    /// Use open-source provider.
    #[arg(long = "oss", default_value_t = false)]
    pub oss: bool,

    /// Specify which local provider to use (lmstudio or ollama).
    /// If not specified with --oss, will use config default or show selection.
    #[arg(long = "local-provider")]
    pub oss_provider: Option<String>,

    /// Explicit model provider id (e.g. `openai`, `minimax`, `ollama`).
    ///
    /// Equivalent to using the `provider/model` prefix on `--model`. Cannot
    /// be combined with that prefix or with `--oss`.
    #[arg(long = "provider", value_name = "PROVIDER")]
    pub model_provider: Option<String>,

    /// Configuration profile from config.toml to specify default options.
    #[arg(long = "profile", short = 'p')]
    pub config_profile: Option<String>,

    /// Select the sandbox policy to use when executing model-generated shell
    /// commands.
    #[arg(long = "sandbox", short = 's')]
    pub sandbox_mode: Option<SandboxModeCliArg>,

    /// Convenience alias for low-friction sandboxed automatic execution.
    #[arg(long = "full-auto", default_value_t = false)]
    pub full_auto: bool,

    /// Skip all confirmation prompts and execute commands without sandboxing.
    /// EXTREMELY DANGEROUS. Intended solely for running in environments that are externally sandboxed.
    #[arg(
        long = "dangerously-bypass-approvals-and-sandbox",
        alias = "yolo",
        default_value_t = false,
        conflicts_with = "full_auto"
    )]
    pub dangerously_bypass_approvals_and_sandbox: bool,

    /// Tell the agent to use the specified directory as its working root.
    #[clap(long = "cd", short = 'C', value_name = "DIR")]
    pub cwd: Option<PathBuf>,

    /// Additional directories that should be writable alongside the primary workspace.
    #[arg(long = "add-dir", value_name = "DIR", value_hint = clap::ValueHint::DirPath)]
    pub add_dir: Vec<PathBuf>,
}

/// Result of resolving the model + provider from the CLI flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelSelection {
    /// The bare model name (no provider prefix), if one was supplied.
    pub model: Option<String>,
    /// The explicit provider id, if one was supplied via either the
    /// `provider/model` prefix or the `--provider` flag.
    pub provider: Option<String>,
}

/// Errors produced when reconciling the `--model`, `--provider`, and
/// `--oss` flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSelectionError {
    EmptyProviderPrefix(String),
    EmptyModelAfterPrefix(String),
    TooManySlashes(String),
    ProviderConflict {
        prefix: String,
        explicit: String,
    },
    OssConflict(String),
}

impl std::fmt::Display for ModelSelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyProviderPrefix(raw) => {
                write!(
                    f,
                    "invalid --model {raw:?}: provider prefix before '/' is empty"
                )
            }
            Self::EmptyModelAfterPrefix(raw) => {
                write!(
                    f,
                    "invalid --model {raw:?}: model name after '/' is empty"
                )
            }
            Self::TooManySlashes(raw) => {
                write!(
                    f,
                    "invalid --model {raw:?}: expected 'provider/model' (one '/'), got multiple"
                )
            }
            Self::ProviderConflict { prefix, explicit } => {
                write!(
                    f,
                    "conflicting providers: --model uses prefix {prefix:?} but --provider is {explicit:?}"
                )
            }
            Self::OssConflict(detail) => {
                write!(
                    f,
                    "--oss cannot be combined with an explicit provider ({detail})"
                )
            }
        }
    }
}

impl std::error::Error for ModelSelectionError {}

/// Split a `--model` value into `(provider, model)` if it has the
/// `provider/model` prefix syntax. Returns `Ok(None)` when there is no
/// slash in the input. Errors on malformed input (empty halves, multiple
/// slashes).
fn split_provider_model(raw: &str) -> Result<Option<(String, String)>, ModelSelectionError> {
    if !raw.contains('/') {
        return Ok(None);
    }
    if raw.matches('/').count() > 1 {
        return Err(ModelSelectionError::TooManySlashes(raw.to_string()));
    }
    let (provider, model) = raw
        .split_once('/')
        .expect("verified that the string contains exactly one '/'");
    let provider = provider.trim();
    let model = model.trim();
    if provider.is_empty() {
        return Err(ModelSelectionError::EmptyProviderPrefix(raw.to_string()));
    }
    if model.is_empty() {
        return Err(ModelSelectionError::EmptyModelAfterPrefix(raw.to_string()));
    }
    Ok(Some((provider.to_string(), model.to_string())))
}

impl SharedCliOptions {
    /// Reconcile `--model`, `--provider`, and `--oss` into a single
    /// `(model, provider)` pair. Errors when the flags conflict.
    ///
    /// Precedence and conflict rules:
    /// - `provider/model` prefix and `--provider` must agree (both can be
    ///   omitted, both can be present with the same value, or only one
    ///   can be present); mismatched values are an error.
    /// - `--oss` cannot be combined with the prefix or with `--provider`;
    ///   the OSS path is selected through `--local-provider`.
    pub fn resolve_model_and_provider(
        &self,
    ) -> Result<ResolvedModelSelection, ModelSelectionError> {
        let split = match self.model.as_deref() {
            Some(raw) => split_provider_model(raw)?,
            None => None,
        };

        let (model, prefix_provider) = match (split, self.model.clone()) {
            (Some((p, m)), _) => (Some(m), Some(p)),
            (None, raw) => (raw, None),
        };

        let explicit = self.model_provider.clone();
        let provider = match (prefix_provider, explicit) {
            (None, None) => None,
            (Some(p), None) | (None, Some(p)) => Some(p),
            (Some(p), Some(e)) if p == e => Some(p),
            (Some(p), Some(e)) => {
                return Err(ModelSelectionError::ProviderConflict {
                    prefix: p,
                    explicit: e,
                });
            }
        };

        if self.oss && provider.is_some() {
            return Err(ModelSelectionError::OssConflict(format!(
                "{:?} was set",
                provider.unwrap_or_default()
            )));
        }

        Ok(ResolvedModelSelection { model, provider })
    }

    pub fn inherit_exec_root_options(&mut self, root: &Self) {
        let self_selected_sandbox_mode = self.sandbox_mode.is_some()
            || self.full_auto
            || self.dangerously_bypass_approvals_and_sandbox;
        let Self {
            images,
            model,
            oss,
            oss_provider,
            model_provider,
            config_profile,
            sandbox_mode,
            full_auto,
            dangerously_bypass_approvals_and_sandbox,
            cwd,
            add_dir,
        } = self;
        let Self {
            images: root_images,
            model: root_model,
            oss: root_oss,
            oss_provider: root_oss_provider,
            model_provider: root_model_provider,
            config_profile: root_config_profile,
            sandbox_mode: root_sandbox_mode,
            full_auto: root_full_auto,
            dangerously_bypass_approvals_and_sandbox: root_dangerously_bypass_approvals_and_sandbox,
            cwd: root_cwd,
            add_dir: root_add_dir,
        } = root;

        if model.is_none() {
            model.clone_from(root_model);
        }
        if *root_oss {
            *oss = true;
        }
        if oss_provider.is_none() {
            oss_provider.clone_from(root_oss_provider);
        }
        if model_provider.is_none() {
            model_provider.clone_from(root_model_provider);
        }
        if config_profile.is_none() {
            config_profile.clone_from(root_config_profile);
        }
        if sandbox_mode.is_none() {
            *sandbox_mode = *root_sandbox_mode;
        }
        if !self_selected_sandbox_mode {
            *full_auto = *root_full_auto;
            *dangerously_bypass_approvals_and_sandbox =
                *root_dangerously_bypass_approvals_and_sandbox;
        }
        if cwd.is_none() {
            cwd.clone_from(root_cwd);
        }
        if !root_images.is_empty() {
            let mut merged_images = root_images.clone();
            merged_images.append(images);
            *images = merged_images;
        }
        if !root_add_dir.is_empty() {
            let mut merged_add_dir = root_add_dir.clone();
            merged_add_dir.append(add_dir);
            *add_dir = merged_add_dir;
        }
    }

    pub fn apply_subcommand_overrides(&mut self, subcommand: Self) {
        let subcommand_selected_sandbox_mode = subcommand.sandbox_mode.is_some()
            || subcommand.full_auto
            || subcommand.dangerously_bypass_approvals_and_sandbox;
        let Self {
            images,
            model,
            oss,
            oss_provider,
            model_provider,
            config_profile,
            sandbox_mode,
            full_auto,
            dangerously_bypass_approvals_and_sandbox,
            cwd,
            add_dir,
        } = subcommand;

        if let Some(model) = model {
            self.model = Some(model);
        }
        if oss {
            self.oss = true;
        }
        if let Some(oss_provider) = oss_provider {
            self.oss_provider = Some(oss_provider);
        }
        if let Some(model_provider) = model_provider {
            self.model_provider = Some(model_provider);
        }
        if let Some(config_profile) = config_profile {
            self.config_profile = Some(config_profile);
        }
        if subcommand_selected_sandbox_mode {
            self.sandbox_mode = sandbox_mode;
            self.full_auto = full_auto;
            self.dangerously_bypass_approvals_and_sandbox =
                dangerously_bypass_approvals_and_sandbox;
        }
        if let Some(cwd) = cwd {
            self.cwd = Some(cwd);
        }
        if !images.is_empty() {
            self.images = images;
        }
        if !add_dir.is_empty() {
            self.add_dir.extend(add_dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> SharedCliOptions {
        SharedCliOptions::default()
    }

    #[test]
    fn bare_model_passes_through_with_no_provider() {
        let mut o = opts();
        o.model = Some("gpt-5.1".into());
        let r = o.resolve_model_and_provider().expect("ok");
        assert_eq!(r.model.as_deref(), Some("gpt-5.1"));
        assert!(r.provider.is_none());
    }

    #[test]
    fn provider_slash_model_splits_cleanly() {
        let mut o = opts();
        o.model = Some("minimax/MiniMax-M2.7".into());
        let r = o.resolve_model_and_provider().expect("ok");
        assert_eq!(r.model.as_deref(), Some("MiniMax-M2.7"));
        assert_eq!(r.provider.as_deref(), Some("minimax"));
    }

    #[test]
    fn explicit_provider_flag_alone_works() {
        let mut o = opts();
        o.model = Some("MiniMax-M2.7".into());
        o.model_provider = Some("minimax".into());
        let r = o.resolve_model_and_provider().expect("ok");
        assert_eq!(r.model.as_deref(), Some("MiniMax-M2.7"));
        assert_eq!(r.provider.as_deref(), Some("minimax"));
    }

    #[test]
    fn matching_prefix_and_explicit_provider_collapses_to_one() {
        let mut o = opts();
        o.model = Some("minimax/MiniMax-M2.7".into());
        o.model_provider = Some("minimax".into());
        let r = o.resolve_model_and_provider().expect("ok");
        assert_eq!(r.provider.as_deref(), Some("minimax"));
    }

    #[test]
    fn conflicting_prefix_and_provider_errors() {
        let mut o = opts();
        o.model = Some("minimax/MiniMax-M2.7".into());
        o.model_provider = Some("openai".into());
        let err = o.resolve_model_and_provider().expect_err("conflict");
        assert!(matches!(err, ModelSelectionError::ProviderConflict { .. }));
        let msg = format!("{err}");
        assert!(msg.contains("minimax"));
        assert!(msg.contains("openai"));
    }

    #[test]
    fn empty_provider_prefix_errors() {
        let mut o = opts();
        o.model = Some("/MiniMax-M2.7".into());
        let err = o.resolve_model_and_provider().expect_err("empty prefix");
        assert!(matches!(err, ModelSelectionError::EmptyProviderPrefix(_)));
    }

    #[test]
    fn empty_model_after_prefix_errors() {
        let mut o = opts();
        o.model = Some("minimax/".into());
        let err = o.resolve_model_and_provider().expect_err("empty model");
        assert!(matches!(err, ModelSelectionError::EmptyModelAfterPrefix(_)));
    }

    #[test]
    fn multiple_slashes_errors() {
        let mut o = opts();
        o.model = Some("minimax/MiniMax-M2.7/extra".into());
        let err = o.resolve_model_and_provider().expect_err("too many");
        assert!(matches!(err, ModelSelectionError::TooManySlashes(_)));
    }

    #[test]
    fn oss_with_explicit_provider_errors() {
        let mut o = opts();
        o.oss = true;
        o.model = Some("MiniMax-M2.7".into());
        o.model_provider = Some("minimax".into());
        let err = o.resolve_model_and_provider().expect_err("oss conflict");
        assert!(matches!(err, ModelSelectionError::OssConflict(_)));
    }

    #[test]
    fn oss_with_prefix_errors() {
        let mut o = opts();
        o.oss = true;
        o.model = Some("minimax/MiniMax-M2.7".into());
        let err = o.resolve_model_and_provider().expect_err("oss conflict");
        assert!(matches!(err, ModelSelectionError::OssConflict(_)));
    }

    #[test]
    fn oss_alone_does_not_error() {
        let mut o = opts();
        o.oss = true;
        o.model = Some("llama3".into());
        let r = o.resolve_model_and_provider().expect("ok");
        assert_eq!(r.model.as_deref(), Some("llama3"));
        assert!(r.provider.is_none());
    }

    #[test]
    fn no_model_no_provider_no_flags_returns_empty() {
        let r = opts().resolve_model_and_provider().expect("ok");
        assert!(r.model.is_none());
        assert!(r.provider.is_none());
    }

    #[test]
    fn whitespace_around_prefix_is_trimmed() {
        let mut o = opts();
        o.model = Some(" minimax / MiniMax-M2.7 ".into());
        let r = o.resolve_model_and_provider().expect("ok");
        assert_eq!(r.provider.as_deref(), Some("minimax"));
        assert_eq!(r.model.as_deref(), Some("MiniMax-M2.7"));
    }
}
