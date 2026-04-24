use drm_fourcc::DrmFourcc;

pub fn fourcc_from_plane(plane: i32, dma_drm_format: DrmFourcc) -> DrmFourcc {
    macro_rules! two_plane {
        ($first:ident, $second:ident) => {
            if plane == 0 {
                DrmFourcc::$first
            } else {
                DrmFourcc::$second
            }
        };
    }

    macro_rules! three_plane {
        ($first:ident, $second:ident, $third:ident) => {
            if plane == 0 {
                DrmFourcc::$first
            } else if plane == 1 {
                DrmFourcc::$second
            } else {
                DrmFourcc::$third
            }
        };
    }

    match dma_drm_format {
        DrmFourcc::Nv12
        | DrmFourcc::Nv15
        | DrmFourcc::Nv16
        | DrmFourcc::Nv21
        | DrmFourcc::Nv24
        | DrmFourcc::Nv42
        | DrmFourcc::Nv61 => two_plane!(R8, Gr88),
        DrmFourcc::P010 | DrmFourcc::P012 | DrmFourcc::P016 | DrmFourcc::P210 => {
            two_plane!(R16, Gr1616)
        }
        DrmFourcc::Q401 | DrmFourcc::Q410 => three_plane!(R16, R16, R16),
        DrmFourcc::Bgr233 | DrmFourcc::Rgb332 => DrmFourcc::R8,
        DrmFourcc::Rgb565 => todo!(),
        DrmFourcc::Rgb565_a8 => two_plane!(Rgb565, R8),
        DrmFourcc::Rgb888_a8
        | DrmFourcc::Rgbx8888_a8
        | DrmFourcc::Xbgr8888_a8
        | DrmFourcc::Xrgb8888_a8 => two_plane!(Rgb888, R8),
        DrmFourcc::Yuv410
        | DrmFourcc::Yvu410
        | DrmFourcc::Yuv411
        | DrmFourcc::Yvu411
        | DrmFourcc::Yuv420
        | DrmFourcc::Yvu420
        | DrmFourcc::Yuv422
        | DrmFourcc::Yvu422
        | DrmFourcc::Yuv444
        | DrmFourcc::Yvu444 => three_plane!(R8, R8, R8),
        _ => dma_drm_format,
    }
}
