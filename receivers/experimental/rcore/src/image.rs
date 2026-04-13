use std::collections::HashMap;

use bytes::Bytes;
use imagelib::{
    AnimationDecoder, DynamicImage, ImageFormat, ImageReader,
    codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder},
    metadata,
};
use tracing::{debug, debug_span, error, info};

use crate::{CompoundImage, MessageSender, SlintRgba8Pixbuf, fcast::CompanionContext};

pub type ImageId = u32;
pub type ImageDownloadId = u32;

#[derive(Debug, thiserror::Error)]
pub enum DownloadImageError {
    #[error("request failed: {0:?}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("response is missing content type")]
    MissingContentType,
    #[error("response has invalid content type")]
    InvalidContentType,
    #[error("content type is not a string")]
    ContentTypeIsNotString,
    #[error("content type ({0}) is unsupported")]
    UnsupportedContentType(String),
    #[error("failed to decode image: {0:?}")]
    DecodeImage(#[from] imagelib::ImageError),
    #[error("failed to parse URL: {0:?}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("unsuccessful status={0}")]
    Unsuccessful(reqwest::StatusCode),
}

pub fn orientation_to_degs(orientation: metadata::Orientation) -> f32 {
    match orientation {
        metadata::Orientation::Rotate90 | metadata::Orientation::Rotate90FlipH => 90.0,
        metadata::Orientation::Rotate180 => 180.0,
        metadata::Orientation::Rotate270 | metadata::Orientation::Rotate270FlipH => 270.0,
        metadata::Orientation::FlipHorizontal
        | metadata::Orientation::FlipVertical
        | metadata::Orientation::NoTransforms => 0.0,
    }
}

impl crate::CompoundImage {
    pub fn new(pixels: SlintRgba8Pixbuf, orientation: metadata::Orientation) -> Self {
        Self {
            img: slint::Image::from_rgba8(pixels),
            rotation: orientation_to_degs(orientation),
        }
    }
}

fn to_slint_pixbuf(img: &imagelib::RgbaImage) -> SlintRgba8Pixbuf {
    slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        img.as_raw(),
        img.width(),
        img.height(),
    )
}

#[derive(Debug)]
pub struct DecodedImage {
    pub id: ImageId,
    pub image: imagelib::RgbaImage,
    pub orientation: metadata::Orientation,
}

impl DecodedImage {
    pub fn as_compound(&self) -> CompoundImage {
        CompoundImage::new(to_slint_pixbuf(&self.image), self.orientation)
    }
}

#[derive(Clone, Copy)]
pub enum ImageDecodeJobType {
    AudioThumbnail,
    Regular,
}

pub enum EncodedImageData {
    Vec(Vec<u8>),
    Bytes(Bytes),
}

impl std::ops::Deref for EncodedImageData {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Vec(vec) => vec.as_slice(),
            Self::Bytes(bytes) => bytes,
        }
    }
}

impl From<Vec<u8>> for EncodedImageData {
    fn from(value: Vec<u8>) -> Self {
        Self::Vec(value)
    }
}

impl From<Bytes> for EncodedImageData {
    fn from(value: Bytes) -> Self {
        Self::Bytes(value)
    }
}

pub struct ImageDecodeJob {
    pub image: EncodedImageData,
    pub format: Option<ExtendedImageFormat>,
    pub typ: ImageDecodeJobType,
}

impl ImageDecodeJob {
    pub fn new(
        image: impl Into<EncodedImageData>,
        format: ExtendedImageFormat,
        typ: ImageDecodeJobType,
    ) -> Self {
        Self {
            image: image.into(),
            format: Some(format),
            typ,
        }
    }

    pub fn new_no_format(image: impl Into<EncodedImageData>, typ: ImageDecodeJobType) -> Self {
        Self {
            image: image.into(),
            format: None,
            typ,
        }
    }
}

#[derive(Debug)]
pub struct AnimationFrame {
    pub image: SlintRgba8Pixbuf,
    pub delay_ms: i64,
}

#[derive(Debug)]
pub enum Event {
    DownloadResult {
        id: ImageDownloadId,
        res: std::result::Result<(Bytes, ExtendedImageFormat), DownloadImageError>,
    },
    AudioThumbnailAvailable(DecodedImage),
    Decoded(DecodedImage),
    DecodedAnimation {
        id: ImageId,
        frames: Vec<AnimationFrame>,
    },
}

struct DecoderContext<'a> {
    msg_tx: &'a MessageSender,
    job_id: ImageId,
    job_type: ImageDecodeJobType,
}

impl<'a> DecoderContext<'a> {
    fn new(msg_tx: &'a MessageSender, job_id: ImageId, job_type: ImageDecodeJobType) -> Self {
        Self {
            msg_tx,
            job_id,
            job_type,
        }
    }

