pub use fiatlux_sys::*;

use std::{
    ffi::{CString, c_char},
    ptr::null,
};

use anyhow::{Result, anyhow};

pub struct Client {
    pub client: *mut fl_Client,
}

impl Client {
    pub fn new() -> Result<Self> {
        let client = unsafe {
            fl_connect(null())
                .as_mut()
                .expect("Failed to connect to the fiatlux server")
        };
        Ok(Self { client: client })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        unsafe {
            fl_disconnect(self.client);
        }
    }
}

#[derive(Copy, Clone)]
pub struct ClientPtr(pub *mut fl_Client);

unsafe impl Send for ClientPtr {}
unsafe impl Sync for ClientPtr {}

pub struct GraphicsContext {
    pub gc: *mut fl_GraphicsContext,
}

impl GraphicsContext {
    pub fn new(client: &Client) -> Result<Self> {
        let gc = unsafe {
            match fl_graphics_context_init(client.client).as_mut() {
                Some(gc) => gc,
                None => return Err(anyhow!("fl_graphics_context_init failed")),
            }
        };
        Ok(Self { gc: gc })
    }
}

impl Drop for GraphicsContext {
    fn drop(&mut self) {
        unsafe {
            fl_graphics_context_deinit(self.gc);
        }
    }
}

pub struct Window {
    pub window_id: fl_protocol_WindowId,
    pub width: u32,
    pub height: u32,
    pub display_scale: f32,
}

impl Window {
    pub fn new(client: &Client, identifier: *const c_char, title: *const c_char) -> Result<Self> {
        let icon_filepath = CString::new("")?;
        unsafe {
            let create_window_seq = fl_create_window(
                client.client,
                fl_protocol_WindowType_fl_protocol_WindowType_fullscreen,
                identifier,
                title,
                icon_filepath.as_ptr(),
                true,
            );
            if create_window_seq.value == 0 {
                return Err(anyhow!("Failed to create window"));
            }

            let mut create_window_rep: fl_reply_CreateWindow = std::mem::zeroed();
            if !fl_receive_reply_create_window(client.client, create_window_seq, &mut create_window_rep) {
                return Err(anyhow!("fl_create_window failed"));
            }

            Ok(Self {
                window_id: create_window_rep.window_id,
                width: create_window_rep.width,
                height: create_window_rep.height,
                display_scale: create_window_rep.display_scale,
            })
        }
    }
}
