use std::io::{Read, Write};
use std::net::TcpStream;
use tungstenite::protocol::WebSocket;
use tungstenite::Message;

pub trait Transport {
    fn transport_read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error>;
    fn transport_write(&mut self, buf: &[u8]) -> Result<(), std::io::Error>;
    fn transport_shutdown(&mut self) -> Result<(), std::io::Error>;
    fn transport_read_exact(&mut self, buf: &mut [u8]) -> Result<(), std::io::Error>;
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

    fn transport_read_exact(&mut self, buf: &mut [u8]) -> Result<(), std::io::Error> {
        self.read_exact(buf)
    }
}

impl<T: Read + Write> Transport for WebSocket<T> {
    fn transport_read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        match self.read() {
            Ok(Message::Binary(data)) => {
                let len = std::cmp::min(buf.len(), data.len());
                buf[..len].copy_from_slice(&data[..len]);
                Ok(len)
            }
            _ => Err(std::io::Error::other("Invalid message type")),
        }
    }

    fn transport_write(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
        self.write(Message::Binary(buf.to_vec()))
            .map_err(std::io::Error::other)?;
        self.flush().map_err(std::io::Error::other)
    }

    fn transport_shutdown(&mut self) -> Result<(), std::io::Error> {
        self.close(None).map_err(std::io::Error::other)?;
        loop {
            match self.read() {
                Ok(_) => continue,
                Err(tungstenite::Error::ConnectionClosed) => break,
                Err(e) => return Err(std::io::Error::other(e)),
            }
        }

        Ok(())
    }

    fn transport_read_exact(&mut self, buf: &mut [u8]) -> Result<(), std::io::Error> {
        let mut total_read = 0;
        while total_read < buf.len() {
            total_read += self.transport_read(&mut buf[total_read..])?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn websocket_read_exact() {
        let jh = std::thread::spawn(|| {
            let server = TcpListener::bind("127.0.0.1:51234").unwrap();
            let stream = server.incoming().next().unwrap().unwrap();
            let mut websocket = tungstenite::accept(stream).unwrap();
            websocket
                .send(tungstenite::Message::binary([1, 2, 3]))
                .unwrap();
        });

        let (mut websocket, _) = tungstenite::connect("ws://127.0.0.1:51234").unwrap();

        fn read_exact<T: Transport>(stream: &mut T) {
            let mut buf = [0u8; 3];
            stream.transport_read_exact(&mut buf).unwrap();
            assert_eq!(buf, [1, 2, 3]);
        }

        read_exact(&mut websocket);

        websocket.close(None).unwrap();

        jh.join().unwrap();
    }
}
