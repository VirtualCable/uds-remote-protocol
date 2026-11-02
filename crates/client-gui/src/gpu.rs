// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
// Authors: Adolfo Gómez, dkmaster at dkmon dot com

//! Shared wgpu context: surface/device/queue, the fullscreen BGRA texture
//! pipeline used by the session view, and the text brush (wgpu_text, same
//! setup as uds-client's `wgpu_render`).

use anyhow::{Context, Result};
use wgpu_text::glyph_brush::ab_glyph::FontRef;
use wgpu_text::glyph_brush::OwnedSection;
use wgpu_text::{BrushBuilder, TextBrush};
use winit::window::Window;

/// Embedded UI font (Inter, SIL OFL — see `fonts/OFL.txt`).
const FONT_DATA: &[u8] = include_bytes!("fonts/Inter-Regular.ttf");

/// A translucent rectangle drawn behind overlay text (stats panel), in
/// surface pixel coordinates with the origin at the top-left.
#[derive(Debug, Clone, Copy)]
pub struct Panel {
    /// Left edge, pixels.
    pub x: f32,
    /// Top edge, pixels.
    pub y: f32,
    /// Width, pixels.
    pub w: f32,
    /// Height, pixels.
    pub h: f32,
    /// RGBA color (alpha blends over the frame).
    pub color: [f32; 4],
}

/// Everything the GUI needs from the GPU.
pub struct Gpu {
    pub surface: wgpu::Surface<'static>,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub config: wgpu::SurfaceConfiguration,
    quad: QuadPipeline,
    fill: FillPipeline,
    brush: TextBrush<FontRef<'static>>,
}

impl Gpu {
    /// Initializes wgpu on `window` (which must outlive the surface; the
    /// caller keeps it in an `Arc`).
    pub fn new(window: &std::sync::Arc<Window>) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .context("creating the surface")?;
        let adapter = pollster_block(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        }))
        .context("no suitable GPU adapter")?;
        let (device, queue) = pollster_block(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("uds-rp-client"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
        }))
        .context("requesting the device")?;
        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("no default surface config")?;
        config.present_mode = wgpu::PresentMode::AutoNoVsync;
        surface.configure(&device, &config);
        let brush = BrushBuilder::using_font_bytes(FONT_DATA)
            .context("building the text brush")?
            .build(&device, size.width.max(1), size.height.max(1), config.format);
        let quad = QuadPipeline::new(&device, config.format);
        let fill = FillPipeline::new(&device, config.format);
        Ok(Self {
            surface,
            device,
            queue,
            config,
            quad,
            fill,
            brush,
        })
    }

    /// Reconfigures the surface after a resize.
    #[expect(clippy::cast_precision_loss, reason = "window sizes fit in f32")]
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.brush
            .resize_view(width as f32, height as f32, &self.queue);
    }

    /// One frame: `texture` is drawn fullscreen when present; translucent
    /// `panels` go over it and `sections` (text) on top. `clear` is the
    /// background color.
    pub fn render(
        &mut self,
        texture: Option<&wgpu::TextureView>,
        clear: wgpu::Color,
        sections: &[OwnedSection],
        panels: &[Panel],
    ) -> Result<()> {
        use wgpu::CurrentSurfaceTexture as Cst;
        let frame = match self.surface.get_current_texture() {
            Cst::Success(f) | Cst::Suboptimal(f) => f,
            Cst::Lost | Cst::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            Cst::Timeout | Cst::Occluded => return Ok(()),
            Cst::Validation => anyhow::bail!("surface validation error acquiring the frame"),
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("main"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                ..wgpu::RenderPassDescriptor::default()
            });
            if let Some(tex) = texture {
                self.quad.draw(&self.device, &mut rpass, tex);
            }
            for panel in panels {
                self.fill.draw(
                    &self.device,
                    &self.queue,
                    &mut rpass,
                    panel,
                    self.config.width,
                    self.config.height,
                );
            }
            if !sections.is_empty() {
                self.brush
                    .queue(&self.device, &self.queue, sections)
                    .context("queueing text")?;
                self.brush.draw(&mut rpass);
            }
        }
        self.queue.submit([encoder.finish()]);
        self.queue.present(frame);
        Ok(())
    }
}

/// `pollster::block_on` without the dependency: the futures here are ready
/// immediately on every supported backend.
fn pollster_block<F: std::future::Future>(future: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let std::task::Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
        std::thread::yield_now();
    }
}

// ---------------------------------------------------------------------------
// Fullscreen textured quad
// ---------------------------------------------------------------------------

const QUAD_SHADER: &str = r"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    // Fullscreen triangle.
    var out: VsOut;
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
";

struct QuadPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl QuadPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("quad"),
            source: wgpu::ShaderSource::Wgsl(QUAD_SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("quad"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("quad"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("quad"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("quad"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..wgpu::SamplerDescriptor::default()
        });
        Self {
            pipeline,
            layout,
            sampler,
        }
    }

    fn draw(
        &self,
        device: &wgpu::Device,
        rpass: &mut wgpu::RenderPass<'_>,
        texture: &wgpu::TextureView,
    ) {
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("quad"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(texture),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        rpass.set_pipeline(&self.pipeline);
        rpass.set_bind_group(0, &bind, &[]);
        rpass.draw(0..3, 0..1);
    }
}

// ---------------------------------------------------------------------------
// Translucent fill quad (overlay panels)
// ---------------------------------------------------------------------------

const FILL_SHADER: &str = r"
struct Uniforms {
    rect: vec4<f32>,   // x, y, w, h in pixels (origin top-left)
    screen: vec4<f32>, // w, h in pixels, unused, unused
    color: vec4<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // Two triangles covering the rect.
    var pos = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let px = u.rect.xy + pos[vi] * u.rect.zw;
    let ndc = vec2<f32>(
        px.x / u.screen.x * 2.0 - 1.0,
        1.0 - px.y / u.screen.y * 2.0,
    );
    return vec4<f32>(ndc, 0.0, 1.0);
}

@fragment
fn fs_main() -> @location(0) vec4<f32> {
    return u.color;
}
";

struct FillPipeline {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

impl FillPipeline {
    fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fill"),
            source: wgpu::ShaderSource::Wgsl(FILL_SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("fill"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fill"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fill"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        Self { pipeline, layout }
    }

    fn draw(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rpass: &mut wgpu::RenderPass<'_>,
        panel: &Panel,
        screen_w: u32,
        screen_h: u32,
    ) {
        #[repr(C)]
        #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
        struct Uniforms {
            rect: [f32; 4],
            screen: [f32; 4],
            color: [f32; 4],
        }
        #[expect(clippy::cast_precision_loss, reason = "window sizes fit in f32")]
        let uniforms = Uniforms {
            rect: [panel.x, panel.y, panel.w, panel.h],
            screen: [screen_w as f32, screen_h as f32, 0.0, 0.0],
            color: panel.color,
        };
        // Debug-overlay path only: a fresh tiny buffer per panel per frame
        // beats plumbing a persistent ring for a handful of rectangles.
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("fill"),
            size: u64::try_from(std::mem::size_of::<Uniforms>()).unwrap_or(64),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&buffer, 0, bytemuck::bytes_of(&uniforms));
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("fill"),
            layout: &self.layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            }],
        });
        rpass.set_pipeline(&self.pipeline);
        rpass.set_bind_group(0, &bind, &[]);
        rpass.draw(0..6, 0..1);
    }
}
