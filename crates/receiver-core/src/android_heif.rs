//! HEIC/HEIF stills through android's own image decoder.
//!
//! libheif wants an HEVC decoder beside it (libde265, or ffmpeg's) and
//! nothing in this tree cross-builds one for android, so the desktop path
//! (`libheif_rs::integration`) has no android equivalent. The platform has
//! decoded HEIF since API 28 and the app's minSdk is 28, so BitmapFactory
//! is the decoder: it answers a software ARGB_8888 bitmap whose memory
//! layout IS RGBA, and one `copyPixelsToBuffer` is the whole conversion.
//!
//! Registered as an `image` decoding hook, the same seam libheif, jxl and
//! JPEG 2000 use, so both lanes get it: fimagedec's `FormatHint::Heif` and
//! the in-GUI decoder both reach it through `ImageReader`.

use std::io::Read;

use image::error::{DecodingError, ImageFormatHint};
use image::hooks::GenericReader;
use image::{ColorType, ImageDecoder, ImageError, ImageResult};
use tracing::debug;

/// An ftyp brand at the two offsets a file can carry it: the major brand,
/// and the first entry of the compatible-brand list (the four bytes after
/// `minor_version`). Same signatures libheif-rs registers, minus avif: the
/// image crate decodes that one itself through dav1d.
macro_rules! brand {
    ($a:literal, $b:literal, $c:literal, $d:literal) => {
        [
            &[
                0, 0, 0, 0, b'f', b't', b'y', b'p', $a, $b, $c, $d, 0, 0, 0, 0,
            ],
            &[
                0, 0, 0, 0, b'f', b't', b'y', b'p', 0, 0, 0, 0, $a, $b, $c, $d,
            ],
        ]
    };
}

/// Skips the box length, and whichever of the two brand slots the signature
/// does not name (`minor_version` for the major-brand form).
static MASKS: [&[u8]; 2] = [
    &[
        0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0,
    ],
    &[
        0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff,
    ],
];

/// HEVC Main or Main Still.
static HEIC_BRAND: [&[u8]; 2] = brand!(b'h', b'e', b'i', b'c');
/// HEVC Main 10.
static HEIX_BRAND: [&[u8]; 2] = brand!(b'h', b'e', b'i', b'x');
/// Structural brands, no coding algorithm implied.
static MIF1_BRAND: [&[u8]; 2] = brand!(b'm', b'i', b'f', b'1');
static MIF2_BRAND: [&[u8]; 2] = brand!(b'm', b'i', b'f', b'2');

pub fn register_decoding_hooks() {
    for (extension, brands) in [
        ("heif", [MIF1_BRAND, MIF2_BRAND]),
        ("heic", [HEIC_BRAND, HEIX_BRAND]),
    ] {
        let registered = image::hooks::register_decoding_hook(
            extension.into(),
            Box::new(|reader| Ok(Box::new(PlatformHeif::new(reader)?))),
        );
        if !registered {
            continue;
        }
        for brand in brands {
            image::hooks::register_format_detection_hook(
                extension.into(),
                brand[0],
                Some(MASKS[0]),
            );
            image::hooks::register_format_detection_hook(
                extension.into(),
                brand[1],
                Some(MASKS[1]),
            );
        }
    }
}

