use std::{fmt, path::PathBuf, process::Stdio, sync::Arc};

use color_eyre::eyre::{self, ensure, eyre};
use serde::Deserialize;
use teloxide::{
    dptree,
    prelude::{Dispatcher, Request, Requester},
    types::{Message, MessageId, Update, UpdateKind},
    ApiError, Bot, RequestError,
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    process::Command,
    sync::Semaphore,
};
use tracing_subscriber::EnvFilter;

macro_rules! or_ok {
    ($x:expr) => {{
        match $x {
            Some(x) => x,
            None => return Ok(()),
        }
    }};
}

#[derive(Deserialize)]
#[serde(transparent)]
struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(hidden)")
    }
}

#[derive(Debug, Deserialize)]
struct Config {
    token: Token,
    db_path: PathBuf,
    #[serde(default = "default_max_parallel")]
    max_parallel: usize,
    // set by Nix
    bwrap: PathBuf,
    perl: PathBuf,
    prlimit: PathBuf,
    timeout: PathBuf,
    allow_dirs: Vec<PathBuf>,
}

fn default_max_parallel() -> usize {
    16
}

async fn run_perl(
    exprs: impl IntoIterator<Item = &str>,
    input: &str,
    cfg: &Config,
    full: bool,
) -> eyre::Result<String> {
    let mut cmd = Command::new(&cfg.timeout);

    #[rustfmt::skip]
    cmd
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .env_clear()
    .env("LANG", "C")
    .args(["--signal", "TERM", "--kill-after", "1s", "0.5s"])
    .arg(&cfg.prlimit).args(["--memlock=65535", "--rss=4194304", "--cpu=2"])
    .arg(&cfg.bwrap).args(["--unshare-all", "--proc", "/proc", "--dev", "/dev"])
                    .args(cfg.allow_dirs.iter().flat_map(|dir| ["--ro-bind".as_ref(), dir.as_os_str(), dir.as_os_str()]))
    .arg(&cfg.prlimit).args(["--nproc=1", "--fsize=0"])
    .arg(&cfg.perl).args(["-Mutf8", "-e", "BEGIN { binmode STDIN, ':encoding(UTF-8)'; binmode STDOUT, ':encoding(UTF-8)'; }"]);

    if full {
        cmd.args(["-e", "local $/; $_ = <>; @W = split;"]);
    } else {
        cmd.args(["-lne", "@W = split;"]);
    }

    for expr in exprs {
        cmd.args(["-E", &format!("{};", expr)]);
    }

    cmd.args(["-e", "say"]);

    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    stdin.write_all(input.as_bytes()).await?;
    drop(stdin);

    let mut buf = [0_u8; 4096];
    let mut cur = buf.as_mut_slice();
    while !cur.is_empty() {
        let n = stdout.read(cur).await?;
        if n == 0 {
            break;
        }

        cur = &mut cur[n..];
    }

    let status = child.wait().await?;
    ensure!(status.success(), "perl exited with code {:?}", status);

    Ok(String::from_utf8_lossy(&buf).into())
}

fn filter_exprs(raw_exprs: &str) -> impl Iterator<Item = &str> {
    raw_exprs
        .lines()
        .filter(|line| matches!(line.get(..2), Some("s/" | "s(" | "s[" | "s<" | "s{")))
}

fn unique_id(message: &Message) -> [u8; 12] {
    let mut res = [0; 12];
    res[..8].copy_from_slice(&message.chat.id.0.to_le_bytes());
    res[8..].copy_from_slice(&message.id.0.to_le_bytes());
    res
}

async fn do_main() -> eyre::Result<()> {
    let cfg: Config = envy::from_env()?;
    tracing::info!(
        config = format_args!("{cfg:?}"),
        "Starting perlsub Telegram bot"
    );
    let cfg = Arc::new(cfg);
    let db = sled::open(&cfg.db_path)?;
    let semaphore = Arc::new(Semaphore::new(cfg.max_parallel));

    let bot = Bot::new(&cfg.token.0);
    Dispatcher::builder(
        bot,
        dptree::endpoint(move |bot: Bot, update: Update| {
            let cfg = cfg.clone();
            let db = db.clone();
            let semaphore = semaphore.clone();
            async move {
                let (message, edited) = match update.kind {
                    UpdateKind::Message(message) => (message, false),
                    UpdateKind::EditedMessage(message) => (message, true),
                    _ => return Ok(()),
                };

                let reply_to = or_ok!(message.reply_to_message());
                let text = or_ok!(reply_to.text().or(reply_to.caption()));
                let raw_exprs = or_ok!(message.text().or(reply_to.caption()));
                let mut exprs = filter_exprs(raw_exprs).peekable();
                or_ok!(exprs.peek());
                let full = raw_exprs.lines().any(|line| line == ";full");
                let res = {
                    let _permit = semaphore.acquire().await?;
                    run_perl(exprs, text, &cfg, full).await?
                };
                if res.is_empty() {
                    return Ok(());
                }

                // If the update is of "edit" kind, ensure that the original message is present in
                // db, otherwise send a new message
                // Get from db conditionally while handling errors
                if let Some(original_reply_id_bytes) = edited.then(|| db.get(unique_id(&message)))
                        .transpose()?
                        .flatten()
                {
                    let original_reply_id = MessageId(i32::from_le_bytes(
                        (&*original_reply_id_bytes)
                            .try_into()
                            .map_err(|_| eyre!("wrong ID len in db"))?,
                    ));

                    if let Err(err) = bot
                        .edit_message_text(message.chat.id, original_reply_id, res)
                        .send()
                        .await
                    {
                        if !matches!(err, RequestError::Api(ApiError::MessageNotModified)) {
                            return Err(err.into());
                        }
                    }
                } else {
                    let mut request = bot.send_message(reply_to.chat.id, res);
                    request.reply_to_message_id = Some(reply_to.id);
                    let sent = request.send().await?;
                    db.insert(unique_id(&message), &sent.id.0.to_le_bytes())?;
                }

                if raw_exprs.lines().any(|line| line == ";del") {
                    bot.delete_message(message.chat.id, message.id)
                        .send()
                        .await?;
                }

                eyre::Result::<_>::Ok(())
            }
        }),
    )
    .enable_ctrlc_handler()
    .build()
    .dispatch()
    .await;

    Ok(())
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    do_main().await
}
