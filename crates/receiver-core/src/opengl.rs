use std::rc::Rc;

use anyhow::{Result, anyhow, ensure};
use glow::HasContext;

pub struct Texture {
    pub inner: glow::Texture,
    pub width: u32,
    pub height: u32,
    gl: Rc<glow::Context>,
}

impl Texture {
    pub fn new(
        gl: &Rc<glow::Context>,
        width: u32,
        height: u32,
        pixels: Option<&[u8]>,
    ) -> Result<Self> {
        unsafe {
            let texture = gl.create_texture().map_err(|e| anyhow!(e))?;

            gl.bind_texture(glow::TEXTURE_2D, Some(texture));

            macro_rules! tex_param_i {
                ($param:expr, $val:expr) => {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, $param, $val);
                };
            }

            tex_param_i!(glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            tex_param_i!(glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            tex_param_i!(glow::TEXTURE_MIN_FILTER, glow::LINEAR as i32);
            tex_param_i!(glow::TEXTURE_MAG_FILTER, glow::LINEAR as i32);

            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as _,
                width as _,
                height as _,
                0,
                glow::RGBA as _,
                glow::UNSIGNED_BYTE as _,
                glow::PixelUnpackData::Slice(pixels),
            );

            Ok(Self {
                gl: gl.clone(),
                inner: texture,
                width,
                height,
            })
        }
    }

    pub fn to_borrowed_slint_image(&self) -> slint::Image {
        unsafe {
            slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                self.inner.0,
                (self.width, self.height).into(),
            )
            .build()
        }
    }
}

impl Drop for Texture {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_texture(self.inner);
        }
    }
}

pub struct Framebuffer {
    pub fbo: glow::Framebuffer,
    gl: Rc<glow::Context>,
}

impl Framebuffer {
    pub fn new(gl: &Rc<glow::Context>) -> Result<Self> {
        Ok(Self {
            fbo: unsafe { gl.create_framebuffer().map_err(|e| anyhow!(e))? },
            gl: gl.clone(),
        })
    }

    pub fn set_texture_and_bind(&self, tex: &Texture) -> Result<()> {
        unsafe {
            self.gl
                .bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.fbo));

            self.gl.framebuffer_texture_2d(
                glow::DRAW_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex.inner),
                0,
            );

            ensure!(
                self.gl.check_framebuffer_status(glow::DRAW_FRAMEBUFFER)
                    == glow::FRAMEBUFFER_COMPLETE
            );
        }

        Ok(())
    }
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_framebuffer(self.fbo);
        }
    }
}

pub struct Program {
    gl: Rc<glow::Context>,
    pub prog: glow::Program,
}

impl Program {
    pub fn new(gl: &Rc<glow::Context>, vert_source: &str, frag_source: &str) -> Result<Self> {
        unsafe fn create_and_attach_shader(
            gl: &Rc<glow::Context>,
            program: glow::Program,
            typ: u32,
            source: &str,
        ) -> Result<glow::NativeShader> {
            unsafe {
                let shader = gl.create_shader(typ).map_err(|e| anyhow!(e))?;
                gl.shader_source(shader, source);
                gl.compile_shader(shader);
                ensure!(gl.get_shader_compile_status(shader));
                gl.attach_shader(program, shader);
                Ok(shader)
            }
        }

        unsafe {
            let program = gl.create_program().map_err(|e| anyhow!(e))?;

            let vert = create_and_attach_shader(gl, program, glow::VERTEX_SHADER, vert_source)?;
            let frag = create_and_attach_shader(gl, program, glow::FRAGMENT_SHADER, frag_source)?;

            gl.link_program(program);
            ensure!(gl.get_program_link_status(program));

            gl.detach_shader(program, vert);
            gl.delete_shader(vert);

            gl.detach_shader(program, frag);
            gl.delete_shader(frag);

            Ok(Self {
                gl: gl.clone(),
                prog: program,
            })
        }
    }

