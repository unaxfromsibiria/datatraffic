use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{CommandFactory, Parser};
use datatraffic::config::generate_key;
use datatraffic::settings::Settings;
use datatraffic::TrafficClient;
use datatraffic::{Bytes, DataFile};

/// Demo client + server for exchanging encrypted byte streams (files) between projects over Redis.
#[derive(Parser, Debug)]
#[command(
    name = "hubdatatraffic",
    about = "Send and receive encrypted byte streams (files) between projects via Redis pub/sub."
)]
struct Cli {
    /// Redis connection URL; overrides the REDIS_URL environment variable.
    #[arg(long)]
    redis: Option<String>,

    /// AES-256-GCM key in hex (32 bytes / 64 chars); overrides CRYPTO_KEY.
    #[arg(long)]
    key: Option<String>,

    /// Redis password; overrides REDIS_PASSWORD env. Omit for no authentication.
    #[arg(long)]
    password: Option<String>,

    /// Client name; overrides CLIENT_NAME env or random generation.
    #[arg(long)]
    client: Option<String>,

    /// Project topics to subscribe to; enables server mode.
    #[arg(long, num_args = 1..)]
    listen: Vec<String>,

    /// Directory to save received files into (server/listen mode).
    #[arg(long)]
    out_dir: Option<PathBuf>,

    /// Path of the file to send; enables client mode.
    #[arg(long)]
    send_file: Option<PathBuf>,

    /// Note attached to the sent payload.
    #[arg(long)]
    msg: Option<String>,

    /// Target project topic for the sent payload (client mode).
    #[arg(long)]
    project: Option<String>,

    /// Print a random 32-byte key in hex and exit.
    #[arg(long)]
    genkey: bool,
}

/// Resolve Settings from CLI args + process environment.
fn resolve_settings(cli: &Cli) -> Result<Settings, String> {
    let env_redis = std::env::var("REDIS_URL").ok();
    let env_key = std::env::var("CRYPTO_KEY").ok();
    let env_client = std::env::var("CLIENT_NAME").ok();
    let env_password = std::env::var("REDIS_PASSWORD").ok();

    // CLI overrides take priority; pass None to fall through to env.
    let redis_url = cli.redis.as_deref().or(env_redis.as_deref());
    let key_hex = cli.key.as_deref().or(env_key.as_deref());
    let client_name = cli.client.as_deref().or(env_client.as_deref());
    let password = cli.password.as_deref().or(env_password.as_deref());

    Settings::resolve(redis_url, key_hex, client_name, password)
}

/// Server mode: subscribe to project topics and log incoming files.
async fn run_server(cli: &Cli) -> Result<(), String> {
    let settings = resolve_settings(cli)?;
    let projects = cli.listen.clone();
    let out_dir = cli.out_dir.clone();

    info!("starting server as '{}' watching {:?}", settings.client_name, projects);
    let mut client = TrafficClient::new(settings, projects).await.map_err(
        |e| format!("failed to create traffic client: {e}")
    )?;

    // Loop reading from inbox until the channel is closed.
    while let Some((project, file)) = client.inbox().recv().await {
        info!(
            "received file '{}' ({} bytes) on project '{}' (note: '{}')",
            file.name,
            file.data.len(),
            project,
            file.note
        );
        if let Some(dir) = &out_dir {
            save_file(dir, &file)?;
        }
    }

    Ok(())
}

/// Persist a received DataFile to `<out_dir>/<name>`, creating the directory if needed.
fn save_file(out_dir: &Path, file: &DataFile) -> Result<(), String> {
    std::fs::create_dir_all(out_dir).map_err(
        |e| format!("failed to create {}: {e}", out_dir.display())
    )?;
    let path = out_dir.join(&file.name);
    std::fs::write(&path, file.data.as_ref()).map_err(
        |e| format!("failed to write {}: {e}", path.display())
    )?;
    info!("saved to {}", path.display());
    Ok(())
}

/// Client mode: read the file and publish it as a DataFile on the project topic.
async fn run_client(cli: &Cli) -> Result<(), String> {
    let settings = resolve_settings(cli)?;
    let send_file = cli.send_file.as_ref().expect("validated in main");
    let project = cli.project.as_ref().expect("validated in main").clone();

    info!("starting client as '{}'", settings.client_name);
    let traffic = TrafficClient::new(settings, vec![]).await.map_err(
        |e| format!("failed to create traffic client: {e}")
    )?;

    let data = std::fs::read(send_file).map_err(
        |e| format!("failed to read {}: {}", send_file.display(), e)
    )?;
    let name = send_file.file_name().and_then(
        |n| n.to_str()
    ).unwrap_or("<unknown>").to_string();

    let data_file = DataFile {
        name,
        note: cli.msg.clone().unwrap_or_default(),
        data: Bytes::from(data),
    };

    info!(
        "sending '{}' ({} bytes) to project '{}'",
        data_file.name,
        data_file.data.len(),
        project
    );

    let tx = traffic.outbox();
    tx.send((project.clone(), data_file)).await.map_err(|_| "outbox closed")?;

    // Give the background publisher a moment to flush.
    tokio::time::sleep(Duration::from_millis(200)).await;
    info!("done");
    Ok(())
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let cli = Cli::parse();

    if cli.genkey {
        println!("{}", generate_key());
        return;
    }

    // Mode selection: --listen enables server mode, otherwise client mode.
    if !cli.listen.is_empty() {
        exit_on_error(run_server(&cli).await);
        return;
    }

    let has_client_args = cli.send_file.is_some() || cli.project.is_some();
    if !has_client_args {
        let _ = Cli::command().print_help();
        eprintln!();
        std::process::exit(1);
    }

    if cli.send_file.is_none() || cli.project.is_none() {
        eprintln!("Error: client mode requires both --send-file <path> and --project <name>");
        std::process::exit(1);
    }

    exit_on_error(run_client(&cli).await);
}

