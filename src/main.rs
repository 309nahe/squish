//! squish: a tiny image compressor and converter.
//!
//! Decodes the upload in memory, applies the EXIF orientation, optionally resizes,
//! and re-encodes to WebP, JPEG or PNG. Re-encoding drops all metadata (EXIF, GPS).
//! Nothing touches the disk; a semaphore and pixel caps keep memory bounded.

use axum::{
    extract::{DefaultBodyLimit, Multipart},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};
use image::{imageops::FilterType, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use std::{io::Cursor, sync::OnceLock};
use tokio::sync::Semaphore;

/// Upload cap (bytes).
const MAX_UPLOAD: usize = 40 * 1024 * 1024;
/// Largest accepted image: 50 megapixels (~200 MB as RGBA).
const MAX_PIXELS: u64 = 50_000_000;
/// Longest accepted edge.
const MAX_EDGE: u32 = 16_384;
/// Work budget in megapixels shared by concurrent jobs (~14 MB of RAM per MP at peak),
/// so many small images run in parallel but two huge ones queue.
const BUDGET_MP: u32 = 60;

static SEM: OnceLock<Semaphore> = OnceLock::new();

type Fail = (StatusCode, String);
fn fail(code: StatusCode, msg: impl Into<String>) -> Fail {
    (code, msg.into())
}

#[derive(Clone, Copy, PartialEq)]
enum Out {
    Webp,
    Jpeg,
    Png,
}

impl Out {
    fn mime(self) -> &'static str {
        match self {
            Out::Webp => "image/webp",
            Out::Jpeg => "image/jpeg",
            Out::Png => "image/png",
        }
    }
    fn ext(self) -> &'static str {
        match self {
            Out::Webp => "webp",
            Out::Jpeg => "jpg",
            Out::Png => "png",
        }
    }
}

struct Job {
    data: Vec<u8>,
    format: String,
    quality: u8,
    max_edge: u32,
}

struct Done {
    bytes: Vec<u8>,
    out: Out,
    in_w: u32,
    in_h: u32,
    w: u32,
    h: u32,
}

