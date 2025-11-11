use std::rc::Rc;

use anyhow::Result;
use clap::{Args, Subcommand};
use xshell::{cmd, Shell};

use crate::{sh, workspace};

#[derive(Subcommand)]
pub enum TestCorpusCommand {
    None,
}

#[derive(Args)]
pub struct TestCorpusArgs {
    #[clap(subcommand)]
    pub cmd: TestCorpusCommand,
}

#[derive(Debug)]
enum VideoCodec {
    H264,
    H265,
    Vp8,
    Vp9,
    Av1,
    Theora,
}

impl VideoCodec {
    pub fn encoder(&self) -> &'static str {
        match self {
            VideoCodec::H264 => "x264enc",
            VideoCodec::H265 => "x265enc ! h265parse",
            VideoCodec::Vp8 => "vp8enc cpu-used=4",
            VideoCodec::Vp9 => "vp9enc cpu-used=4 ! vp9parse",
            VideoCodec::Av1 => "av1enc cpu-used=4",
            VideoCodec::Theora => "theoraenc",
        }
    }
}

enum AudioCodec {
    Mp3,
    Opus,
    Flac,
    Aac,
    Wav,
}

impl AudioCodec {
    pub fn encoder(&self) -> &'static str {
        match self {
            AudioCodec::Mp3 => "lamemp3enc",
            AudioCodec::Opus => "opusenc",
            AudioCodec::Flac => "flacenc",
            AudioCodec::Aac => "fdkaacenc",
            AudioCodec::Wav => "wavenc",
        }
    }
}

fn run_cmd(sh: &Rc<Shell>, cmd: &str) -> Result<()> {
    cmd!(sh, "sh -c {cmd}").run()?;
    Ok(())
}

impl TestCorpusArgs {
    pub fn run(self) -> Result<()> {
        let sh = sh();
        let root_path = workspace::root_path()?;
        let _p = sh.push_dir(root_path.clone());
        sh.create_dir("test_corpus")?;
        let _p_corpus = sh.push_dir("test_corpus");

        for vcodec in [
            VideoCodec::H264,
            VideoCodec::H265,
            VideoCodec::Vp8,
            VideoCodec::Vp9,
            VideoCodec::Av1,
        ] {
            run_cmd(
                &sh,
                &format!(
                    r#"
                    gst-launch-1.0 videotestsrc num-buffers=30 ! video/x-raw,framerate=30/1 ! {} ! tee name=muxtee  ! queue ! qtmux ! filesink location=single_video_{vcodec:?}.mp4 \
                                                                                                            muxtee. ! queue ! matroskamux ! filesink location=single_video_{vcodec:?}.mkv
                    "#,
                    vcodec.encoder()
                ),
            )?;
        }

        for vcodec in [VideoCodec::Vp8, VideoCodec::Vp9, VideoCodec::Av1] {
            run_cmd(
                &sh,
                &format!(
                    r#"
                    gst-launch-1.0 videotestsrc num-buffers=30 ! video/x-raw,framerate=30/1 ! {} ! webmmux ! filesink location=single_video_{vcodec:?}.webm
                    "#,
                    vcodec.encoder()
                ),
            )?;
        }

        for vcodec in [VideoCodec::Theora] {
            run_cmd(
                &sh,
                &format!(
                    r#"
                    gst-launch-1.0 videotestsrc num-buffers=30 ! video/x-raw,framerate=30/1 ! {} ! oggmux ! filesink location=single_video_{vcodec:?}.ogg
                    "#,
                    vcodec.encoder()
                ),
            )?;
        }

        Ok(())
    }
}