    fn uniform_location(&self, name: &str) -> Result<glow::NativeUniformLocation> {
        unsafe {
            self.gl
                .get_uniform_location(self.prog, name)
                .ok_or(anyhow!("couldn't get uniform {name}"))
        }
    }
}

impl Drop for Program {
    fn drop(&mut self) {
        unsafe {
            self.gl.delete_program(self.prog);
        }
    }
}

pub fn quad_vao(gl: &glow::Context) -> Result<(glow::VertexArray, glow::Buffer)> {
    unsafe {
        let vao = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
        let vbo = gl.create_buffer().map_err(|e| anyhow!(e))?;

        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));

        #[rustfmt::skip]
        let vertices: [f32; 8] = [
            -1.0,  1.0,
            -1.0, -1.0,
             1.0,  1.0,
             1.0, -1.0,
        ];
        let vertices_u8 = std::slice::from_raw_parts(
            vertices.as_ptr() as *const u8,
            vertices.len() * std::mem::size_of::<f32>(),
        );

        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, vertices_u8, glow::STATIC_DRAW);

        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
        gl.enable_vertex_attrib_array(0);

        gl.bind_buffer(glow::ARRAY_BUFFER, None);
        gl.bind_vertex_array(None);

        Ok((vao, vbo))
    }
}

struct BlurProgram {
    pub inner: Program,
    pub resolution: glow::NativeUniformLocation,
    pub input_texture: glow::NativeUniformLocation,
    pub direction: glow::NativeUniformLocation,
}

impl BlurProgram {
    pub fn new(gl: &Rc<glow::Context>) -> Result<Self> {
        let inner = Program::new(
            gl,
            include_str!("../shaders/quad_vert.glsl"),
            include_str!("../shaders/blur_frag.glsl"),
        )?;

        let resolution = inner.uniform_location("resolution")?;
        let input_texture = inner.uniform_location("inputTexture")?;
        let direction = inner.uniform_location("direction")?;

        Ok(Self {
            inner,
            resolution,
            input_texture,
            direction,
        })
    }
}

pub struct Renderer {
    pub gl: Rc<glow::Context>,
    blur: BlurProgram,
    pub blured_audio_cover: Option<Texture>,
}

impl Renderer {
    pub fn new(gl: glow::Context) -> Result<Self> {
        let gl = Rc::new(gl);
        let blur = BlurProgram::new(&gl)?;

        Ok(Self {
            gl,
            blur,
            blured_audio_cover: None,
        })
    }

    pub fn blur_rgba8_image(&self, img: &[u8], width: u32, height: u32) -> Result<Texture> {
        let mut input_texture = Texture::new(&self.gl, width, height, Some(img))?;
        let mut result_texture = Texture::new(&self.gl, width, height, None)?;
        let result_framebuffer = Framebuffer::new(&self.gl)?;

        unsafe {
            self.gl.use_program(Some(self.blur.inner.prog));
            self.gl
                .uniform_2_f32(Some(&self.blur.resolution), width as f32, height as f32);
            let (vao, vbo) = quad_vao(&self.gl)?;

            self.gl.bind_vertex_array(Some(vao));
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.uniform_1_i32(Some(&self.blur.input_texture), 0);

            let iters = 12;
            for i in 0..iters {
                let radius = (iters - i - 1) as f32 * 1.5;

                result_framebuffer.set_texture_and_bind(&result_texture)?;

                self.gl.viewport(0, 0, width as i32, height as i32);

                self.gl
                    .bind_texture(glow::TEXTURE_2D, Some(input_texture.inner));

                let (dir_x, dir_y) = match i % 2 == 0 {
                    true => (radius, 0.0),
                    false => (0.0, radius),
                };
                self.gl
                    .uniform_2_f32(Some(&self.blur.direction), dir_x, dir_y);

                self.gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);

                if i < iters - 1 {
                    std::mem::swap(&mut result_texture, &mut input_texture);
                }
            }

            self.gl.use_program(None);
            self.gl.bind_vertex_array(None);
            self.gl.delete_vertex_array(vao);
            self.gl.delete_buffer(vbo);
        }

        Ok(result_texture)
    }
}