fn decode(data: &[u8]) -> Result<(DynamicImage, ImageFormat), Fail> {
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .map_err(|_| fail(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unreadable file"))?;
    let fmt = reader.format().ok_or_else(|| fail(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Not a supported image (JPEG, PNG, WebP, GIF, BMP, TIFF)"))?;
    let mut dec = reader.into_decoder().map_err(|e| fail(StatusCode::UNPROCESSABLE_ENTITY, format!("Cannot decode: {e}")))?;
    let (w, h) = dec.dimensions();
    if w > MAX_EDGE || h > MAX_EDGE || (w as u64) * (h as u64) > MAX_PIXELS {
        return Err(fail(StatusCode::PAYLOAD_TOO_LARGE, format!("Image too large ({w}×{h}); limit is 50 megapixels")));
    }
    let mut limits = Limits::default();
    limits.max_alloc = Some(512 * 1024 * 1024);
    dec.set_limits(limits).map_err(|e| fail(StatusCode::PAYLOAD_TOO_LARGE, e.to_string()))?;
    let orient = dec.orientation().ok();
    let mut img = DynamicImage::from_decoder(dec).map_err(|e| fail(StatusCode::UNPROCESSABLE_ENTITY, format!("Cannot decode: {e}")))?;
    if let Some(o) = orient {
        img.apply_orientation(o);
    }
    Ok((img, fmt))
}

/// Puts transparent pixels on white, for formats without alpha.
fn flatten(img: &DynamicImage) -> image::RgbImage {
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut out = image::RgbImage::new(w, h);
    for (o, p) in out.pixels_mut().zip(rgba.pixels()) {
        let a = p[3] as u32;
        for c in 0..3 {
            o[c] = ((p[c] as u32 * a + 255 * (255 - a)) / 255) as u8;
        }
    }
    out
}

fn encode(img: &DynamicImage, out: Out, quality: u8) -> Result<Vec<u8>, Fail> {
    let err = |e: String| fail(StatusCode::INTERNAL_SERVER_ERROR, format!("Encode failed: {e}"));
    match out {
        Out::Webp => {
            let (w, h) = (img.width(), img.height());
            let (rgb, rgba);
            let enc = if img.color().has_alpha() {
                rgba = img.to_rgba8();
                webp::Encoder::from_rgba(rgba.as_raw(), w, h)
            } else {
                rgb = img.to_rgb8();
                webp::Encoder::from_rgb(rgb.as_raw(), w, h)
            };
            let mem = if quality >= 100 { enc.encode_lossless() } else { enc.encode(quality as f32) };
            Ok(mem.to_vec())
        }
        Out::Jpeg => {
            let rgb = if img.color().has_alpha() { flatten(img) } else { img.to_rgb8() };
            let mut buf = Vec::new();
            let mut enc = jpeg_encoder::Encoder::new(&mut buf, quality.clamp(1, 100));
            enc.set_progressive(true);
            enc.set_optimized_huffman_tables(true);
            enc.encode(rgb.as_raw(), rgb.width() as u16, rgb.height() as u16, jpeg_encoder::ColorType::Rgb)
                .map_err(|e| err(e.to_string()))?;
            Ok(buf)
        }
        Out::Png => {
            let mut buf = Vec::new();
            let enc = image::codecs::png::PngEncoder::new_with_quality(
                &mut buf,
                image::codecs::png::CompressionType::Best,
                image::codecs::png::FilterType::Adaptive,
            );
            let src = if img.color().has_alpha() { DynamicImage::ImageRgba8(img.to_rgba8()) } else { DynamicImage::ImageRgb8(img.to_rgb8()) };
            src.write_with_encoder(enc).map_err(|e| err(e.to_string()))?;
            Ok(buf)
        }
    }
}

fn process(job: Job) -> Result<Done, Fail> {
    let (img, in_fmt) = decode(&job.data)?;
    drop(job.data);
    let (in_w, in_h) = (img.width(), img.height());
    let out = match job.format.as_str() {
        "jpeg" => Out::Jpeg,
        "png" => Out::Png,
        "keep" => match in_fmt {
            ImageFormat::Jpeg => Out::Jpeg,
            ImageFormat::Png => Out::Png,
            _ => Out::Webp,
        },
        _ => Out::Webp,
    };
    let img = if job.max_edge > 0 && in_w.max(in_h) > job.max_edge {
        img.resize(job.max_edge, job.max_edge, FilterType::CatmullRom)
    } else {
        img
    };
    let bytes = encode(&img, out, job.quality)?;
    Ok(Done { bytes, out, in_w, in_h, w: img.width(), h: img.height() })
}

/// POST /api/squish: multipart `file`, `format` (webp|jpeg|png|keep), `quality` (1-100), `max` (long edge, 0 = keep).
async fn squish(mut mp: Multipart) -> Result<Response, Fail> {
    let mut job = Job { data: Vec::new(), format: "webp".into(), quality: 80, max_edge: 0 };
    while let Some(field) = mp.next_field().await.map_err(|e| fail(StatusCode::BAD_REQUEST, e.body_text()))? {
        match field.name().unwrap_or("") {
            "file" => job.data = field.bytes().await.map_err(|e| fail(StatusCode::PAYLOAD_TOO_LARGE, e.body_text()))?.to_vec(),
            "format" => job.format = field.text().await.unwrap_or_default(),
            "quality" => job.quality = field.text().await.ok().and_then(|s| s.parse().ok()).unwrap_or(80u8).clamp(1, 100),
            "max" => job.max_edge = field.text().await.ok().and_then(|s| s.parse().ok()).unwrap_or(0u32).min(MAX_EDGE),
            _ => {}
        }
    }
    if job.data.is_empty() {
        return Err(fail(StatusCode::BAD_REQUEST, "No file uploaded"));
    }
    let in_bytes = job.data.len();
    // Reserve budget by size; the header read is cheap and unknown sizes take one unit.
    let mp = ImageReader::new(Cursor::new(&job.data))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .map(|(w, h)| ((w as u64 * h as u64).div_ceil(1_000_000)) as u32)
        .unwrap_or(1)
        .clamp(1, BUDGET_MP);
    let _slot = SEM.get().unwrap().acquire_many(mp).await.map_err(|_| fail(StatusCode::SERVICE_UNAVAILABLE, "Shutting down"))?;
    let done = tokio::task::spawn_blocking(move || process(job))
        .await
        .map_err(|_| fail(StatusCode::INTERNAL_SERVER_ERROR, "Worker crashed"))??;

    let mut h = HeaderMap::new();
    let mut set = |k: &'static str, v: String| {
        if let Ok(v) = HeaderValue::from_str(&v) {
            h.insert(k, v);
        }
    };
    set("content-type", done.out.mime().into());
    set("cache-control", "no-store".into());
    set("x-in-bytes", in_bytes.to_string());
    set("x-in-size", format!("{}x{}", done.in_w, done.in_h));
    set("x-out-size", format!("{}x{}", done.w, done.h));
    set("x-ext", done.out.ext().into());
    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("*"));
    Ok((h, done.bytes).into_response())
}

#[tokio::main]
async fn main() {
    SEM.set(Semaphore::new(BUDGET_MP as usize)).ok();
    let app = Router::new()
        .route("/", get(|| async { Html(include_str!("index.html")) }))
        .route("/api/squish", post(squish))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD + 64 * 1024));
    let addr = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:8452".into());
    let l = tokio::net::TcpListener::bind(&addr).await.expect("bind");
    println!("listening on {addr}");
    axum::serve(l, app).await.unwrap();
}