fn exit_on_error(result: Result<(), String>) {
    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

use log::info;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    // --- CLI parsing -------------------------------------------------------

    #[test]
    fn cli_genkey_flag() {
        let cli = parse(&["hubdatatraffic", "--genkey"]).unwrap();
        assert!(cli.genkey);
    }

    #[test]
    fn cli_listen_multiple_projects() {
        let cli = parse(&["hubdatatraffic", "--listen", "alpha", "beta", "gamma"]).unwrap();
        assert_eq!(cli.listen, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn cli_client_args() {
        let cli = parse(&[
            "hubdatatraffic",
            "--send-file",
            "/tmp/data.bin",
            "--project",
            "alpha",
            "--msg",
            "hello",
        ]).unwrap();
        assert_eq!(cli.send_file.as_deref(), Some(Path::new("/tmp/data.bin")));
        assert_eq!(cli.project.as_deref(), Some("alpha"));
        assert_eq!(cli.msg.as_deref(), Some("hello"));
    }

    #[test]
    fn cli_redis_and_key_overrides() {
        let key = "c".repeat(64);
        let cli = parse(&[
            "hubdatatraffic",
            "--redis",
            "redis://localhost:6379",
            "--key",
            &key,
        ]).unwrap();
        assert_eq!(cli.redis.as_deref(), Some("redis://localhost:6379"));
        assert_eq!(cli.key.as_deref().map(|s| s.len()), Some(64));
    }

    #[test]
    fn cli_client_name_flag() {
        let cli = parse(&["hubdatatraffic", "--client", "my-node"]).unwrap();
        assert_eq!(cli.client.as_deref(), Some("my-node"));
    }

    #[test]
    fn cli_out_dir_and_password_flags() {
        let cli = parse(&[
            "hubdatatraffic",
            "--listen",
            "alpha",
            "--out-dir",
            "./received",
            "--password",
            "s3cret",
        ]).unwrap();
        assert_eq!(cli.out_dir.as_deref(), Some(Path::new("./received")));
        assert_eq!(cli.password.as_deref(), Some("s3cret"));
    }

    #[test]
    fn cli_unknown_flag_rejected() {
        assert!(parse(&["hubdatatraffic", "--nope"]).is_err());
    }

    // --- Settings resolution (CLI > env) ------------------------------------

    #[test]
    fn settings_cli_overrides_env() {
        unsafe {
            std::env::set_var("REDIS_URL", "redis://from-env");
            std::env::set_var("CRYPTO_KEY", &"a".repeat(64));
            std::env::remove_var("CLIENT_NAME");
        }

        let cli = Cli {
            redis: Some("redis://from-cli".into()),
            key: Some("b".repeat(64)),
            password: Some("pw-cli".into()),
            client: Some("cli-client".into()),
            listen: vec![],
            out_dir: None,
            send_file: None,
            msg: None,
            project: None,
            genkey: false,
        };

        let s = resolve_settings(&cli).unwrap();
        assert_eq!(s.redis_url, "redis://from-cli");
        assert_eq!(s.key, [0xbb; 32]);
        assert_eq!(s.client_name, "cli-client");

        unsafe {
            std::env::remove_var("REDIS_URL");
            std::env::remove_var("CRYPTO_KEY");
        }
    }

    #[test]
    fn settings_falls_back_to_env() {
        unsafe {
            std::env::set_var("REDIS_URL", "redis://from-env");
            std::env::set_var("CRYPTO_KEY", &"c".repeat(64));
            std::env::remove_var("CLIENT_NAME");
        }

        let cli = Cli {
            redis: None,
            key: None,
            password: None,
            client: None,
            listen: vec![],
            out_dir: None,
            send_file: None,
            msg: None,
            project: None,
            genkey: false,
        };

        let s = resolve_settings(&cli).unwrap();
        assert_eq!(s.redis_url, "redis://from-env");
        assert_eq!(s.key, [0xcc; 32]);
        // auto-generated name is 8 hex chars
        assert_eq!(s.client_name.len(), 8);

        unsafe {
            std::env::remove_var("REDIS_URL");
            std::env::remove_var("CRYPTO_KEY");
        }
    }

    #[test]
    fn settings_password_cli_overrides_env() {
        unsafe {
            std::env::set_var("REDIS_URL", "redis://from-env");
            std::env::set_var("CRYPTO_KEY", &"c".repeat(64));
            std::env::set_var("REDIS_PASSWORD", "pw-from-env");
        }

        let cli = Cli {
            redis: None,
            key: None,
            password: Some("pw-cli".into()),
            client: None,
            listen: vec![],
            out_dir: None,
            send_file: None,
            msg: None,
            project: None,
            genkey: false,
        };

        let s = resolve_settings(&cli).unwrap();
        assert_eq!(s.password.as_deref(), Some("pw-cli"));

        unsafe {
            std::env::remove_var("REDIS_URL");
            std::env::remove_var("CRYPTO_KEY");
            std::env::remove_var("REDIS_PASSWORD");
        }
    }

    #[test]
    fn settings_missing_all_is_error() {
        unsafe {
            std::env::remove_var("REDIS_URL");
            std::env::remove_var("CRYPTO_KEY");
        }

        let cli = Cli {
            redis: None,
            key: None,
            password: None,
            client: None,
            listen: vec![],
            out_dir: None,
            send_file: None,
            msg: None,
            project: None,
            genkey: false,
        };

        assert!(resolve_settings(&cli).is_err());
    }

    // --- Key generation ----------------------------------------------------

    #[test]
    fn genkey_produces_64_hex_chars() {
        let key = generate_key();
        assert_eq!(key.len(), 64);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
