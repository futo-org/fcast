use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use bytes::Bytes;
use fcast_protocol::companion;
use image as imagelib;
// Re-exported so the UI layer can name a decoded frame without depending on
// `image`.
pub use imagelib::RgbaImage;
use imagelib::{DynamicImage, ImageFormat, ImageReader, metadata};
use tracing::{debug, debug_span, error, info};

use crate::{MessageSender, fcast::CompanionContext, media_formats, utils::map_to_header_map};

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
    #[error("URL scheme ({0}) is unsupported")]
    UnsupportedScheme(String),
    #[error("unsuccessful status={0}")]
    Unsuccessful(reqwest::StatusCode),
    #[error("failed to get resource info")]
    FailedToGetInfo,
    #[error("invalid FCompanion URL")]
    InvalidCompUrl,
    #[error("FCompanion provider not found")]
    ProviderNotFound,
    #[error("FCompanion request failed")]
    CompRequestFailed,
    #[error("FCompanion resource not found")]
    ResourceNotFound,
    #[error("FCompanion resource failed: {0:?}")]
    CompanionResource(crate::fcast::CompanionResourceOutcome),
    #[error("FCompanion request timed out")]
    CompanionTimeout,
}

async fn collect_companion_resource(
    mut resource_rx: tokio::sync::mpsc::UnboundedReceiver<companion::ResourceResponse>,
    timeout: Duration,
) -> Result<Vec<u8>, DownloadImageError> {
    let mut res = Vec::new();
    let mut received = false;
    loop {
        let response = tokio::time::timeout(timeout, resource_rx.recv())
            .await
            .map_err(|_| DownloadImageError::CompanionTimeout)?;
        match response {
            Some(response) => {
                received = true;
                match response.result {
                    companion::GetResourceResult::Success(buf) => res.extend_from_slice(&buf),
                    companion::GetResourceResult::EndOfStream => break,
                    companion::GetResourceResult::NotFound => {
                        return Err(DownloadImageError::ResourceNotFound);
                    }
                    result => {
                        return Err(DownloadImageError::CompanionResource(
                            crate::fcast::companion_resource_outcome(&result),
                        ));
                    }
                }
            }
            None => break,
        }
    }
    if !received {
        return Err(DownloadImageError::CompRequestFailed);
    }
    Ok(res)
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

#[derive(Debug)]
pub struct DecodedImage {
    pub id: ImageId,
    pub image: imagelib::RgbaImage,
    pub orientation: metadata::Orientation,
    /// Source format short name (see `media_formats::Image::to_str`).
    pub format: &'static str,
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
    pub format: Option<media_formats::Image>,
    pub typ: ImageDecodeJobType,
}

impl ImageDecodeJob {
    pub fn new(
        image: impl Into<EncodedImageData>,
        format: media_formats::Image,
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
        res: std::result::Result<(Bytes, media_formats::Image), DownloadImageError>,
    },
    AudioThumbnailAvailable(DecodedImage),
    Decoded(DecodedImage),
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

    fn handle_still(
        &self,
        mut decoder: impl imagelib::ImageDecoder,
        format: &'static str,
    ) -> anyhow::Result<()> {
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
            format,
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
                Ok(format) => media_formats::Image::ImageLib(format),
                Err(err) => {
                    error!(?err, "Could not guess image format");
                    return Ok(());
                }
            }
        };

        let format_str = format.to_str();
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
            media_formats::Image::ImageLib(format) => {
                // Animations are decoded and looped by the player pipeline;
                // anything reaching here (cover art, odd mime) shows as a still.
                let decoder = match ImageReader::with_format(img_data, format).into_decoder() {
                    Ok(d) => d,
                    Err(err) => {
                        error!(?err, "Failed to read image");
                        return Ok(());
                    }
                };
                self.handle_still(decoder, format_str)?;
            }
            media_formats::Image::JpegXl => {
                // TODO: handle animations
                let decoder =
                    non_fatal!(jxl_oxide::integration::JxlDecoder::new(img_data), "JPEG XL");
                self.handle_still(decoder, format_str)?;
            }
            media_formats::Image::Jpeg2000 => {
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
                self.handle_still(decoder, format_str)?;
            }
            #[cfg(not(target_os = "android"))]
            media_formats::Image::Heif => {
                let reader = non_fatal!(ImageReader::new(img_data).with_guessed_format(), "HEIF");
                let decoder = non_fatal!(reader.into_decoder(), "HEIF");
                self.handle_still(decoder, format_str)?;
            }
        }

        Ok(())
    }
}

