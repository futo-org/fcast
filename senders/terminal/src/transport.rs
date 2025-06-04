use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use tungstenite::protocol::WebSocket as TWebSocket;
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

pub struct WebSocket<T>
where
    T: Read + Write,
{
    inner: TWebSocket<T>,
    buffer: VecDeque<u8>,
}

impl<T> WebSocket<T>
where
    T: Read + Write,
{
    pub fn new(web_socket: TWebSocket<T>) -> Self {
        Self {
            inner: web_socket,
            buffer: VecDeque::new(),
        }
    }

    pub fn read_buffered(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        if !self.buffer.is_empty() {
            let bytes_to_read = buf.len().min(self.buffer.len());
            assert!(buf.len() >= bytes_to_read);
            assert!(self.buffer.len() >= bytes_to_read);
            for i in 0..bytes_to_read {
                buf[i] = self.buffer.pop_front().unwrap(); // Safe unwrap as bounds was checked previously
            }
        } else {
            match self.inner.read() {
                Ok(Message::Binary(data)) => {
                    let bytes_to_read = buf.len().min(data.len());
                    buf.copy_from_slice(&data[..bytes_to_read]);
                    for rest in data[bytes_to_read..].iter() {
                        self.buffer.push_back(*rest);
                    }
                }
                _ => return Err(std::io::Error::other("Invalid message type")),
            }
        }

        Ok(buf.len())
    }
}

impl<T> Transport for WebSocket<T>
where
    T: Read + Write,
{
    fn transport_read(&mut self, buf: &mut [u8]) -> Result<usize, std::io::Error> {
        self.read_buffered(buf)
    }

    fn transport_write(&mut self, buf: &[u8]) -> Result<(), std::io::Error> {
        self.inner
            .write(Message::Binary(buf.to_vec()))
            .map_err(std::io::Error::other)?;
        self.inner.flush().map_err(std::io::Error::other)
    }

    fn transport_shutdown(&mut self) -> Result<(), std::io::Error> {
        self.inner.close(None).map_err(std::io::Error::other)?;
        loop {
            match self.inner.read() {
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
            total_read += self.read_buffered(&mut buf[total_read..])?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;

    use super::*;

    #[test]
    fn websocket_read_buffered() {
        let jh = std::thread::spawn(|| {
            let server = TcpListener::bind("127.0.0.1:51232").unwrap();
            let stream = server.incoming().next().unwrap().unwrap();
            let mut websocket = tungstenite::accept(stream).unwrap();
            websocket
                .send(tungstenite::Message::binary([1, 2, 3, 4]))
                .unwrap();
            websocket
                .send(tungstenite::Message::binary([5, 6, 7, 8]))
                .unwrap();
        });

        let (websocket, _) = tungstenite::connect("ws://127.0.0.1:51232").unwrap();
        let mut websocket = WebSocket::new(websocket);

        let mut buf = [0u8; 2];
        assert_eq!(websocket.read_buffered(&mut buf).unwrap(), 2);
        assert_eq!(buf, [1, 2]);
        assert_eq!(websocket.read_buffered(&mut buf).unwrap(), 2);
        assert_eq!(buf, [3, 4]);

        let mut buf = [0u8; 4];
        assert_eq!(websocket.read_buffered(&mut buf).unwrap(), 4);
        assert_eq!(buf, [5, 6, 7, 8]);

        let _ = websocket.transport_shutdown();

        jh.join().unwrap();
    }

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

        let (websocket, _) = tungstenite::connect("ws://127.0.0.1:51234").unwrap();
        let mut websocket = WebSocket::new(websocket);

        fn read_exact<T: Transport>(stream: &mut T) {
            let mut buf = [0u8; 3];
            stream.transport_read_exact(&mut buf).unwrap();
            assert_eq!(buf, [1, 2, 3]);
        }

        read_exact(&mut websocket);

        let _ = websocket.transport_shutdown();

        jh.join().unwrap();
    }
}
