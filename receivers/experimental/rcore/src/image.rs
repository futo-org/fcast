use std::collections::HashMap;

use bytes::Bytes;
use imagelib::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, metadata};
use tracing::{debug, debug_span, error, info};

use crate::{CompoundImage, EventSender, SlintRgba8Pixbuf};

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

impl crate::CompoundImage {
    pub fn new(
        pixels: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
        orientation: metadata::Orientation,
    ) -> Self {
        let rotation = match orientation {
            metadata::Orientation::Rotate90 | metadata::Orientation::Rotate90FlipH => 90.0,
            metadata::Orientation::Rotate180 => 180.0,
            metadata::Orientation::Rotate270 | metadata::Orientation::Rotate270FlipH => 270.0,
            metadata::Orientation::FlipHorizontal
            | metadata::Orientation::FlipVertical
            | metadata::Orientation::NoTransforms => 0.0,
        };
        Self {
            img: slint::Image::from_rgba8(pixels),
            rotation,
        }
    }
}

#[derive(Debug)]
pub struct DecodedImage {
    pub id: ImageId,
    pub pixels: SlintRgba8Pixbuf,
    pub orientation: metadata::Orientation,
}

impl DecodedImage {
    pub fn into_compound(self) -> CompoundImage {
        CompoundImage::new(self.pixels, self.orientation)
    }
}

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
    pub format: Option<ImageFormat>,
    pub typ: ImageDecodeJobType,
}

impl ImageDecodeJob {
    pub fn new(
        image: impl Into<EncodedImageData>,
        format: ImageFormat,
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
pub enum Event {
    DownloadResult {
        id: ImageDownloadId,
        res: std::result::Result<(Bytes, ImageFormat), DownloadImageError>,
    },
    AudioThumbnailAvailable(DecodedImage),
    AudioThumbnailBlurAvailable(DecodedImage),
    Decoded(DecodedImage),
}

pub struct Decoder {
    job_tx: std::sync::mpsc::Sender<(ImageId, ImageDecodeJob)>,
}

impl Decoder {
    pub fn new(event_tx: crate::EventSender) -> std::io::Result<Self> {
        let (job_tx, job_rx) = std::sync::mpsc::channel();

        std::thread::Builder::new()
            .name("image-decoder".to_owned())
            .spawn(move || {
                if let Err(err) = Self::image_decode_worker(job_rx, event_tx) {
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
        event_tx: EventSender,
    ) -> anyhow::Result<()> {
        let span = debug_span!("image-decoder");
        let _entered = span.enter();

        #[cfg(all(feature = "desktop", target_os = "linux"))]
        libheif_rs::integration::image::register_all_decoding_hooks();
        hayro_jpeg2000::integration::register_decoding_hook();
        jxl_oxide::integration::register_image_decoding_hook();

        while let Ok((id, job)) = job_rx.recv() {
            debug!(?id, ?job.format, "Got job");

            let img_data: std::io::Cursor<&[u8]> = std::io::Cursor::new(&job.image);
            let reader_res = match job.format {
                Some(format) => ImageReader::with_format(img_data, format).into_decoder(),
                None => match ImageReader::new(img_data).with_guessed_format() {
                    Ok(guessed) => guessed.into_decoder(),
                    Err(err) => {
                        error!(?err, "Failed to guess image format from data");
                        continue;
                    }
                },
            };

            let (decoded_res, orientation) = match reader_res {
                Ok(mut decoder) => {
                    let orientation = decoder
                        .orientation()
                        .unwrap_or(metadata::Orientation::NoTransforms);
                    let image = DynamicImage::from_decoder(decoder);
                    (image, orientation)
                }
                Err(err) => {
                    error!(?err, "Failed to read image");
                    continue;
                }
            };

            let decoded = match decoded_res {
                Ok(img) => img.to_rgba8(),
                Err(err) => {
                    // TODO: should notify about failure
                    error!(?err, "Failed to decode image");
                    continue;
                }
            };

            fn to_slint_pixbuf(img: &imagelib::RgbaImage) -> SlintRgba8Pixbuf {
                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    img.as_raw(),
                    img.width(),
                    img.height(),
                )
            }

            let img = DecodedImage {
                id,
                pixels: to_slint_pixbuf(&decoded),
                orientation,
            };

            match job.typ {
                ImageDecodeJobType::AudioThumbnail => {
                    event_tx.send(crate::Event::Image(Event::AudioThumbnailAvailable(img)))?;
                    let blured = to_slint_pixbuf(&imagelib::imageops::fast_blur(&decoded, 64.0));
                    event_tx.send(crate::Event::Image(Event::AudioThumbnailBlurAvailable(
                        DecodedImage {
                            id,
                            pixels: blured,
                            orientation,
                        },
                    )))?;
                }
                ImageDecodeJobType::Regular => {
                    event_tx.send(crate::Event::Image(Event::Decoded(img)))?;
                }
            }
        }

        info!("Image decoding worker finished");

        Ok(())
    }
}

pub struct Downloader {
    event_tx: crate::EventSender,
    client: reqwest::Client,
}

impl Downloader {
    pub fn new(event_tx: crate::EventSender, client: reqwest::Client) -> Self {
        Self { event_tx, client }
    }

    #[cfg_attr(not(target_os = "android"), tracing::instrument(skip_all, fields(url = url)))]
    async fn download_image(
        client: &reqwest::Client,
        url: &str,
        headers: Option<HashMap<String, String>>,
    ) -> std::result::Result<(Bytes, ImageFormat), DownloadImageError> {
        let url = url::Url::parse(url)?;
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
        let format = ImageFormat::from_mime_type(content_type).ok_or(
            DownloadImageError::UnsupportedContentType(content_type.to_string()),
        )?;

        let body = resp.bytes().await?;
        Ok((body, format))
    }

    pub fn queue_download(&self, id: u32, url: String, headers: Option<HashMap<String, String>>) {
        let client = self.client.clone();
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            let res = Self::download_image(&client, &url, headers).await;
            let _ = tx.send(crate::Event::Image(Event::DownloadResult { id, res }));
        });
    }
}
