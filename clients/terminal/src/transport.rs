use std::io::{Read, Write};
use std::net::TcpStream;
use tungstenite::Message;
use tungstenite::protocol::WebSocket;

pub trait Transport {
    fn transport_read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error>;
    fn transport_write(&mut self, buf: &[u8]) -> Result<(), std::io::Error>;
    fn transport_shutdown(&mut self) -> Result<(), std::io::Error>;
}

impl Transport for TcpStream {
    fn transport_read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.read(buf)
    }

    fn transport_write(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
        self.write_all(buf)
    }

    fn transport_shutdown(&mut self) -> Result<(), std::io::Error> {
        self.shutdown(std::net::Shutdown::Both)
    }
}

impl<T: Read + Write> Transport for WebSocket<T> {
    fn transport_read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        match self.read() {
            Ok(Message::Binary(data)) => {
                let len = std::cmp::min(buf.len(), data.len());
                buf[..len].copy_from_slice(&data[..len]);
                Ok(len)
            },
            _ => Err(std::io::Error::new(std::io::ErrorKind::Other, "Invalid message type"))
        }
    }

    fn transport_write(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
        self.write(Message::Binary(buf.to_vec()))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        self.flush().map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }

    fn transport_shutdown(&mut self) -> Result<(), std::io::Error> {
        self.close(None).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        loop {
            match self.read() {
                Ok(_) => continue,
                Err(tungstenite::Error::ConnectionClosed) => break,
                Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::Other, e)),
            }
        }

        Ok(())
    }
}