    fn handle_animation<'b>(&self, decoder: impl AnimationDecoder<'b>) -> anyhow::Result<()> {
        let mut slint_frames = Vec::new();
        for frame in decoder.into_frames() {
            let Ok(frame) = frame else {
                break;
            };

            let delay = frame.delay();
            let (num, denom) = delay.numer_denom_ms();
            let delay_ms = (num as f64 / denom as f64) as i64;
            slint_frames.push(AnimationFrame {
                image: to_slint_pixbuf(&frame.into_buffer()),
                delay_ms,
            });
        }

        self.msg_tx.image(Event::DecodedAnimation {
            id: self.job_id,
            frames: slint_frames,
        });

        Ok(())
    }

    fn handle_still(&self, mut decoder: impl imagelib::ImageDecoder) -> anyhow::Result<()> {
        let orientation = decoder
            .orientation()
            .unwrap_or(metadata::Orientation::NoTransforms);
        let image = DynamicImage::from_decoder(decoder);

        let decoded = match image {
            Ok(img) => img.to_rgba8(),
            Err(err) => {
                // TODO: should notify about failure
                error!(?err, "Failed to decode image");
                return Ok(());
            }
        };

        let img = DecodedImage {
            id: self.job_id,
            image: decoded,
            orientation,
        };

        match self.job_type {
            ImageDecodeJobType::AudioThumbnail => {
                self.msg_tx.image(Event::AudioThumbnailAvailable(img));
            }
            ImageDecodeJobType::Regular => {
                self.msg_tx.image(Event::Decoded(img));
            }
        }

        Ok(())
    }

    fn decode(self, job: ImageDecodeJob) -> anyhow::Result<()> {
        let format = if let Some(format) = job.format {
            format
        } else {
            match imagelib::guess_format(&job.image) {
                Ok(format) => ExtendedImageFormat::ImageLib(format),
                Err(err) => {
                    error!(?err, "Could not guess image format");
                    return Ok(());
                }
            }
        };

        let img_data: std::io::Cursor<&[u8]> = std::io::Cursor::new(&job.image);

        macro_rules! non_fatal {
            ($res:expr, $format:expr) => {
                match $res {
                    Ok(d) => d,
                    Err(err) => {
                        error!(?err, format = $format, "Failed to create decoder");
                        return Ok(());
                    }
                }
            };
        }

        match format {
            ExtendedImageFormat::ImageLib(format) => match format {
                ImageFormat::Png => {
                    let decoder = non_fatal!(PngDecoder::new(img_data), "PNG");
                    if decoder.is_apng().unwrap_or(false) {
                        self.handle_animation(non_fatal!(decoder.apng(), "APNG"))?;
                    } else {
                        self.handle_still(decoder)?;
                    }
                }
                ImageFormat::Gif => {
                    let decoder = non_fatal!(GifDecoder::new(img_data), "GIF");
                    self.handle_animation(decoder)?;
                }
                ImageFormat::WebP => {
                    let decoder = non_fatal!(WebPDecoder::new(img_data), "WebP");
                    if decoder.has_animation() {
                        self.handle_animation(decoder)?;
                    } else {
                        self.handle_still(decoder)?;
                    }
                }
                _ => {
                    let decoder = match ImageReader::with_format(img_data, format).into_decoder() {
                        Ok(d) => d,
                        Err(err) => {
                            error!(?err, "Failed to read image");
                            return Ok(());
                        }
                    };
                    self.handle_still(decoder)?;
                }
            },
            ExtendedImageFormat::JpegXl => {
                // TODO: handle animations
                // let image = jxl_oxide::JxlImage::builder().read(img_data).unwrap();
                // let header = image.image_header();
                // if let Some(anim) = &header.metadata.animation {
                // } else {
                // }
                let decoder =
                    non_fatal!(jxl_oxide::integration::JxlDecoder::new(img_data), "JPEG XL");
                self.handle_still(decoder)?;
            }
            ExtendedImageFormat::Jpeg2000 => {
                let decoder = non_fatal!(
                    hayro_jpeg2000::Image::new(
                        &job.image,
                        &hayro_jpeg2000::DecodeSettings {
                            resolve_palette_indices: true,
                            strict: false,
                            target_resolution: None,
                        },
                    ),
                    "JPEG 2000"
                );
                self.handle_still(decoder)?;
            }
            #[cfg(all(feature = "desktop", target_os = "linux"))]
            ExtendedImageFormat::Heif => {
                let reader = non_fatal!(ImageReader::new(img_data).with_guessed_format(), "HEIF");
                let decoder = non_fatal!(reader.into_decoder(), "HEIF");
                self.handle_still(decoder)?;
            }
        }

        Ok(())
    }
}

pub struct Decoder {
    job_tx: std::sync::mpsc::Sender<(ImageId, ImageDecodeJob)>,
}

