use crossterm::event::KeyEvent;
use std::{
    env,
    io::{IsTerminal, Write, stdout},
    time::Duration,
};
use terminput::Encoding;
use terminput_crossterm::to_terminput;
use tokio::net::TcpStream;
use tokio::{io::AsyncWriteExt, task::JoinHandle};
use tracing::Level;

// TODO have install command, that downloads the server trough "broadcast setup"
// TODO give the user a command to setup aliases in their shell (broadcast install -c "command" -a "alias")
// The user should be able to choose the wsl distribution to install to (nice menu for that)
// TODO have edit config command so config can be edited from windows
use broadcast_protocol::{
    ClientMessage, CommandRequest, CommandResponse, PORT, decode_msg, encode_msg,
};
use clap::Parser;

#[derive(clap::Parser, Debug)]
#[command(name = "broadcast", about = "Broadcast commands to WSL")]
#[command(author, version, about, long_about = None)]
#[command(arg_required_else_help = true)]
struct Args {
    #[arg(
        required = true,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        help = "The command to broadcast"
    )]
    command: Vec<String>,
    #[arg(short, long, help = "Enable verbose output")]
    verbose: bool,
    #[arg(short, long, help = "Enable debug logging to file")]
    debug: bool,
    // shell, can overwrite default / config value
}

#[derive(thiserror::Error, Debug)]
enum ClientError {
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Protocol Error: {0}")]
    Protocol(#[from] broadcast_protocol::ProtocolError),
    #[error("Join Error: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("Log Setup Error: {0}")]
    LogSetup(#[from] tracing::subscriber::SetGlobalDefaultError),
}

type ClientResult<T> = Result<T, ClientError>;

#[tokio::main]
async fn main() -> ClientResult<()> {
    let args = Args::parse();

    let _log_guard = setup_logging(args.debug, args.verbose)?;

    let cmd = args.command.join(" ");

    let cwd = env::current_dir()?.to_string_lossy().to_string();

    let stdin_is_tty = std::io::stdin().is_terminal();

    let addr = format!("127.0.0.1:{}", PORT);
    let mut stream = TcpStream::connect(&addr).await?;

    // disable Nagle's algorithm, we need to disable TCP buffering for interactive commands,
    // otherwise we get weird issues
    stream.set_nodelay(true)?;

    let terminal_size = if stdin_is_tty {
        use crossterm::terminal;
        terminal::size().ok()
    } else {
        None
    };

    let request = CommandRequest {
        command: cmd.clone(),
        working_dir: cwd,
        terminal_size,
    };

    let msg = broadcast_protocol::encode_msg(&request)?;
    stream.write_all(&msg).await?;

    let exit_code = handle_response(stream, stdin_is_tty).await?;
    std::process::exit(exit_code);
}

fn setup_logging(
    debug: bool,
    verbose: bool,
) -> ClientResult<Option<tracing_appender::non_blocking::WorkerGuard>> {
    let dbg = debug || verbose;
    let level = if dbg { Level::DEBUG } else { Level::INFO };

    if dbg {
        let file_appender = tracing_appender::rolling::daily("logs", "broadcast-client.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::fmt()
            .with_max_level(level)
            .with_writer(non_blocking)
            .with_ansi(false)
            .init();

        tracing::debug!("Debug logging enabled");
        return Ok(Some(guard));
    }

    tracing_subscriber::fmt().with_max_level(level).init();
    Ok(None)
}

async fn handle_response(stream: TcpStream, stdin_is_tty: bool) -> ClientResult<i32> {
    use crossterm::{
        event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
        terminal::{disable_raw_mode, enable_raw_mode},
    };

    if stdin_is_tty {
        enable_raw_mode()?
    }

    let (mut stream_read, mut stream_write) = stream.into_split();

    let output_task: JoinHandle<ClientResult<i32>> = tokio::spawn(async move {
        let mut stdout = stdout();

        loop {
            match decode_msg::<CommandResponse>(&mut stream_read).await {
                Ok(response) => match response {
                    CommandResponse::Stdout(data) => {
                        stdout.write_all(&data)?;
                        stdout.flush()?;
                    }
                    CommandResponse::Exit(code) => return Ok(code),
                    CommandResponse::Error(e) => {
                        tracing::error!("Server error: {}", e);
                        return Ok(1);
                    }
                    // Stderr is merged
                    _ => {}
                },
                Err(_) => {
                    // Connection closed
                    tracing::warn!("Connection closed by server");
                    return Ok(1);
                }
            }
        }
    });

    let input_task = tokio::spawn(async move {
        loop {
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('c'),
                        modifiers: KeyModifiers::CONTROL,
                        ..
                    }) => {
                        let msg = encode_msg(&ClientMessage::Input(vec![3]))?;
                        stream_write.write_all(&msg).await?;
                        break;
                    }
                    Event::Key(KeyEvent {
                        code: KeyCode::Char('d'),
                        modifiers: KeyModifiers::CONTROL,
                        ..
                    }) => {
                        let msg = encode_msg(&ClientMessage::Eof)?;
                        stream_write.write_all(&msg).await?;
                        break;
                    }
                    Event::Key(key_event) => {
                        if key_event.kind != event::KeyEventKind::Press {
                            continue;
                        }

                        if let Some(bytes) = key_event.to_bytes() {
                            let msg = encode_msg(&ClientMessage::Input(bytes))?;
                            stream_write.write_all(&msg).await?;
                        }
                    }
                    Event::Resize(cols, rows) => {
                        tracing::debug!("Resize event: resized to {} cols and {} rows", cols, rows);
                        let msg = encode_msg(&ClientMessage::Resize(cols, rows))?;
                        stream_write.write_all(&msg).await?;
                    }
                    _ => {}
                }
            }
        }
        Ok::<_, ClientError>(())
    });

    let exit_code = output_task.await??;

    input_task.abort();
    if stdin_is_tty {
        disable_raw_mode()?
    }

    Ok(exit_code)
}

// FIXME this doesn't seem to work well with VIM
// Vim does this weird thing where it will try to query the terminal for its size?

trait KeyEventExt {
    fn to_bytes(&self) -> Option<Vec<u8>>;
}

// Forgot the issues that mentioned it, but not a trivial issue fix
impl KeyEventExt for KeyEvent {
    fn to_bytes(&self) -> Option<Vec<u8>> {
        let term_event = to_terminput(crossterm::event::Event::Key(*self)).ok()?;

        let mut buf = [0u8; 32];

        match term_event.encode(&mut buf, Encoding::Xterm) {
            Ok(len) => Some(buf[..len].to_vec()),
            Err(_) => None,
        }
    }
}
