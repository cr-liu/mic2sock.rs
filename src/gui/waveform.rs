use axum::{
    extract::{ws::{Message, WebSocket, WebSocketUpgrade}, State},
    response::Response,
};
use std::sync::Arc;
use tokio::time::{interval, Duration};
use std::time::{SystemTime, UNIX_EPOCH};

use super::GuiHandles;

const FRAME_MS: u64 = 50;
const POINTS_PER_FRAME: usize = 100;

pub async fn ws_waveform(
    ws: WebSocketUpgrade,
    State(handles): State<Arc<GuiHandles>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_waveform_socket(socket, handles))
}

async fn handle_waveform_socket(mut socket: WebSocket, handles: Arc<GuiHandles>) {
    let mut sub = handles.waveform_tap.subscribe();
    let mut accum: Vec<i16> = Vec::with_capacity(16000 / 20);

    let mut tick = interval(Duration::from_millis(FRAME_MS));
    tick.tick().await;

    loop {
        tokio::select! {
            res = sub.recv() => {
                match res {
                    Ok(samples) => accum.extend_from_slice(&samples),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
            _ = tick.tick() => {
                if accum.is_empty() { continue; }
                let downsampled = downsample(&accum, POINTS_PER_FRAME);
                accum.clear();
                let frame = build_frame(downsampled);
                if socket.send(Message::Binary(frame)).await.is_err() {
                    break;
                }
            }
        }
    }
}

fn downsample(samples: &[i16], points: usize) -> Vec<i16> {
    if samples.is_empty() {
        return vec![];
    }
    let chunk_size = samples.len().div_ceil(points);
    samples
        .chunks(chunk_size)
        .map(|c| c.iter().copied().max_by_key(|s| s.unsigned_abs()).unwrap_or(0))
        .collect()
}

fn build_frame(samples: Vec<i16>) -> Vec<u8> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u32;
    let mut buf = Vec::with_capacity(4 + samples.len() * 2);
    buf.extend_from_slice(&ts.to_le_bytes());
    for s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    buf
}
