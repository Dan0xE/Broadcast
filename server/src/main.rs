// TODO this probably has to be a daemon that runs in the background and listens for commands
// TODO give user the option to start server in vebose mode (config file? command line arg?)
// TODO give the user the option to choose what shell to use

use broadcast_protocol::{ClientMessage, CommandRequest, CommandResponse, PORT};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::Level;

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Join Error: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("Protocol Error: {0}")]
    Protocol(#[from] broadcast_protocol::ProtocolError),
    #[error("Invalid Path Error: {0}")]
    InvalidPath(String),
    #[error("PTY Error: {0}")]
    Pty(#[from] anyhow::Error),
}

pub type ServerResult<T> = Result<T, ServerError>;

#[tokio::main]
async fn main() -> ServerResult<()> {
    // TODO change this and make configurable
    tracing_subscriber::fmt()
        .with_max_level(Level::DEBUG)
        .init();

    let addr = format!("0.0.0.0:{}", PORT);
    let listener = TcpListener::bind(&addr).await?;

    tracing::info!("Listening on {}", addr);

    loop {
        match listener.accept().await {
            Ok((socket, addr)) => {
                tracing::info!("Accepted connection from {}", addr);
                tokio::spawn(async move {
                    if let Err(e) = handle_client(socket).await {
                        tracing::error!("Error handling client {}: {}", addr, e);
                    }
                });
            }
            Err(e) => {
                tracing::error!("Failed to accept connection: {}", e);
            }
        }
    }
}

async fn handle_client(socket: TcpStream) -> ServerResult<()> {
    socket.set_nodelay(true)?;

    let mut socket = socket;
    let mut request = broadcast_protocol::decode_msg::<CommandRequest>(&mut socket).await?;

    let path = if request.wsl_mode {
        convert_win_to_wsl_path(&request.working_dir)?
    } else {
        request.working_dir.clone()
    };

    if !path.exists() {
        return Err(ServerError::InvalidPath(format!(
            "The specified working directory does not exist: {:?}",
            path
        )));
    }

    request.working_dir = path;

    tracing::info!(
        "Executing command: {} in: {:?}",
        request.command,
        request.working_dir
    );

    handle_command(socket, request).await
}

async fn handle_command(socket: TcpStream, req: CommandRequest) -> ServerResult<()> {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    use std::io::{Read, Write};

    let (cols, rows) = req.terminal_size.unwrap_or((80, 24));

    let pty_size = PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(pty_size)?;

    // TODO shell make configurable from the client
    let shell = "sh";
    let mut cmd = CommandBuilder::new(shell);
    cmd.cwd(&req.working_dir);
    cmd.arg("-c");
    cmd.arg(&req.command);

    let mut child = pair.slave.spawn_command(cmd)?;

    drop(pair.slave); // close the slave end in the parent process

    let pty_master = pair.master;

    let (sock_read, mut sock_write) = socket.into_split();
    let mut sock_read = tokio::io::BufReader::new(sock_read);

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let pty_reader = pty_master.try_clone_reader()?;

    std::thread::spawn(move || {
        let mut reader = pty_reader;
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    let data = buffer[..n].to_vec();

                    if let Err(e) = out_tx.send(data) {
                        tracing::error!("Failed to send output to channel: {e}");
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!("PTY read error: {}", e);
                    break;
                }
            }
        }
    });

    let write_task = tokio::spawn(async move {
        while let Some(data) = out_rx.recv().await {
            let response = CommandResponse::Stdout(data);
            let msg = broadcast_protocol::encode_msg(&response)?;
            sock_write.write_all(&msg).await?;
            sock_write.flush().await?;
        }

        Ok::<_, ServerError>(sock_write)
    });

    let read_task = tokio::spawn(async move {
        let mut writer = pty_master.take_writer()?;
        loop {
            match broadcast_protocol::decode_msg::<ClientMessage>(&mut sock_read).await {
                Ok(msg) => match msg {
                    ClientMessage::Input(data) => {
                        tokio::task::block_in_place(|| -> ServerResult<()> {
                            writer.write_all(&data)?;
                            writer.flush()?;
                            Ok(())
                        })?;

                        tracing::debug!("Wrote {} bytes to PTY", data.len());
                    }
                    ClientMessage::Resize(cols, rows) => {
                        let size = PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        };
                        pty_master.resize(size)?;
                        tracing::debug!("Resized PTY to {} rows and {} cols", rows, cols);
                    }
                    ClientMessage::Eof => {
                        tracing::debug!("Received EOF from client, terminating session");
                        break;
                    }
                },
                Err(e) => {
                    tracing::error!("Error reading from socket: {}", e);
                    tracing::error!("Assuming client disconnected, terminating session");
                    break; // Client disconnected
                }
            }
        }
        Ok::<_, ServerError>(())
    });

    let status = tokio::task::spawn_blocking(move || child.wait()).await??;
    let exit_code = status.exit_code() as i32;

    read_task.abort();

    let mut sock_write = write_task.await??;
    let response = CommandResponse::Exit(exit_code);
    let msg = broadcast_protocol::encode_msg(&response)?;
    sock_write.write_all(&msg).await?;

    Ok(())
}

/// Converts a given windows path to a wsl path, E.g., "C:\Users\Username" -> "/mnt/c/Users/Username"
fn convert_win_to_wsl_path(win_path: &PathBuf) -> ServerResult<PathBuf> {
    let path_str = win_path.to_string_lossy();
    let mut chars = path_str.chars();
    let Some(next) = chars.next() else {
        return Err(ServerError::InvalidPath("Empty path provided".to_string()));
    };
    let drive_letter = next.to_ascii_lowercase();
    let rest_of_path: String = chars.collect();
    let rest_of_path = rest_of_path.replace('\\', "/");
    let converted_path_str = format!(
        "/mnt/{}{}",
        drive_letter,
        rest_of_path.trim_start_matches(':')
    );

    let converted_path = PathBuf::from(converted_path_str);

    Ok(converted_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_path_conversion() {
        assert_eq!(
            convert_win_to_wsl_path(&PathBuf::from("C:\\Users\\Username")).unwrap(),
            PathBuf::from("/mnt/c/Users/Username")
        );

        assert_eq!(
            convert_win_to_wsl_path(&PathBuf::from("D:\\Projects\\Test")).unwrap(),
            PathBuf::from("/mnt/d/Projects/Test")
        );

        assert!(convert_win_to_wsl_path(&PathBuf::from("")).is_err());
    }
}
