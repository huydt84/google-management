use clap::{Parser, Subcommand};

use gm::{config::Config, crypto::SecretBox, rules::RuleSet, secrets};

#[derive(Parser)]
#[command(name = "gm", about = "Google Management", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the web server and background workers.
    Serve,
    /// Apply database migrations and exit.
    Migrate,
    /// Print a fresh random 32-byte key (for master-key and session-secret).
    GenKey,
    /// Manage secrets in the macOS Keychain.
    Keychain {
        #[command(subcommand)]
        cmd: KeychainCmd,
    },
    /// Validate a rules file and check every rule's examples.
    Rules {
        #[arg(default_value = "rules.yaml")]
        path: std::path::PathBuf,
    },
    /// Show what is installed and running: launchd agents, the running build, web, model,
    /// migrations and the queue. Exit code 1 when something is broken.
    Status,
    /// Run rules, model triage and summaries on the local eval set against the local model.
    Eval {
        /// Eval set folder (default: GM_EVAL_DIR or ~/.local/share/gm/eval).
        #[arg(long)]
        dir: Option<std::path::PathBuf>,
        /// Output language for summaries (default: GM_DEFAULT_SUMMARY_LANGUAGE).
        #[arg(long)]
        language: Option<String>,
    },
}

#[derive(Subcommand)]
enum KeychainCmd {
    /// Store a secret (you are prompted for the value).
    Set { name: String },
    /// Show which known secrets are present (values are never printed).
    List,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve => {
            let cfg = Config::from_env()?;
            let _guard = gm::app::init_tracing(cfg.log_dir.as_deref());
            gm::app::serve(cfg).await
        }
        Cmd::Migrate => {
            let _ = dotenvy::dotenv();
            let url = std::env::var("GM_DATABASE_URL")
                .unwrap_or_else(|_| "postgres://gm:gm@127.0.0.1:5433/gm".into());
            let pool = gm::db::connect(&url).await?;
            gm::db::migrate(&pool).await?;
            println!("migrations applied");
            Ok(())
        }
        Cmd::GenKey => {
            println!("{}", SecretBox::generate_key());
            Ok(())
        }
        Cmd::Keychain {
            cmd: KeychainCmd::Set { name },
        } => {
            if !secrets::KNOWN.iter().any(|(n, _)| *n == name) {
                anyhow::bail!(
                    "unknown secret {name:?}; known: {:?}",
                    secrets::KNOWN.iter().map(|(n, _)| n).collect::<Vec<_>>()
                );
            }
            secrets::keychain_set_interactive(&name)?;
            println!("stored {name}");
            Ok(())
        }
        Cmd::Keychain {
            cmd: KeychainCmd::List,
        } => {
            for (name, desc) in secrets::KNOWN {
                let mark = if secrets::keychain_has(name) {
                    "✓"
                } else {
                    "·"
                };
                println!("{mark} {name:22} {desc}");
            }
            Ok(())
        }
        Cmd::Rules { path } => {
            let rs = RuleSet::parse(&std::fs::read_to_string(&path)?)?;
            let errors = rs.check_examples();
            let examples: usize = rs.rules.iter().map(|r| r.examples.len()).sum();
            if errors.is_empty() {
                println!(
                    "{}: {} rules, {examples} examples OK",
                    path.display(),
                    rs.rules.len()
                );
                Ok(())
            } else {
                for e in &errors {
                    eprintln!("✗ {e}");
                }
                anyhow::bail!("{} example(s) failed", errors.len())
            }
        }
        Cmd::Status => {
            let cfg = Config::from_env_without_secrets()?;
            let checks = gm::status::collect(&cfg).await;
            print!("{}", gm::status::format(&checks));
            if checks.iter().any(|c| c.state == gm::status::State::Fail) {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::Eval { dir, language } => {
            let cfg = Config::from_env_without_secrets()?;
            gm::eval::ensure_local(&cfg.llm_base_url)?;
            let dir = dir.unwrap_or_else(|| cfg.eval_dir.clone());
            let cases = gm::eval::load(&dir)?;
            anyhow::ensure!(
                !cases.is_empty(),
                "no eval cases in {} (add them from the dashboard)",
                dir.display()
            );
            let rules = RuleSet::parse(&std::fs::read_to_string(&cfg.rules_path)?)?;
            let language = language.unwrap_or_else(|| cfg.default_summary_language.clone());
            let mut llm = gm::llm::LlmClient::new(&cfg, None)?;
            let calls = llm.record_calls();
            let report = gm::eval::run(&llm, &calls, &rules, &cases, &language).await?;
            print!("{}", gm::eval::format_report(&report, &cfg.llm_model));
            Ok(())
        }
    }
}
