//! MultiPC: one keyboard and mouse across several PCs, file transfer, clipboard sync.

mod clipboard;
mod daemon;
mod net;
mod platform;
mod transfer;
mod web;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use mpc_core::config::{generate_key, parse_key, pretty_key, Config};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "multipc", version, about = "Один набор клавиатуры и мыши для нескольких ПК, передача файлов и общий буфер обмена")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Запустить MultiPC (по умолчанию).
    Run,
    /// Создать настройки. Без --key создаёт новый ключ сопряжения для новой группы.
    Init {
        /// Ключ сопряжения с первого компьютера группы.
        #[arg(long)]
        key: Option<String>,
        /// Имя этого компьютера (по умолчанию имя хоста).
        #[arg(long)]
        name: Option<String>,
        /// Перезаписать существующие настройки.
        #[arg(long)]
        force: bool,
    },
    /// Показать ключ сопряжения, чтобы ввести его на другом компьютере.
    Key,
    /// Отправить файлы или папки на другой компьютер.
    Send {
        /// Имя компьютера-получателя.
        #[arg(long)]
        to: String,
        #[arg(required = true)]
        paths: Vec<PathBuf>,
    },
    /// Показать подключённые компьютеры и передачи.
    Status,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let path = Config::path();
    match cli.command.unwrap_or(Command::Run) {
        Command::Run => {
            let cfg = if path.exists() {
                Config::load_from(&path)?
            } else {
                let cfg = Config { key: generate_key(), ..Default::default() };
                cfg.save_to(&path)?;
                cfg
            };
            tracing::info!("MultiPC {} on {:?}, settings in {}", env!("CARGO_PKG_VERSION"), cfg.name, path.display());
            println!(
                "MultiPC запущен. Панель управления: http://127.0.0.1:{}\nЧтобы подключить другой ПК, запустите MultiPC и на нём, затем нажмите «Отправить запрос» в разделе «Устройства в сети».\nНе закрывайте это окно.\n",
                cfg.control_port
            );
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(daemon::run(cfg))
        }
        Command::Init { key, name, force } => {
            if path.exists() && !force {
                anyhow::bail!("настройки уже есть: {} (добавьте --force, чтобы перезаписать)", path.display());
            }
            let key = match key {
                Some(k) => {
                    parse_key(&k)?;
                    k.chars().filter(|c| c.is_ascii_hexdigit()).collect::<String>().to_lowercase()
                }
                None => generate_key(),
            };
            let mut cfg = Config { key, ..Default::default() };
            if let Some(n) = name {
                cfg.name = n;
            }
            cfg.save_to(&path)?;
            println!("Настройки сохранены в {}", path.display());
            println!("Имя компьютера: {}", cfg.name);
            println!("Ключ сопряжения: {}", pretty_key(&cfg.key));
            Ok(())
        }
        Command::Key => {
            let cfg = Config::load_from(&path)?;
            println!("{}", pretty_key(&cfg.key));
            Ok(())
        }
        Command::Send { to, paths } => {
            let cfg = Config::load_from(&path)?;
            let paths: Vec<PathBuf> = paths
                .iter()
                .map(|p| std::fs::canonicalize(p).with_context(|| format!("не найдено: {}", p.display())))
                .collect::<Result<_>>()?;
            let body = serde_json::json!({ "peer": to, "paths": paths }).to_string();
            web::client::request(&Config::dir(), cfg.control_port, "POST", "/api/send", Some(&body))?;
            println!("Отправка на {to} началась; ход передачи виден в multipc status и в панели http://127.0.0.1:{}", cfg.control_port);
            Ok(())
        }
        Command::Status => {
            let cfg = Config::load_from(&path)?;
            let body = web::client::request(&Config::dir(), cfg.control_port, "GET", "/api/state", None)?;
            let v: serde_json::Value = serde_json::from_str(&body)?;
            println!("Этот компьютер: {}   курсор на: {}", v["name"].as_str().unwrap_or(""), v["cursor_on"].as_str().unwrap_or(""));
            for m in v["machines"].as_array().into_iter().flatten().filter(|m| m["is_self"] == false) {
                println!(
                    "  {}  {}  {}",
                    m["name"].as_str().unwrap_or(""),
                    m["platform"].as_str().unwrap_or(""),
                    m["address"].as_str().unwrap_or("")
                );
            }
            for t in v["transfers"].as_array().into_iter().flatten().rev().take(10) {
                println!(
                    "  {} {} {}  {}/{}  {}",
                    if t["outgoing"] == true { "->" } else { "<-" },
                    t["peer"].as_str().unwrap_or(""),
                    t["name"].as_str().unwrap_or(""),
                    t["done"],
                    t["size"],
                    t["state"].as_str().unwrap_or("")
                );
            }
            Ok(())
        }
    }
}
