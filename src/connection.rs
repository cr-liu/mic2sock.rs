use arc_swap::access::Access;
use arc_swap::ArcSwapAny;
use bytes::BytesMut;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub struct Connection {
    stream: TcpStream,
    data_to_send: Arc<ArcSwapAny<Arc<BytesMut>>>,
    // The buffer for reading frames.
    read_buffer: BytesMut,
}

impl Connection {
    /// Create a new `Connection`, backed by `socket`. Read and write buffers
    /// are initialized.
    pub fn new(socket: TcpStream, data: Arc<ArcSwapAny<Arc<BytesMut>>>) -> Connection {
        Connection {
            stream: socket,
            data_to_send: data,
            read_buffer: BytesMut::with_capacity(1024),
        }
    }

    pub async fn read_frame(&mut self) -> crate::Result<()> {
        let mut read_buf = BytesMut::with_capacity(1024);
        loop {
            self.stream.read_buf(&mut read_buf).await?;
            if !self.read_buffer.is_empty() {
                println!("unexpected incoming stream");
            } else {
                return Err("connection reset by peer".into());
            }
        }
    }

    pub async fn write_packet(&mut self) -> crate::Result<()> {
        self.stream
            .write(
                (self.data_to_send.as_ref() as &ArcSwapAny<Arc<BytesMut>>)
                    .load()
                    .as_ref(),
            )
            .await?;
        self.stream.flush().await?;
        Ok(())
    }
}
