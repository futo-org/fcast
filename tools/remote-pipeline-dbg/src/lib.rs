#[cfg(feature = "client")]
use std::io::Write;
use std::{io::Read, net::TcpStream};

#[derive(Debug)]
#[repr(u8)]
pub enum PipelineSource {
    MainPlayer = 0,
    RaopPlayer = 1,
}

impl PipelineSource {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::MainPlayer,
            1 => Self::RaopPlayer,
            _ => unreachable!(),
        }
    }
}

impl ToString for PipelineSource {
    fn to_string(&self) -> String {
        match self {
            PipelineSource::MainPlayer => "Main player",
            PipelineSource::RaopPlayer => "RAOP player",
        }
        .to_string()
    }
}

// TODO: add manual trigger
#[derive(Debug)]
#[repr(u8)]
pub enum Trigger {
    Pause = 0,
    Warning = 1,
    Error = 2,
    Manual = 3,
}

impl Trigger {
    fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Pause,
            1 => Self::Warning,
            2 => Self::Error,
            3 => Self::Manual,
            _ => unreachable!(),
        }
    }
}

impl ToString for Trigger {
    fn to_string(&self) -> String {
        match self {
            Trigger::Pause => "Pause",
            Trigger::Warning => "Warning",
            Trigger::Error => "Error",
            Trigger::Manual => "Manual",
        }
        .to_string()
    }
}

#[cfg(feature = "client")]
pub fn post_graph(graph: &[u8], source: PipelineSource, trigger: Trigger) -> std::io::Result<()> {
    let source = source as u8;
    let trigger = trigger as u8;

    #[cfg(target_os = "android")]
    let sockaddr = option_env!("PIPELINE_DBG_HOST").unwrap_or("127.0.0.1:3000");
    #[cfg(not(target_os = "android"))]
    let sockaddr = std::env::var("PIPELINE_DBG_HOST").unwrap_or("127.0.0.1:3000".to_owned());

    let mut stream = TcpStream::connect(sockaddr)?;
    stream.write_all(&[source, trigger])?;
    let len_buf = (graph.len() as u32).to_le_bytes();
    stream.write_all(&len_buf)?;
    stream.write_all(graph)?;

    Ok(())
}

pub fn read_graph(mut stream: TcpStream) -> std::io::Result<(Vec<u8>, PipelineSource, Trigger)> {
    let mut meta_buf = [0u8; 2];
    stream.read_exact(&mut meta_buf)?;

    let source = PipelineSource::from_u8(meta_buf[0]);
    let trigger = Trigger::from_u8(meta_buf[1]);

    let mut length_buf = [0u8; 4];
    stream.read_exact(&mut length_buf)?;
    let data_len = u32::from_le_bytes(length_buf) as usize;
    let mut data_buf: Vec<u8> = Vec::with_capacity(data_len);
    data_buf.resize(data_len, 0);
    stream.read_exact(&mut data_buf)?;

    Ok((data_buf, source, trigger))
}