pub fn init_extra_decoders() {
    #[cfg(not(target_os = "android"))]
    libheif_rs::integration::image::register_all_decoding_hooks();
    hayro_jpeg2000::integration::register_decoding_hook();
    jxl_oxide::integration::register_image_decoding_hook();
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

        while let Ok((id, job)) = job_rx.recv() {
            debug!(?id, ?job.format, "Got job");
            DecoderContext::new(&msg_tx, id, job.typ).decode(job)?;
        }

        info!("Image decoding worker finished");

        Ok(())
    }
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

    fn format_from_content_type(
        ctype: &str,
    ) -> std::result::Result<media_formats::Image, DownloadImageError> {
        Ok(match ImageFormat::from_mime_type(ctype) {
            Some(f) => media_formats::Image::ImageLib(f),
            None => match ctype {
                "image/jxl" => media_formats::Image::JpegXl,
                "image/jp2" | "image/jpx" | "image/jpm" | "video/mj2" => {
                    media_formats::Image::Jpeg2000
                }
                #[cfg(not(target_os = "android"))]
                "image/heif" | "image/heic" => media_formats::Image::Heif,
                _ => {
                    return Err(DownloadImageError::UnsupportedContentType(
                        ctype.to_string(),
                    ));
                }
            },
        })
    }

    #[cfg_attr(not(target_os = "android"), tracing::instrument(skip_all, fields(url = %url)))]
    async fn download_image_http(
        client: &reqwest::Client,
        url: url::Url,
        headers: Option<HashMap<String, String>>,
    ) -> std::result::Result<(Bytes, media_formats::Image), DownloadImageError> {
        debug!("Starting image download");
        let random_user_agent = crate::user_agent::random_browser_user_agent(url.domain());
        let mut request = client.get(url);
        let mut did_set_user_agent = false;
        if let Some(headers) = headers {
            let header_map = map_to_header_map(&headers);
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
        let format = Self::format_from_content_type(content_type)?;

        let body = resp.bytes().await?;
        Ok((body, format))
    }

    #[cfg_attr(not(target_os = "android"), tracing::instrument(skip_all, fields(url = %url)))]
    async fn download_image_comp(
        ctx: &CompanionContext,
        url: url::Url,
    ) -> std::result::Result<(Bytes, media_formats::Image), DownloadImageError> {
        debug!("Starting image download");

        let url = crate::fcompsrc::FCompUrl::new(&url).ok_or(DownloadImageError::InvalidCompUrl)?;

        let provider = ctx
            .get_provider(url.provider_id)
            .ok_or(DownloadImageError::ProviderNotFound)?;
        let mut info = provider
            .get_resource_info(url.resource_id, &url.route)
            .map_err(|_| DownloadImageError::FailedToGetInfo)?;
        let info = tokio::time::timeout(crate::fcast::COMPANION_REQUEST_TIMEOUT, info.recv())
            .await
            .map_err(|_| DownloadImageError::CompanionTimeout)?
            .ok_or(DownloadImageError::FailedToGetInfo)?;
        let info = info.borrow_dependent();
        let format = Self::format_from_content_type(info.content_type())?;
        match crate::fcast::companion_info_outcome(info.status()) {
            crate::fcast::CompanionResourceOutcome::Success => (),
            crate::fcast::CompanionResourceOutcome::NotFound => {
                return Err(DownloadImageError::ResourceNotFound);
            }
            crate::fcast::CompanionResourceOutcome::EndOfStream => {
                return Ok((Bytes::new(), format));
            }
            outcome => return Err(DownloadImageError::CompanionResource(outcome)),
        }

        let resource_rx = provider
            .get_resource(url.resource_id, &url.route, None)
            .map_err(|_| DownloadImageError::CompRequestFailed)?;
        let res = collect_companion_resource(resource_rx, crate::fcast::COMPANION_REQUEST_TIMEOUT)
            .await?;

        Ok((Bytes::from_owner(res), format))
    }

    pub fn queue_download(&self, id: u32, url: String, headers: Option<HashMap<String, String>>) {
        let tx = self.msg_tx.clone();

        // `url` is sender-supplied and unvalidated, so the parse and the scheme
        // dispatch must report rather than panic.
        let url = match url::Url::parse(&url) {
            Ok(url) => url,
            Err(err) => {
                tx.image(Event::DownloadResult {
                    id,
                    res: Err(DownloadImageError::InvalidUrl(err)),
                });
                return;
            }
        };

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
                    let res = Self::download_image_comp(&ctx, url).await;
                    tx.image(Event::DownloadResult { id, res });
                });
            }
            scheme => {
                tx.image(Event::DownloadResult {
                    id,
                    res: Err(DownloadImageError::UnsupportedScheme(scheme.to_owned())),
                });
            }
        }
    }
}