impl Decoder {
    pub fn new(msg_tx: MessageSender) -> std::io::Result<Self> {
        let (job_tx, job_rx) = std::sync::mpsc::channel();

        std::thread::Builder::new()
            .name("image-decoder".to_owned())
            .spawn(move || {
                if let Err(err) = Self::image_decode_worker(job_rx, msg_tx) {
                    error!(?err, "Image decode worker failed");
                }
            })?;

        Ok(Self { job_tx })
    }

    pub fn queue_job(&self, id: ImageId, job: ImageDecodeJob) {
        let _ = self.job_tx.send((id, job));
    }

    fn image_decode_worker(
        job_rx: std::sync::mpsc::Receiver<(ImageId, ImageDecodeJob)>,
        msg_tx: MessageSender,
    ) -> anyhow::Result<()> {
        let span = debug_span!("image-decoder");
        let _entered = span.enter();

        #[cfg(all(feature = "desktop", target_os = "linux"))]
        libheif_rs::integration::image::register_all_decoding_hooks();
        hayro_jpeg2000::integration::register_decoding_hook();
        jxl_oxide::integration::register_image_decoding_hook();

        while let Ok((id, job)) = job_rx.recv() {
            debug!(?id, ?job.format, "Got job");
            DecoderContext::new(&msg_tx, id, job.typ).decode(job)?;
        }

        info!("Image decoding worker finished");

        Ok(())
    }
}

#[derive(Debug)]
pub enum ExtendedImageFormat {
    ImageLib(ImageFormat),
    JpegXl,
    Jpeg2000,
    #[cfg(all(feature = "desktop", target_os = "linux"))]
    Heif,
}

pub struct Downloader {
    msg_tx: crate::MessageSender,
    client: reqwest::Client,
    companion_ctx: CompanionContext,
}

impl Downloader {
    pub fn new(
        msg_tx: crate::MessageSender,
        client: reqwest::Client,
        companion_ctx: CompanionContext,
    ) -> Self {
        Self {
            msg_tx,
            client,
            companion_ctx,
        }
    }

    #[cfg_attr(not(target_os = "android"), tracing::instrument(skip_all, fields(url = %url)))]
    async fn download_image_http(
        client: &reqwest::Client,
        url: url::Url,
        headers: Option<HashMap<String, String>>,
    ) -> std::result::Result<(Bytes, ExtendedImageFormat), DownloadImageError> {
        debug!("Starting image download");
        let random_user_agent = crate::user_agent::random_browser_user_agent(url.domain());
        let mut request = client.get(url);
        let mut did_set_user_agent = false;
        if let Some(headers) = headers {
            let header_map = crate::map_to_header_map(&headers);
            did_set_user_agent = header_map.contains_key(reqwest::header::USER_AGENT);
            request = request.headers(header_map);
        }
        if !did_set_user_agent {
            request = request.header(reqwest::header::USER_AGENT, random_user_agent);
        }

        let resp = request.send().await?;
        if !resp.status().is_success() {
            return Err(DownloadImageError::Unsuccessful(resp.status()));
        }

        let headers = resp.headers();
        let content_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .ok_or(DownloadImageError::MissingContentType)?
            .to_str()
            .map_err(|_| DownloadImageError::ContentTypeIsNotString)?;
        let format = match ImageFormat::from_mime_type(content_type) {
            Some(f) => ExtendedImageFormat::ImageLib(f),
            None => match content_type {
                "image/jxl" => ExtendedImageFormat::JpegXl,
                "image/jp2" | "image/jpx" | "image/jpm" | "video/mj2" => {
                    ExtendedImageFormat::Jpeg2000
                }
                #[cfg(all(feature = "desktop", target_os = "linux"))]
                "image/heif" | "image/heic" => ExtendedImageFormat::Heif,
                _ => {
                    return Err(DownloadImageError::UnsupportedContentType(
                        content_type.to_string(),
                    ));
                }
            },
        };

        let body = resp.bytes().await?;
        Ok((body, format))
    }

    #[cfg_attr(not(target_os = "android"), tracing::instrument(skip_all, fields(url = %url)))]
    async fn download_image_comp(
        ctx: &CompanionContext,
        url: url::Url,
    ) -> std::result::Result<(Bytes, ExtendedImageFormat), DownloadImageError> {
        debug!("Starting image download");
        todo!()
    }

    pub fn queue_download(&self, id: u32, url: String, headers: Option<HashMap<String, String>>) {
        let url = url::Url::parse(&url).unwrap();
        let tx = self.msg_tx.clone();

        match url.scheme() {
            "http" | "https" => {
                let client = self.client.clone();
                tokio::spawn(async move {
                    let res = Self::download_image_http(&client, url, headers).await;
                    tx.image(Event::DownloadResult { id, res });
                });
            }
            "fcomp" => {
                let ctx = self.companion_ctx.clone();
                tokio::spawn(async move {
                });
            }
            _ => todo!(),
        }
    }
}
