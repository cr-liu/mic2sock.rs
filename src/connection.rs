use bytes::{Buf, BytesMut};
use std::io::{self, Cursor};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::MIC_ID;

/// Send and receive `Frame` values from a remote peer.
///
/// When implementing networking protocols, a message on that protocol is
/// often composed of several smaller messages known as frames. The purpose of
/// `Connection` is to read and write frames on the underlying `TcpStream`.
///
/// To read frames, the `Connection` uses an internal buffer, which is filled
/// up until there are enough bytes to create a full frame. Once this happens,
/// the `Connection` creates the frame and returns it to the caller.
///
/// When sending frames, the frame is first encoded into the write buffer.
/// The contents of the write buffer are then written to the socket.
#[derive(Debug)]
pub struct Connection {
    // The `TcpStream`. It is decorated with a `BufWriter`, which provides write
    // level buffering. The `BufWriter` implementation provided by Tokio is
    // sufficient for our needs.
    stream: TcpStream,
    packet_id: u32,
    mic_id: u16,
    // The buffer for reading frames.
    buffer: BytesMut,
}

impl Connection {
    /// Create a new `Connection`, backed by `socket`. Read and write buffers
    /// are initialized.
    pub fn new(socket: TcpStream) -> Connection {
        Connection {
            stream: socket,
            packet_id: 0,
            // use set_mic_id() to apply config
            mic_id: 0,
            // Default to a 4KB read buffer. For the use case of mini redis,
            // this is fine. However, real applications will want to tune this
            // value to their specific use case. There is a high likelihood that
            // a larger read buffer will work better.

            buffer: BytesMut::with_capacity(4 * 1024),
        }
    }

    pub fn set_mic_id(&mut self) {
        unsafe { self.mic_id = MIC_ID; }
    }

    /// Read a single `Frame` value from the underlying stream.
    ///
    /// The function waits until it has retrieved enough data to parse a frame.
    /// Any data remaining in the read buffer after the frame has been parsed is
    /// kept there for the next call to `read_frame`.
    ///
    /// # Returns
    ///
    /// On success, the received frame is returned. If the `TcpStream`
    /// is closed in a way that doesn't break a frame in half, it returns
    /// `None`. Otherwise, an error is returned.
    pub async fn read_frame(&mut self) -> crate::Result<()> {
        loop {
            // There is not enough buffered data to read a frame. Attempt to
            // read more data from the socket.
            //
            // On success, the number of bytes is returned. `0` indicates "end
            // of stream".
            if 0 == self.stream.read_buf(&mut self.buffer).await? {
                // The remote closed the connection. For this to be a clean
                // shutdown, there should be no data in the read buffer. If
                // there is, this means that the peer closed the socket while
                // sending a frame.
                if self.buffer.is_empty() {
                    return Ok(());
                } else {
                    return Err("connection reset by peer".into());
                }
            }
        }
    }

    pub async fn write_packet(&mut self) -> crate::Result<()> {
        let now_in_ms = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH).unwrap().as_millis();
        let now_in_s: u32 = (now_in_ms / 1000).try_into().unwrap();
        let ms: u16 = (now_in_ms % 1000).try_into().unwrap();

        self.stream.write_u32(now_in_s).await?;
        self.stream.write_u16(ms).await?;
        self.stream.write_u32(self.packet_id).await?;
        self.stream.write_u16(self.mic_id).await?;

        self.stream.flush().await?;
        Ok(())
    }
}