pub fn find_formats() -> HashSet<media_formats::Image> {
    use media_formats::Image;

    macro_rules! il {
        ($fmt:ident) => {
            Image::ImageLib(image::ImageFormat::$fmt)
        };
    }

    HashSet::from([
        il!(Png),
        il!(Jpeg),
        il!(Gif),
        il!(WebP),
        il!(Pnm),
        il!(Tiff),
        il!(Tga),
        il!(Dds),
        il!(Bmp),
        il!(Ico),
        il!(Farbfeld),
        il!(Avif),
        il!(Qoi),
        Image::Jpeg2000,
        Image::JpegXl,
        #[cfg(not(target_os = "android"))]
        Image::Heif,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    type Events = tokio::sync::mpsc::UnboundedReceiver<crate::message::Message>;

    fn downloader() -> (Downloader, Events) {
        // `reqwest::Client::new()` panics without a process-wide provider on
        // the `rustls-no-provider` build. Idempotent, so racing tests are fine.
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let downloader = Downloader::new(
            MessageSender::new(tx),
            reqwest::Client::new(),
            CompanionContext::new(),
        );
        (downloader, rx)
    }

    /// Pull the single expected `DownloadResult` and unwrap its error.
    /// `try_recv` is deliberate: both rejection paths post synchronously.
    fn download_error(events: &mut Events, expected_id: ImageDownloadId) -> DownloadImageError {
        match events.try_recv().expect("no image event was posted") {
            crate::message::Message::Image(Event::DownloadResult { id, res }) => {
                assert_eq!(id, expected_id);
                res.expect_err("expected the download to fail")
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn unparseable_url_is_reported_not_panicked() {
        let (downloader, mut events) = downloader();

        downloader.queue_download(7, "not a url".to_owned(), None);

        let err = download_error(&mut events, 7);
        assert!(
            matches!(err, DownloadImageError::InvalidUrl(_)),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn unsupported_url_scheme_is_reported_not_panicked() {
        let (downloader, mut events) = downloader();

        downloader.queue_download(9, "ftp://example.invalid/cover.png".to_owned(), None);

        let err = download_error(&mut events, 9);
        assert!(
            matches!(&err, DownloadImageError::UnsupportedScheme(s) if s == "ftp"),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn companion_part_timeout_resets_after_each_part() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let task = tokio::spawn(collect_companion_resource(rx, Duration::from_secs(1)));

        tokio::time::advance(Duration::from_millis(900)).await;
        tx.send(companion::ResourceResponse {
            request_id: 1,
            part: 0,
            total_parts: 2,
            result: companion::GetResourceResult::Success(vec![1, 2, 3].into()),
        })
        .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(900)).await;
        tx.send(companion::ResourceResponse {
            request_id: 1,
            part: 1,
            total_parts: 2,
            result: companion::GetResourceResult::EndOfStream,
        })
        .unwrap();

        assert_eq!(task.await.unwrap().unwrap(), vec![1, 2, 3]);
    }
}
