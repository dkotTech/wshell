use std::sync::mpsc::SyncSender;
use std::sync::Arc;

use crate::types::LastFrame;

/// Raw pixels handed from the engine thread to the encoder thread.
pub struct PixelBuf {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
}

#[allow(dead_code)] // each engine constructs only its own variant
pub enum PixelFormat {
    /// Tightly packed RGBA (servo's read_to_image output).
    Rgba,
    /// BGRA with a row stride in bytes (WPE SHM buffers).
    BgraStride(u32),
}

/// Spawns a per-render encoding thread: raw pixels → PNG → `last_frame`.
/// The `sync_channel(1)` sender drops stale frames when the encoder is busy,
/// so the engine thread is never blocked by encoding. The thread exits when
/// the sender side is dropped (render destroyed).
pub fn spawn_encoder(last_frame: LastFrame) -> SyncSender<PixelBuf> {
    let (pixel_tx, pixel_rx) = std::sync::mpsc::sync_channel::<PixelBuf>(1);

    std::thread::spawn(move || {
        while let Ok(pb) = pixel_rx.recv() {
            let rgba: Vec<u8> = match pb.format {
                PixelFormat::Rgba => pb.data,
                PixelFormat::BgraStride(stride) => {
                    // BGRA + stride → tightly packed RGBA (channel swap)
                    let (w, h) = (pb.width as usize, pb.height as usize);
                    let mut out = Vec::with_capacity(w * h * 4);
                    for row in 0..h {
                        for col in 0..w {
                            let i = row * stride as usize + col * 4;
                            out.push(pb.data[i + 2]); // R
                            out.push(pb.data[i + 1]); // G
                            out.push(pb.data[i]); // B
                            out.push(255); // A (opaque)
                        }
                    }
                    out
                }
            };

            let mut buf = Vec::new();
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};
            use image::{ExtendedColorType, ImageEncoder};
            let enc = PngEncoder::new_with_quality(
                &mut buf,
                CompressionType::Fast, // zlib level 1 — fast encode
                FilterType::Sub,       // delta filter — good for UI content
            );
            if enc
                .write_image(&rgba, pb.width, pb.height, ExtendedColorType::Rgba8)
                .is_ok()
            {
                // Only publish the latest frame; the API-side ticker broadcasts
                // it at a steady fps (so the stream never stalls on a static page).
                *last_frame.lock().unwrap() = Some(Arc::new(buf));
            }
        }
    });

    pixel_tx
}
