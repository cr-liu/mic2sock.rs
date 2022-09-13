use arc_swap::ArcSwap;
use bytes::BytesMut;
use std::io::Error;
use std::result::Result;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

pub struct SocketWriter {
    pub(crate) writer: OwnedWriteHalf,
    pub(crate) data_to_send: Arc<ArcSwap<BytesMut>>,
    // _read_buffer: BytesMut,
}

impl SocketWriter {
    pub async fn write_packet(&mut self) -> crate::Result<()> {
        self.writer
            .write(
                (self.data_to_send.as_ref() as &ArcSwap<BytesMut>)
                    .load()
                    .as_ref(),
            )
            .await?;
        // self.stream.flush().await?;
        Ok(())
    }
}

pub struct SocketReader {
    pub(crate) reader: OwnedReadHalf,
}

impl SocketReader {
    pub async fn read_packet(&mut self) -> Result<usize, Error> {
        let mut read_buffer = BytesMut::with_capacity(1024);
        // self.reader.readable().await?;
        let read_size = self.reader.read(&mut read_buffer).await?;
        if read_size != 0 {
            println!("unexpected incoming socket: {:?}", &read_buffer);
        }
        Ok(read_size)
    }
}
