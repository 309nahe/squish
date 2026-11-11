# squish

A tiny self-hosted image compressor and converter. Drop images, pick a format, quality and maximum size,
compare before and after with a slider, download.

- Outputs **WebP** (lossy, or lossless at max quality), **JPEG** (progressive, optimized Huffman tables) or **PNG**
- Resize by longest edge (4K, 2560, 1920, 1280, 800)
- Applies the EXIF orientation, then **strips all metadata** (EXIF, GPS) by re-encoding
- Changing a setting re-applies it to every image in the list
- **Stateless**: images are processed in memory and discarded; nothing touches the disk
- Bounded memory: 40 MB / 50 megapixels per image, and a shared 60-megapixel work budget so large images queue instead of exhausting RAM

## Stack

Rust, [axum](https://github.com/tokio-rs/axum) + tokio, [`image`](https://github.com/image-rs/image) for decoding and resizing,
`webp` (libwebp) and `jpeg-encoder` for encoding. One embedded HTML page, no external assets.

## Run

Building needs a C compiler (libwebp is compiled from source).

```sh
cargo build --release
BIND=127.0.0.1:8452 ./target/release/squish
```

## Deploy

`deploy/squish.service` is a hardened systemd unit. There is no authentication, so keep it private, for example:

```sh
sudo tailscale serve --bg --set-path=/squish http://127.0.0.1:8452
```

## API

`POST /api/squish`, multipart with `file`, `format` (`webp|jpeg|png|keep`), `quality` (1-100), `max` (longest edge in px, 0 = keep).
Returns the image. Headers: `x-in-size`, `x-out-size` (`WxH`), `x-in-bytes`, `x-ext`.

```sh
curl -F file=@photo.jpg -F format=webp -F quality=80 -F max=1920 http://127.0.0.1:8452/api/squish -o photo.webp
```