fn image_error(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> ImageError {
    ImageError::Decoding(DecodingError::new(ImageFormatHint::Name("heif".into()), err))
}

/// What a step inside the local frame failed with. A named type because
/// `with_local_frame_returning_local` wants an error a JNI failure converts
/// into, which a bare `String` is not.
#[derive(Debug)]
struct Failed(String);

impl From<jni::errors::Error> for Failed {
    fn from(err: jni::errors::Error) -> Self {
        Self(err.to_string())
    }
}

/// One decoded still. The platform decodes in one call, so the whole image
/// is already in hand by the time the trait asks for anything.
struct PlatformHeif {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

impl PlatformHeif {
    fn new(mut reader: GenericReader<'_>) -> ImageResult<Self> {
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(ImageError::IoError)?;
        decode(&bytes).map_err(image_error)
    }
}

impl ImageDecoder for PlatformHeif {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn color_type(&self) -> ColorType {
        ColorType::Rgba8
    }

    fn read_image(self, buf: &mut [u8]) -> ImageResult<()> {
        if buf.len() != self.rgba.len() {
            return Err(image_error(format!(
                "buffer is {} bytes for a {}x{} image",
                buf.len(),
                self.width,
                self.height
            )));
        }
        buf.copy_from_slice(&self.rgba);
        Ok(())
    }

    fn read_image_boxed(self: Box<Self>, buf: &mut [u8]) -> ImageResult<()> {
        (*self).read_image(buf)
    }
}

/// `BitmapFactory.decodeByteArray` with options that rule out everything
/// that would make the bytes something other than straight RGBA: a
/// non-8888 config, premultiplied alpha, density scaling.
fn decode(bytes: &[u8]) -> Result<PlatformHeif, String> {
    let ctx = ndk_context::android_context();
    let vm = unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }.map_err(|err| err.to_string())?;
    let mut env = vm.attach_current_thread().map_err(|err| err.to_string())?;
    let decoded = decode_with(&mut env, bytes);
    if decoded.is_err() {
        // ONE clear for every failure path below. A JNI call that fails
        // leaves its exception pending on the thread, and the next call made
        // on that thread from anywhere in the process fails on it instead of
        // on its own merits. The decode threads detach today, which is the
        // only reason this was survivable.
        let _ = env.exception_clear();
    }
    decoded
}

fn decode_with(env: &mut jni::JNIEnv, bytes: &[u8]) -> Result<PlatformHeif, String> {
    use jni::objects::{JObject, JValue};

    // The options, the config and the encoded byte[] live in this frame and
    // go with its pop. The BITMAP deliberately outlives it (that is what
    // this variant of the call is for) and is made an auto-local below, so
    // the megabytes it holds are released at the end of this function rather
    // than whenever the thread happens to detach.
    let result = env.with_local_frame_returning_local(16, |env| -> Result<JObject<'_>, Failed> {
        let options = env
            .new_object("android/graphics/BitmapFactory$Options", "()V", &[])
            .map_err(|err| Failed(format!("BitmapFactory.Options: {err}")))?;
        let argb8888 = env
            .get_static_field(
                "android/graphics/Bitmap$Config",
                "ARGB_8888",
                "Landroid/graphics/Bitmap$Config;",
            )
            .and_then(|config| config.l())
            .map_err(|err| Failed(format!("Bitmap.Config.ARGB_8888: {err}")))?;
        env.set_field(
            &options,
            "inPreferredConfig",
            "Landroid/graphics/Bitmap$Config;",
            JValue::Object(&argb8888),
        )
        .and_then(|()| env.set_field(&options, "inPremultiplied", "Z", JValue::Bool(0)))
        .and_then(|()| env.set_field(&options, "inScaled", "Z", JValue::Bool(0)))
        .map_err(|err| Failed(format!("BitmapFactory.Options fields: {err}")))?;

        let encoded = env
            .byte_array_from_slice(bytes)
            .map_err(|err| Failed(format!("byte[] for the encoded image: {err}")))?;
        let bitmap = env
            .call_static_method(
                "android/graphics/BitmapFactory",
                "decodeByteArray",
                "([BIILandroid/graphics/BitmapFactory$Options;)Landroid/graphics/Bitmap;",
                &[
                    JValue::Object(&encoded),
                    JValue::Int(0),
                    JValue::Int(bytes.len() as i32),
                    JValue::Object(&options),
                ],
            )
            .and_then(|bitmap| bitmap.l())
            .map_err(|err| Failed(format!("BitmapFactory.decodeByteArray: {err}")))?;
        // A refusal is a null return rather than an exception, and covers
        // both an unsupported file and a device whose HEVC decoder said no.
        if bitmap.is_null() {
            return Err(Failed(
                "the platform decoder refused the image".to_string(),
            ));
        }
        Ok(bitmap)
    });
    let bitmap = env.auto_local(result.map_err(|Failed(err)| err)?);

    // The config is a HINT: `inPreferredConfig` is ignored for a bitmap the
    // platform will not give in 8888, and a 10-bit HEIF can come back
    // RGBA_F16 or RGBA_1010102. Those are 8 and 4 bytes per pixel, so the
    // stride arithmetic below would pass and the bytes would be read as
    // RGBA8: half the picture, scrambled. Asked rather than assumed.
    let config = env
        .call_method(&bitmap, "getConfig", "()Landroid/graphics/Bitmap$Config;", &[])
        .and_then(|config| config.l())
        .map_err(|err| format!("Bitmap.getConfig: {err}"))?;
    let config = env.auto_local(config);
    let argb8888 = env
        .get_static_field(
            "android/graphics/Bitmap$Config",
            "ARGB_8888",
            "Landroid/graphics/Bitmap$Config;",
        )
        .and_then(|value| value.l())
        .map_err(|err| format!("Bitmap.Config.ARGB_8888: {err}"))?;
    let argb8888 = env.auto_local(argb8888);
    if !env
        .is_same_object(&config, &argb8888)
        .map_err(|err| format!("comparing the bitmap config: {err}"))?
    {
        return Err("the platform decoded the image to a config other than ARGB_8888".to_string());
    }

    let int_call = |env: &mut jni::JNIEnv, name: &str| -> Result<i32, String> {
        env.call_method(&bitmap, name, "()I", &[])
            .and_then(|value| value.i())
            .map_err(|err| format!("Bitmap.{name}: {err}"))
    };
    let width = int_call(env, "getWidth")?;
    let height = int_call(env, "getHeight")?;
    let row_bytes = int_call(env, "getRowBytes")?;
    let byte_count = int_call(env, "getByteCount")?;
    if width <= 0 || height <= 0 || row_bytes < width * 4 || byte_count < row_bytes * height {
        return Err(format!(
            "implausible bitmap: {width}x{height}, {row_bytes} per row, {byte_count} total"
        ));
    }
    debug!(width, height, row_bytes, "decoded a HEIF still on the platform");

    // copyPixelsToBuffer writes getByteCount() bytes, so the destination is
    // sized from the bitmap and not from the picture: a padded stride is
    // legal and the rows are moved down afterwards.
    let mut raw = vec![0u8; byte_count as usize];
    // SAFETY: `raw` outlives the buffer, which is dropped at the end of this
    // function, and java only writes into it during the copy below.
    let buffer = unsafe { env.new_direct_byte_buffer(raw.as_mut_ptr(), raw.len()) }
        .map_err(|err| format!("direct ByteBuffer: {err}"))?;
    let buffer = env.auto_local(buffer);
    let copied = env.call_method(
        &bitmap,
        "copyPixelsToBuffer",
        "(Ljava/nio/Buffer;)V",
        &[JValue::Object(buffer.as_ref())],
    );
    // Eagerly, rather than waiting for the auto-local: the pixels are the
    // megabytes and `recycle` is what frees them on the java side.
    let _ = env.call_method(&bitmap, "recycle", "()V", &[]);
    copied.map_err(|err| format!("Bitmap.copyPixelsToBuffer: {err}"))?;

    let (width, height) = (width as u32, height as u32);
    let stride = row_bytes as usize;
    let tight = width as usize * 4;
    let rgba = if stride == tight {
        raw.truncate(tight * height as usize);
        raw
    } else {
        let mut rgba = Vec::with_capacity(tight * height as usize);
        for row in 0..height as usize {
            rgba.extend_from_slice(&raw[row * stride..row * stride + tight]);
        }
        rgba
    };

    Ok(PlatformHeif {
        width,
        height,
        rgba,
    })
}
