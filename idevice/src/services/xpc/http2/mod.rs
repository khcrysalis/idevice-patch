// DebianArch

use async_recursion::async_recursion;
use error::Http2Error;
use std::collections::HashMap;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc::{self, Receiver, Sender},
};

pub mod error;
pub mod h2;

use h2::{
    DataFrame, Framable, Frame, FrameType, HeadersFrame, SettingsFrame, WindowUpdateFrame,
    HTTP2_MAGIC,
};

use crate::ReadWrite;

pub type Channels = HashMap<u32, (Sender<Vec<u8>>, Receiver<Vec<u8>>)>;

pub const INIT_STREAM: u32 = 0;
pub const ROOT_CHANNEL: u32 = 1;
pub const REPLY_CHANNEL: u32 = 3;

pub struct Connection<R: ReadWrite> {
    pub stream: R,
    channels: Channels,
    window_size: u32,
}

impl<R: ReadWrite> Connection<R> {
    pub async fn new(mut stream: R) -> Result<Self, Http2Error> {
        stream.write_all(HTTP2_MAGIC).await?;
        Ok(Self {
            stream,
            channels: HashMap::new(),
            window_size: 1048576,
        })
    }

    pub async fn send_frame<A: Framable>(&mut self, frame: A) -> Result<(), Http2Error> {
        let body = &frame.serialize();
        if body.len() > self.window_size as usize {
            panic!("we need to chunk it :D")
        }
        self.stream.write_all(body).await?;
        Ok(())
    }

    pub async fn read_data(&mut self) -> Result<Vec<u8>, Http2Error> {
        loop {
            let frame = self.read_frame().await?;
            match frame.frame_type {
                FrameType::Data => {
                    if frame.stream_id % 2 == 0 && !frame.body.is_empty() {
                        let frame_len: u32 = frame.body.len().try_into()?;
                        self.send_frame(WindowUpdateFrame::new(0, frame_len))
                            .await?;
                        self.send_frame(WindowUpdateFrame::new(frame.stream_id, frame_len))
                            .await?;
                    }
                    match self.channels.get_mut(&frame.stream_id) {
                        Some((sender, _receiver)) => {
                            sender.send(frame.body.clone()).await?;
                        }
                        None => {
                            let chan = mpsc::channel(100);
                            chan.0.send(frame.body.clone()).await?;
                            self.channels.insert(frame.stream_id, chan);
                        }
                    }
                    return Ok(frame.body);
                }
                FrameType::GoAway | FrameType::RstStream => {
                    let _last_streamid = u32::from_be_bytes(frame.body[0..4].try_into().unwrap());
                    return Err("connection closed, bye")?;
                }
                FrameType::Settings => {
                    let flags = frame.flags;
                    let settings_frame: SettingsFrame = frame.into();
                    if flags & SettingsFrame::ACK != SettingsFrame::ACK {
                        self.send_frame(SettingsFrame::ack()).await?;
                    }
                    if let Some(&window_size) = settings_frame
                        .settings
                        .get(&SettingsFrame::INITIAL_WINDOW_SIZE)
                    {
                        self.window_size = window_size;
                    }
                }
                _ => continue,
            }
        }
    }

    pub async fn read_frame(&mut self) -> Result<Frame, Http2Error> {
        let mut length_buf = vec![0; 3];
        self.stream.read_exact(&mut length_buf).await?;
        length_buf.insert(0, 0);
        let len = u32::from_be_bytes(length_buf.clone().try_into().unwrap()) as usize;
        let mut rest = vec![0; 9 - 3 + len];
        self.stream.read_exact(&mut rest).await?;

        let mut content = vec![length_buf[1], length_buf[2], length_buf[3]];
        content.extend_from_slice(&rest);
        Frame::deserialize(&content)
    }

    // pub async fn multiplex_write(&mut self, stream_id: u32) -> Result<()> {}

    // gets a Reader + Writer for a channel.
    pub async fn write_streamid(
        &mut self,
        stream_id: u32,
        data: Vec<u8>,
    ) -> Result<(), Http2Error> {
        // TODO: If we ever allow concurrent writes we must not always send 'END_HEADERS'.
        self.send_frame(HeadersFrame::new(stream_id, HeadersFrame::END_HEADERS))
            .await?;
        self.send_frame(DataFrame::new(stream_id, data, Default::default()))
            .await?;
        Ok(())
    }

    #[async_recursion]
    pub async fn read_streamid(&mut self, stream_id: u32) -> Result<Vec<u8>, Http2Error> {
        match self.channels.get_mut(&stream_id) {
            Some((_sender, receiver)) => match receiver.try_recv().ok() {
                Some(data) => Ok(data),
                None => {
                    self.read_data().await?;
                    self.read_streamid(stream_id).await
                }
            },
            None => {
                self.read_data().await?;
                self.read_streamid(stream_id).await
            }
        }
    }
}
