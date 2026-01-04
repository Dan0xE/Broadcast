use std::path::PathBuf;

use bincode::{Decode, Encode, error as BincodeError};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Decode, Encode)]
pub struct CommandRequest {
    /// The command to execute
    pub command: String,
    /// The working directory for the command to be executed in
    pub working_dir: PathBuf,
    /// Whether to convert Windows paths to WSL paths
    pub wsl_mode: bool,
    /// The terminal size (cols, rows) if applicable
    pub terminal_size: Option<(u16, u16)>,
    /// Whether to spawn an interactive shell instead of running a single command
    pub interactive: bool,
}

#[derive(Serialize, Deserialize, Debug, Decode, Encode)]
pub enum ClientMessage {
    Input(Vec<u8>),
    Resize(u16, u16),
    Eof,
}

#[derive(Serialize, Deserialize, Debug, Decode, Encode)]
pub enum CommandResponse {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    Exit(i32),
    Error(String),
}

#[derive(thiserror::Error, Debug)]
pub enum ProtocolError {
    #[error("Encode Error: {0}")]
    Encode(#[from] BincodeError::EncodeError),
    #[error("Decode Error: {0}")]
    Decode(#[from] BincodeError::DecodeError),
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),
}

pub type ProtocolResult<T> = Result<T, ProtocolError>;

// TODO this should not be inside of the protocol crate
pub const PORT: u16 = 9877;

pub fn encode_msg<T: Encode>(msg: &T) -> ProtocolResult<Vec<u8>> {
    let config = bincode::config::standard();
    let data = bincode::encode_to_vec(msg, config)?;
    let len = (data.len() as u32).to_be_bytes();

    let mut result = Vec::with_capacity(4 + data.len());
    result.extend_from_slice(&len);
    result.extend_from_slice(&data);

    Ok(result)
}

pub async fn decode_msg<T: Decode<()>>(
    reader: &mut (impl tokio::io::AsyncReadExt + Unpin),
) -> ProtocolResult<T> {
    let mut len_bytes = [0u8; 4];
    reader.read_exact(&mut len_bytes).await?;
    let len = u32::from_be_bytes(len_bytes);

    let mut buf = vec![0u8; len as usize];
    reader.read_exact(&mut buf).await?;

    let config = bincode::config::standard();
    let decoded: T = bincode::decode_from_slice(&buf, config)?.0;

    Ok(decoded)
}
