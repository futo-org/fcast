#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

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
}

impl Window {
    pub fn new(client: &Client, identifier: *const c_char, title: *const c_char) -> Result<Self> {
        let icon_filepath = CString::new("")?;
        unsafe {
            let create_window_seq = fl_create_full_screen_window(
                client.client,
                identifier,
                title,
                icon_filepath.as_ptr(),
            );
            if create_window_seq.value == 0 {
                return Err(anyhow!("Failed to create window"));
            }

            let create_window_rep =
                match fl_receive_reply_create_full_screen_window(client.client, create_window_seq)
                    .as_mut()
                {
                    Some(create_window_rep) => create_window_rep,
                    None => {
                        return Err(anyhow!("fl_receive_reply_create_full_screen_window failed"));
                    }
                };

            let window_id = create_window_rep.window_id;
            let width = create_window_rep.width;
            let height = create_window_rep.height;
            fl_free_reply_create_full_screen_window(create_window_rep);

            Ok(Self {
                window_id: window_id,
                width: width,
                height: height,
            })
        }
    }
}
